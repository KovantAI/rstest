//! `rstest try`: run the suite under plain pytest and under rstest (-n auto),
//! report whether outcomes are identical and the speedup. The 30-second
//! "should I switch?" proof.

use std::path::Path;

use anyhow::Result;

use super::{Outcomes, Phase};
use crate::reporting::sink::Sink;
use crate::scheduling::worker;

/// One timed child run (the pytest baseline or the rstest run), read back
/// from the JSON it wrote (recorder snapshot / `--report-json`).
struct TimedRun {
    /// Per-test outcomes; `None` when the run wrote no readable JSON.
    outcomes: Option<Outcomes>,
    /// Nodeids of the collectors that failed (the top-level `collect_errors`
    /// list both JSON shapes carry).
    collect_errors: Vec<String>,
    /// `meta.workers` from rstest's report-json (0 = single-worker mode);
    /// `None` for the pytest recorder, which has no such field.
    workers: Option<u64>,
    wall: f64,
    code: i32,
}

/// Run a command and read back what it recorded at `record_path`.
fn time_run(mut cmd: std::process::Command, record_path: &Path) -> TimedRun {
    let t0 = std::time::Instant::now();
    let code = cmd.status().ok().and_then(|s| s.code()).unwrap_or(-1);
    let wall = t0.elapsed().as_secs_f64();
    let doc: Option<serde_json::Value> = std::fs::read_to_string(record_path)
        .ok()
        .and_then(|txt| serde_json::from_str(&txt).ok());
    TimedRun {
        outcomes: doc.as_ref().and_then(|d| super::parse_outcomes(d, false)),
        collect_errors: doc.as_ref().map(collect_errors).unwrap_or_default(),
        workers: doc
            .as_ref()
            .and_then(|d| d.get("meta")?.get("workers")?.as_u64()),
        wall,
        code,
    }
}

/// The `collect_errors` list of a recorder / report-json document.
fn collect_errors(doc: &serde_json::Value) -> Vec<String> {
    doc.get("collect_errors")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .map(|e| e.as_str().unwrap_or("<unknown>").to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// Why a run can't be compared, if it can't: a parity verdict over a suite
/// that didn't fully collect, or collected nothing, would bless tests nobody
/// ran. `side` names the runner (`pytest` / `rstest`) for the message.
fn not_comparable(
    side: &str,
    outcomes: &Outcomes,
    collect_errors: &[String],
    code: i32,
) -> Option<String> {
    if !collect_errors.is_empty() {
        let n = collect_errors.len();
        let shown = collect_errors
            .iter()
            .take(3)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        let more = if n > 3 {
            format!(", +{} more", n - 3)
        } else {
            String::new()
        };
        return Some(format!(
            "{side} hit {n} collection error{} ({shown}{more})",
            if n == 1 { "" } else { "s" }
        ));
    }
    if outcomes.is_empty() {
        return Some(format!(
            "{side} ran no tests (none collected, or all deselected; exit {code})"
        ));
    }
    None
}

/// The speed line's worker clause: what `-n auto` resolved to. The
/// report-json records single-worker mode as 0 workers, which is `-n 1`.
fn workers_clause(workers: Option<u64>) -> String {
    match workers {
        Some(n) => format!("at -n auto = -n {}", n.max(1)),
        None => "at -n auto".to_string(),
    }
}

/// The "already red" note for a pytest baseline that exited non-zero. Counts
/// failing tests plus collection errors and never claims "0 failing": with
/// neither, it quotes the exit code instead.
fn red_note(failing: usize, collect_errors: usize, code: i32) -> String {
    let errs = |c: usize| format!("{c} collection error{}", if c == 1 { "" } else { "s" });
    let what = match (failing, collect_errors) {
        (0, 0) => format!("exit {code}"),
        (f, 0) => format!("{f} failing"),
        (0, c) => errs(c),
        (f, c) => format!("{f} failing, {}", errs(c)),
    };
    format!(
        "  note: your pytest run was already red ({what}). That's pre-existing, \
         not caused by rstest."
    )
}

/// Print the "could not compare" verdict and return try's "couldn't run"
/// exit code (2, as for a pytest/rstest run that produced nothing).
fn report_not_comparable(sink: &mut Sink, reason: &str) -> i32 {
    sink.out_line("\n================= rstest try =================");
    sink.out_line(&format!("  ✗ could not compare: {reason}"));
    sink.out_line("================================================");
    sink.out_line(
        "  → no verdict: parity needs a suite that collects cleanly and runs at least\n\
         \x20   one test. Fix that (check `python -m pytest -q`), then re-run `rstest try`.",
    );
    2
}

/// Estimate CI runs/day from git history: commits in the last 30 days ÷ 30
/// (CI typically runs once per push ≈ per commit). Returns (per_day, count) or
/// None outside a git repo / with no recent history.
fn commits_per_day() -> Option<(f64, u64)> {
    let out = std::process::Command::new("git")
        .args(["rev-list", "--count", "--since=30.days.ago", "HEAD"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let n: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    (n > 0).then_some((n as f64 / 30.0, n))
}

fn fmt_secs(s: f64) -> String {
    if s >= 60.0 {
        format!("{}m{:02.0}s", (s / 60.0).floor(), s % 60.0)
    } else {
        format!("{s:.1}s")
    }
}

/// Outcome parity between the pytest and rstest runs: how many tests each side
/// has that the other doesn't, how many shared tests ended in a different phase,
/// and whether the two sets are byte-for-byte equivalent.
struct Parity {
    total: usize,
    diffs: usize,
    only_py: usize,
    only_rs: usize,
    identical: bool,
}

/// Compare the two runs' per-test outcomes. Pure: keys present in only one side
/// count as divergence, and shared keys diverge when their phases differ.
fn compute_parity(py: &Outcomes, rs: &Outcomes) -> Parity {
    let pk: std::collections::BTreeSet<&str> = py.keys().map(String::as_str).collect();
    let rk: std::collections::BTreeSet<&str> = rs.keys().map(String::as_str).collect();
    let only_py = pk.difference(&rk).count();
    let only_rs = rk.difference(&pk).count();
    let diffs = pk
        .intersection(&rk)
        .filter(|id| py[**id].phase != rs[**id].phase)
        .count();
    Parity {
        total: pk.union(&rk).count(),
        diffs,
        only_py,
        only_rs,
        identical: only_py == 0 && only_rs == 0 && diffs == 0,
    }
}

/// `rstest try`: run the suite under plain pytest and under rstest (-n auto),
/// report whether outcomes are identical and the speedup. The 30-second
/// "should I switch?" proof.
pub fn run_try(python: &Path, args: &[String], sink: &mut Sink) -> Result<i32> {
    let tmpdir = std::env::temp_dir();
    let pid = std::process::id();
    let py_json = tmpdir.join(format!("rstest-try-pytest-{pid}.json"));
    let rs_json = tmpdir.join(format!("rstest-try-rstest-{pid}.json"));

    sink.warn("rstest try: running your suite under pytest…");
    let mut py = std::process::Command::new(python);
    py.args(["-m", "pytest", "-p", "rstest_worker.recorder", "-q"])
        .args(args)
        .env("RSTEST_RECORD", &py_json)
        .env("PYTHONPATH", worker::worker_pythonpath())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    worker::scrub_secrets(&mut py);
    let py_run = time_run(py, &py_json);
    let _ = std::fs::remove_file(&py_json);
    let (py_wall, py_code) = (py_run.wall, py_run.code);

    let Some(py_out) = py_run.outcomes.as_ref() else {
        sink.out_line(
            "rstest try: couldn't run pytest (is it installed and your suite collectable?).\n\
             Try `python -m pytest -q` yourself, then re-run `rstest try`.",
        );
        return Ok(2);
    };
    // A baseline with a collection error or no tests has nothing to compare
    // against: empty == empty is not parity. Stop before the rstest run.
    if let Some(reason) = not_comparable("pytest", py_out, &py_run.collect_errors, py_code) {
        return Ok(report_not_comparable(sink, &reason));
    }

    sink.warn("rstest try: running it under rstest (-n auto)…");
    let exe = std::env::current_exe()?;
    let mut rs = std::process::Command::new(exe);
    // Same interpreter as the pytest baseline: without it the child re-runs
    // discovery and, outside an activated venv, may find none.
    rs.arg("--python")
        .arg(python)
        .arg("-n")
        .arg("auto")
        .args(args)
        .arg("--report-json")
        .arg(&rs_json)
        .args(["-q", "--output", "dots"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let rs_run = time_run(rs, &rs_json);
    let _ = std::fs::remove_file(&rs_json);
    let rs_wall = rs_run.wall;

    let Some(rs_out) = rs_run.outcomes.as_ref() else {
        sink.out_line(
            "rstest try: rstest produced no run (it may have refused to dispatch — \
             often an unstable parametrize id). Run `rstest migrate-check` to see why.",
        );
        return Ok(2);
    };
    if let Some(reason) = not_comparable("rstest", rs_out, &rs_run.collect_errors, rs_run.code) {
        return Ok(report_not_comparable(sink, &reason));
    }

    // ---- parity ----
    let parity = compute_parity(py_out, rs_out);
    let Parity {
        total,
        diffs,
        only_py,
        only_rs,
        identical,
    } = parity;

    sink.out_line("\n================= rstest try =================");
    if identical {
        sink.out_line(&format!(
            "  ✓ parity:  {total} tests — identical outcomes to pytest"
        ));
    } else {
        sink.out_line(&format!(
            "  ⚠ parity:  {} of {total} tests differ ({diffs} different outcome, \
             {only_py} only in pytest, {only_rs} only in rstest)",
            diffs + only_py + only_rs
        ));
    }

    // ---- speed ----
    let speedup = if rs_wall > 0.0 {
        py_wall / rs_wall
    } else {
        0.0
    };
    sink.out_line(&format!(
        "  ⚡ speed:   pytest {}  →  rstest {}   ({speedup:.1}× {})",
        fmt_secs(py_wall),
        fmt_secs(rs_wall),
        workers_clause(rs_run.workers)
    ));
    let saved = (py_wall - rs_wall).max(0.0);
    if saved >= 1.0 {
        match commits_per_day() {
            // Project over the repo's actual recent activity (commits ≈ CI
            // runs). Monthly total avoids rounding a low cadence to "0/day".
            Some((_, n)) => sink.out_line(&format!(
                "  💸 saves   {} per run — ≈ {} over your last 30 days ({n} commits ≈ CI runs)",
                fmt_secs(saved),
                fmt_secs(saved * n as f64),
            )),
            None => sink.out_line(&format!("  💸 saves   {} per run", fmt_secs(saved))),
        }
    }
    sink.out_line("================================================");

    if py_code != 0 {
        sink.out_line(&red_note(
            py_out.values().filter(|r| r.phase == Phase::Fail).count(),
            py_run.collect_errors.len(),
            py_code,
        ));
    }
    if identical {
        sink.out_line(
            "  → drop-in ready: `rstest` is `pytest`, in parallel. Switch with confidence.",
        );
    } else {
        sink.out_line(
            "  → some tests differ. Could be a pytest-version difference or a real parallel-only\n\
             \x20   issue — run `rstest migrate-check` to classify each and get the fix.",
        );
    }
    Ok(if identical { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::{
        collect_errors, commits_per_day, compute_parity, fmt_secs, not_comparable, red_note,
        workers_clause,
    };
    use crate::migrate::{Outcomes, Phase, Rec};

    #[test]
    fn not_comparable_on_a_collection_error_names_it() {
        let ok = outcomes(&[("a", Phase::Pass)]);
        let r = not_comparable("pytest", &ok, &["test_bad.py".into()], 2).unwrap();
        assert!(r.contains("1 collection error (test_bad.py)"), "{r}");
        // Even with no tests recorded, the collection error is the reason.
        let r = not_comparable("rstest", &Outcomes::new(), &["x.py".into()], 2).unwrap();
        assert!(r.starts_with("rstest hit 1 collection error"), "{r}");
        let many: Vec<String> = (0..5).map(|i| format!("t{i}.py")).collect();
        let r = not_comparable("pytest", &ok, &many, 2).unwrap();
        assert!(
            r.contains("5 collection errors (t0.py, t1.py, t2.py, +2 more)"),
            "{r}"
        );
    }

    #[test]
    fn not_comparable_on_zero_tests_and_fine_otherwise() {
        let r = not_comparable("pytest", &Outcomes::new(), &[], 5).unwrap();
        assert!(r.contains("ran no tests") && r.contains("exit 5"), "{r}");
        let ok = outcomes(&[("a", Phase::Fail)]);
        assert_eq!(not_comparable("pytest", &ok, &[], 1), None);
    }

    #[test]
    fn collect_errors_reads_the_top_level_list() {
        let doc = serde_json::json!({"collect_errors": ["a.py", "b.py"], "tests": {}});
        assert_eq!(collect_errors(&doc), vec!["a.py", "b.py"]);
        assert!(collect_errors(&serde_json::json!({"tests": {}})).is_empty());
    }

    #[test]
    fn workers_clause_names_the_resolved_count() {
        assert_eq!(workers_clause(Some(4)), "at -n auto = -n 4");
        // Single-worker mode records 0 workers.
        assert_eq!(workers_clause(Some(0)), "at -n auto = -n 1");
        assert_eq!(workers_clause(None), "at -n auto");
    }

    #[test]
    fn red_note_never_says_zero_failing() {
        assert!(red_note(2, 0, 1).contains("(2 failing)"));
        assert!(red_note(0, 1, 2).contains("(1 collection error)"));
        assert!(red_note(1, 2, 2).contains("(1 failing, 2 collection errors)"));
        let n = red_note(0, 0, 3);
        assert!(n.contains("(exit 3)") && !n.contains("0 failing"), "{n}");
    }

    fn outcomes(entries: &[(&str, Phase)]) -> Outcomes {
        entries
            .iter()
            .map(|(id, phase)| {
                (
                    id.to_string(),
                    Rec {
                        phase: *phase,
                        wall: 0.0,
                        cpu: None,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn compute_parity_identical_when_same_ids_and_phases() {
        let py = outcomes(&[("a", Phase::Pass), ("b", Phase::Fail)]);
        let rs = outcomes(&[("a", Phase::Pass), ("b", Phase::Fail)]);
        let p = compute_parity(&py, &rs);
        assert!(p.identical);
        assert_eq!(p.total, 2);
        assert_eq!(p.diffs, 0);
        assert_eq!(p.only_py, 0);
        assert_eq!(p.only_rs, 0);
    }

    #[test]
    fn compute_parity_flags_phase_divergence() {
        // Same id, opposite phase -> one differing outcome, not identical.
        let py = outcomes(&[("a", Phase::Pass)]);
        let rs = outcomes(&[("a", Phase::Fail)]);
        let p = compute_parity(&py, &rs);
        assert!(!p.identical);
        assert_eq!(p.diffs, 1);
        assert_eq!(p.total, 1);
    }

    #[test]
    fn compute_parity_counts_ids_unique_to_each_side() {
        // 'a' shared+agreeing, 'b' only pytest, 'c' only rstest.
        let py = outcomes(&[("a", Phase::Pass), ("b", Phase::Pass)]);
        let rs = outcomes(&[("a", Phase::Pass), ("c", Phase::Pass)]);
        let p = compute_parity(&py, &rs);
        assert!(!p.identical);
        assert_eq!(p.diffs, 0, "the shared id agrees");
        assert_eq!(p.only_py, 1);
        assert_eq!(p.only_rs, 1);
        assert_eq!(p.total, 3, "union of a, b, c");
    }

    #[test]
    fn commits_per_day_is_positive_or_none_and_never_panics() {
        // Runs `git rev-list` over the ambient repo. The count isn't pinned, but
        // the contract holds: Some((per_day, n)) with both > 0, or None. It must
        // parse cleanly and never panic.
        if let Some((per_day, n)) = commits_per_day() {
            assert!(n > 0, "Some is only returned when there are commits");
            assert!(per_day > 0.0, "per-day rate derives from n > 0");
            assert!((per_day - n as f64 / 30.0).abs() < 1e-9);
        }
    }

    #[test]
    fn fmt_secs_sub_minute_is_one_decimal_seconds() {
        assert_eq!(fmt_secs(0.0), "0.0s");
        assert_eq!(fmt_secs(5.2), "5.2s");
        assert_eq!(fmt_secs(59.9), "59.9s");
    }

    #[test]
    fn fmt_secs_minute_and_over_is_zero_padded_minutes_seconds() {
        // Exactly a minute -> the seconds field is zero-padded to two digits.
        assert_eq!(fmt_secs(60.0), "1m00s");
        assert_eq!(fmt_secs(90.0), "1m30s");
        assert_eq!(fmt_secs(125.0), "2m05s");
    }
}
