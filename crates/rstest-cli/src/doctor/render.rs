//! Doctor output surfaces: the terminal report, the GitHub-flavored markdown
//! job summary, and the CI summary sinks (GitHub step summary, Buildkite
//! annotation). All read-only over a `DoctorReport`.

use super::{DoctorReport, FixtureEntry};

/// The doctor analysis as GitHub-flavored markdown, shaped for a job
/// summary: same signals as the terminal report, tables instead of
/// aligned columns.
pub fn render_markdown(r: &DoctorReport) -> String {
    use std::fmt::Write;

    let mut md = String::from("## rstest doctor\n\n");
    if r.tests == 0 {
        md.push_str("No timing data collected.\n");
        return md;
    }
    let _ = writeln!(
        md,
        "**{} tests** — test time {:.1}s (wall {:.1}s, {} workers)\n",
        r.tests, r.test_time_seconds, r.wall_seconds, r.workers
    );

    if let Some(w) = &r.wait_bound {
        let _ = writeln!(
            md,
            "**Wait-bound:** {:.0}% of test time ({:.1}s) is waiting, not \
             computing (sleeps / IO / timeouts).\n",
            w.wait_pct, w.wait_seconds
        );
        if !w.tests.is_empty() {
            md.push_str("| Waiting | Duration | Test |\n|---:|---:|---|\n");
            for t in w.tests.iter().take(8) {
                let _ = writeln!(
                    md,
                    "| {:.2}s | {:.2}s | `{}` |",
                    t.wait, t.duration, t.nodeid
                );
            }
            if w.tests.len() > 8 {
                let _ = writeln!(md, "\n... and {} more", w.tests.len() - 8);
            }
            md.push('\n');
        }
    }

    if let Some(p) = &r.parallel_floor {
        let _ = writeln!(
            md,
            "**Parallel floor:** the longest test ({:.1}s) exceeds the ideal \
             per-worker share ({:.1}s at `-n {}`); no worker count can finish \
             faster than its longest test.\n",
            p.longest_seconds, p.ideal_share_seconds, r.workers
        );
        if !p.gate_tests.is_empty() {
            md.push_str("| Duration | Gate test |\n|---:|---|\n");
            for t in p.gate_tests.iter().take(5) {
                let _ = writeln!(md, "| {:.2}s | `{}` |", t.duration, t.nodeid);
            }
            md.push('\n');
        }
    }

    if let Some(pe) = &r.parallel_efficiency {
        let _ = writeln!(
            md,
            "**Parallel efficiency:** {:.1}× realized of {}× possible ({:.0}%). \
             Long pole {:.1}s; {:.0}% load imbalance between busiest and idlest \
             worker.\n",
            pe.realized_speedup,
            pe.ideal_speedup,
            pe.efficiency_pct,
            pe.long_pole_seconds,
            pe.imbalance_pct
        );
        if pe.efficiency_pct > 105.0 {
            md.push_str("> Over 100% means tests overlap beyond core count (wait-bound).\n\n");
        }
        if !pe.workers_busy.is_empty() {
            md.push_str("| Worker | Busy | Tests |\n|---|---:|---:|\n");
            for w in pe.workers_busy.iter().take(8) {
                let _ = writeln!(
                    md,
                    "| `{}` | {:.2}s | {} |",
                    w.worker, w.busy_seconds, w.tests
                );
            }
            md.push('\n');
        }
    }

    let interesting: Vec<&FixtureEntry> = r
        .fixtures
        .iter()
        .filter(|f| f.total_seconds >= 0.5)
        .take(8)
        .collect();
    if !interesting.is_empty() {
        md.push_str("### Fixture hotspots (setup time across all workers)\n\n");
        md.push_str("| Fixture | Scope | Runs | Total | |\n|---|---|---:|---:|---|\n");
        for f in interesting {
            let advice = if f.scope == "function" && f.count >= 20 && f.total_seconds >= 1.0 {
                "ran many times; widen scope if value is reusable"
            } else if f.scope == "session" && f.count > 1 {
                "session fixture ran once per worker; must be safe to duplicate"
            } else {
                ""
            };
            let _ = writeln!(
                md,
                "| `{}` | {} | {} | {:.1}s | {advice} |",
                f.name, f.scope, f.count, f.total_seconds
            );
        }
        md.push('\n');
    }

    if !r.slowest_files.is_empty() {
        md.push_str("### Slowest files\n\n| File | Time | Share |\n|---|---:|---:|\n");
        for f in r.slowest_files.iter().take(5) {
            let _ = writeln!(
                md,
                "| `{}` | {:.2}s | {:.0}% |",
                f.file, f.total_seconds, f.pct
            );
        }
    }
    if !r.leaks.is_empty() {
        md.push_str("### Resource leaks\n\n> Net threads/fds still open after teardown.\n\n");
        md.push_str("| Leaked | Test |\n|---|---|\n");
        for l in r.leaks.iter().take(10) {
            let _ = writeln!(md, "| {} | `{}` |", leak_delta(l), l.nodeid);
        }
    }
    md
}

pub fn write_markdown(path: &std::path::Path, report: &DoctorReport) -> anyhow::Result<()> {
    std::fs::write(path, render_markdown(report))?;
    Ok(())
}

/// Publish the markdown report to the CI's job-summary surface, if any:
/// GitHub Actions appends to `$GITHUB_STEP_SUMMARY`, Buildkite pipes to
/// `buildkite-agent annotate`. Others: use `--doctor-md` as an artifact.
pub fn append_ci_summary(report: &DoctorReport) -> anyhow::Result<()> {
    // GitHub Actions: append to the step-summary file (hard error on write
    // failure - the path came from the runner, so a failure is real).
    if let Some(path) = std::env::var("GITHUB_STEP_SUMMARY")
        .ok()
        .filter(|p| !p.is_empty())
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)?;
        f.write_all(render_markdown(report).as_bytes())?;
        return Ok(());
    }
    // Buildkite: pipe the markdown to the agent as an info annotation.
    // Best-effort - a missing/failing agent must not fail the test run
    // (the annotation is cosmetic, unlike GitHub's guaranteed file path).
    if std::env::var("BUILDKITE")
        .ok()
        .filter(|v| !v.is_empty())
        .is_some()
    {
        buildkite_annotate(&render_markdown(report));
    }
    Ok(())
}

/// Feed markdown to `buildkite-agent annotate` over stdin. Swallows all
/// errors (logging to stderr) - see `append_ci_summary`.
fn buildkite_annotate(md: &str) {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let child = Command::new("buildkite-agent")
        .args(["annotate", "--style", "info", "--context", "rstest-doctor"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            eprintln!("rstest: skipping Buildkite annotation (buildkite-agent: {e})");
            return;
        }
    };
    if let Some(stdin) = child.stdin.take() {
        let mut stdin = stdin;
        let _ = stdin.write_all(md.as_bytes());
    }
    if let Err(e) = child.wait() {
        eprintln!("rstest: buildkite-agent annotate failed: {e}");
    }
}

pub fn render(r: &DoctorReport) {
    if r.tests == 0 {
        println!("\n== rstest doctor: no timing data collected ==");
        return;
    }
    println!("\n================== rstest doctor ==================");
    println!(
        "{} tests, {:.1}s test time (wall {:.1}s, {} workers)",
        r.tests, r.test_time_seconds, r.wall_seconds, r.workers
    );

    if let Some(w) = &r.wait_bound {
        println!(
            "\nWAIT-BOUND: {:.0}% of test time ({:.1}s) is waiting, \
             not computing (sleeps / IO / timeouts).",
            w.wait_pct, w.wait_seconds
        );
        for t in w.tests.iter().take(8) {
            println!(
                "  {:7.2}s waiting of {:7.2}s  {}",
                t.wait, t.duration, t.nodeid
            );
        }
        if w.tests.len() > 8 {
            println!("  ... and {} more", w.tests.len() - 8);
        }
    }

    if let Some(p) = &r.parallel_floor {
        println!(
            "\nPARALLEL FLOOR: the longest test ({:.1}s) exceeds the ideal \
             per-worker share ({:.1}s at -n {});\nno worker count can finish \
             faster than its longest test. Gate tests:",
            p.longest_seconds, p.ideal_share_seconds, r.workers
        );
        for t in p.gate_tests.iter().take(5) {
            println!("  {:7.2}s  {}", t.duration, t.nodeid);
        }
    }

    if let Some(pe) = &r.parallel_efficiency {
        println!(
            "\nPARALLEL EFFICIENCY: {:.1}x realized of {}x possible ({:.0}%).",
            pe.realized_speedup, pe.ideal_speedup, pe.efficiency_pct
        );
        if pe.efficiency_pct > 105.0 {
            println!(
                "  over 100%: tests overlap beyond core count \
                 (wait-bound; see WAIT-BOUND above)."
            );
        }
        println!(
            "  long pole: {:.1}s (no worker count finishes faster)",
            pe.long_pole_seconds
        );
        println!("  worker load (busy time):");
        for w in pe.workers_busy.iter().take(8) {
            println!(
                "    {:<8} {:7.2}s ({} tests)",
                w.worker, w.busy_seconds, w.tests
            );
        }
        if pe.workers_busy.len() > 8 {
            println!("    ... and {} more", pe.workers_busy.len() - 8);
        }
        println!(
            "  imbalance: {:.0}% between busiest and idlest worker",
            pe.imbalance_pct
        );
    }

    let interesting: Vec<&FixtureEntry> = r
        .fixtures
        .iter()
        .filter(|f| f.total_seconds >= 0.5)
        .take(8)
        .collect();
    if !interesting.is_empty() {
        println!("\nFIXTURE HOTSPOTS (setup time across all workers):");
        for f in interesting {
            let advice = if f.scope == "function" && f.count >= 20 && f.total_seconds >= 1.0 {
                "  <- ran many times; widen scope if value is reusable"
            } else if f.scope == "session" && f.count > 1 {
                "  <- session fixture ran once PER WORKER; must be safe to duplicate (DBs, servers, ports)"
            } else {
                ""
            };
            println!(
                "  {:7.2}s {:6}x  scope={:<8} {}{advice}",
                f.total_seconds, f.count, f.scope, f.name
            );
        }
    }

    println!("\nSLOWEST FILES:");
    for f in r.slowest_files.iter().take(5) {
        println!("  {:7.2}s ({:4.1}%)  {}", f.total_seconds, f.pct, f.file);
    }

    if !r.leaks.is_empty() {
        println!("\nRESOURCE LEAKS (net threads/fds still open after teardown):");
        for l in r.leaks.iter().take(10) {
            println!("  {}  {}", leak_delta(l), l.nodeid);
        }
        if r.leaks.len() > 10 {
            println!("  ... and {} more", r.leaks.len() - 10);
        }
        println!(
            "  a test opened a thread/fd it never released; leaked state can flake \
             later tests (reset it, or close in teardown)."
        );
    }
    println!("===================================================");
}

/// `+3 threads`, `+5 fds`, or `+3 threads +5 fds` for a leak entry.
/// Shared by the doctor report and the `--fail-on-leak` gate.
pub(crate) fn leak_delta(l: &super::Leak) -> String {
    let mut parts = Vec::new();
    if l.threads > 0 {
        parts.push(format!("+{} thread{}", l.threads, plural(l.threads)));
    }
    if l.fds > 0 {
        parts.push(format!("+{} fd{}", l.fds, plural(l.fds)));
    }
    parts.join(" ")
}

fn plural(n: i64) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::report;
    use super::*;

    #[test]
    fn markdown_renders_all_sections() {
        let md = render_markdown(&report(12));
        assert!(md.starts_with("## rstest doctor\n"));
        assert!(md.contains("**12 tests** — test time 30.0s (wall 9.0s, 4 workers)"));
        assert!(md.contains("**Wait-bound:** 80% of test time (24.0s)"));
        assert!(md.contains("| 5.00s | 5.10s | `tests/test_a.py::test_sleepy` |"));
        assert!(md.contains("**Parallel floor:**"));
        assert!(md.contains("| 8.40s | `tests/test_a.py::test_long` |"));
        assert!(md.contains("**Parallel efficiency:** 3.3× realized of 4× possible (82%)"));
        assert!(md.contains("| `gw0` | 16.00s | 6 |"));
        assert!(md.contains("### Fixture hotspots"));
        assert!(md.contains("| `db` | session | 4 | 6.1s | session fixture ran once per worker"));
        assert!(md.contains("### Slowest files"));
        assert!(md.contains("| `tests/test_a.py` | 20.00s | 67% |"));
    }

    #[test]
    fn markdown_empty_run() {
        let md = render_markdown(&report(0));
        assert!(md.contains("No timing data collected."));
        assert!(!md.contains("Wait-bound"));
    }

    // Terminal `render()` mirrors `render_markdown()` but prints to stdout, so
    // there's nothing to assert on — these drive every branch (the harness
    // captures stdout) to prove the printing paths don't panic and are covered.
    #[test]
    fn render_terminal_populated_and_empty_dont_panic() {
        render(&report(12)); // full report: every section printed
        render(&report(0)); // no timing data: early "no timing" line
    }

    #[test]
    fn render_truncates_long_lists_after_eight() {
        use super::super::{FileEntry, GateTest, WaitTest, WorkerLoad};
        let mut r = report(12);
        // Push each capped list past 8 so the "... and N more" tails fire in
        // BOTH the terminal and markdown renderers.
        if let Some(wb) = r.wait_bound.as_mut() {
            for i in 0..9 {
                wb.tests.push(WaitTest {
                    nodeid: format!("tests/test_a.py::w{i}"),
                    duration: 1.0,
                    wait: 0.5,
                });
            }
        }
        if let Some(pf) = r.parallel_floor.as_mut() {
            for i in 0..9 {
                pf.gate_tests.push(GateTest {
                    nodeid: format!("tests/test_a.py::g{i}"),
                    duration: 1.0,
                });
            }
        }
        if let Some(pe) = r.parallel_efficiency.as_mut() {
            for i in 0..9 {
                pe.workers_busy.push(WorkerLoad {
                    worker: format!("gw{i}"),
                    busy_seconds: 1.0,
                    tests: 1,
                });
            }
        }
        for i in 0..9 {
            r.slowest_files.push(FileEntry {
                file: format!("tests/test_{i}.py"),
                total_seconds: 1.0,
                pct: 1.0,
            });
        }

        render(&r);
        let md = render_markdown(&r);
        assert!(md.contains("... and")); // truncation tail rendered
    }

    #[test]
    fn leak_delta_pluralizes_and_combines() {
        use super::super::Leak;
        let d = |threads, fds| {
            leak_delta(&Leak {
                nodeid: "t".into(),
                threads,
                fds,
            })
        };
        assert_eq!(d(1, 0), "+1 thread"); // singular, threads only
        assert_eq!(d(3, 0), "+3 threads"); // plural
        assert_eq!(d(0, 1), "+1 fd"); // singular, fds only
        assert_eq!(d(0, 5), "+5 fds"); // plural
        assert_eq!(d(3, 2), "+3 threads +2 fds"); // both combined
        assert_eq!(d(0, 0), ""); // nothing positive
    }

    #[test]
    fn leaks_render_in_terminal_and_markdown() {
        use super::super::Leak;
        let mut r = report(12);
        r.leaks = vec![
            Leak {
                nodeid: "tests/test_pool.py::test_executor".into(),
                threads: 3,
                fds: 0,
            },
            Leak {
                nodeid: "tests/test_io.py::test_reader".into(),
                threads: 0,
                fds: 5,
            },
        ];
        render(&r); // exercises the terminal RESOURCE LEAKS branch
        let md = render_markdown(&r);
        assert!(md.contains("### Resource leaks"));
        assert!(md.contains("| +3 threads | `tests/test_pool.py::test_executor` |"));
        assert!(md.contains("| +5 fds | `tests/test_io.py::test_reader` |"));
    }
}
