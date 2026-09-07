//! Monorepo driver: sequential/parallel session groups, one child `rstest`
//! process per subproject, results merged into a single summary and report.

use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;

use super::gates::build_run_meta;
use super::{head_to_none, parse_numprocesses, resolve_changed_base};
use crate::cli::{needs_passthrough_io, Cli};
use crate::scheduling::pool;
use crate::{doctor, mono, select};

/// Sequential session groups, one per subproject (monorepo P0).
pub(super) fn execute_monorepo(
    cli: &Cli,
    args: &[String],
    root: &std::path::Path,
    projects: Vec<PathBuf>,
    run_uid: &str,
) -> Result<i32> {
    if needs_passthrough_io(args) {
        anyhow::bail!(
            "--pdb/-s/--co need a single pytest session; run inside one project \
             of this monorepo (for --collect-only --report-json discovery, run \
             it once per project)"
        );
    }
    if cli.watch {
        anyhow::bail!("--watch at a monorepo root is not supported yet; run inside a project");
    }
    // Validate --doctor-fail-on once here so a malformed condition fails fast
    // at the root, not as N separate child aborts (children re-validate too).
    doctor::parse_conditions(&cli.doctor_fail_on)?;
    // The monorepo orchestrator prints per-project banners and a summary
    // around captured child output, so a clean NDJSON stream isn't possible.
    // The merged --report-json document is the machine-readable surface.
    if cli.output.as_deref() == Some("json") {
        anyhow::bail!(
            "--output json streams live per-session results and can't be merged \
             across a monorepo's projects; use --report-json <path> for one merged \
             machine-readable document, or run --output json inside a single project"
        );
    }
    // Same problem for TAP: each child would emit its own version header,
    // numbering, and plan; concatenated, that is not one valid stream.
    if cli.output.as_deref() == Some("tap") {
        anyhow::bail!(
            "--output tap can't be merged across a monorepo's projects; use \
             --junitxml for per-project machine-readable results, or run \
             --output tap inside a single project"
        );
    }
    let rels: Vec<String> = projects
        .iter()
        .map(|p| {
            p.strip_prefix(root)
                .unwrap_or(p)
                .to_string_lossy()
                // Stable, OS-independent project keys: forward slashes on
                // Windows too, so summary/meta/merged-report keys match the
                // `libs/b` form the gate and report contract expect.
                .replace('\\', "/")
        })
        .collect();
    // Worker budget: the user's -n (or auto = cores), split across projects
    // by their last-known suite time (duration caches). Each project runs as
    // a CHILD rstest process (cwd-isolated, output captured, printed whole).
    let budget = parse_numprocesses(
        &cli.numprocesses
            .clone()
            .unwrap_or_else(|| "auto".to_string()),
    )
    .unwrap_or(4)
    .max(1);
    // --changed at a monorepo root: classify projects ONCE against the
    // repo-wide changed set. Directly-changed projects keep --changed;
    // dependents run their FULL suite; the rest are skipped.
    let mono_changed = resolve_changed_base(cli)?;
    let impacts: Option<Vec<mono::ChangeImpact>> = match &mono_changed {
        Some(rev) => {
            let rev = head_to_none(rev);
            let changed = select::changed_files_from_git(rev)?;
            let impacts = mono::classify_changes(root, &projects, &changed, cli.changed_strict);
            let skipped = impacts
                .iter()
                .filter(|i| **i == mono::ChangeImpact::Unaffected)
                .count();
            eprintln!(
                "rstest: --changed: {} changed file(s) -> {} of {} projects affected",
                changed.len(),
                projects.len() - skipped,
                projects.len()
            );
            Some(impacts)
        }
        None => None,
    };
    let costs: Vec<Option<f64>> = projects.iter().map(|p| mono::project_cost(p)).collect();
    // A project pinning its own numprocesses (e.g. 0 for an
    // order-sensitive suite that needs pytest-exact mode) keeps it.
    let fixed: Vec<Option<usize>> = projects.iter().map(|p| mono::project_fixed_n(p)).collect();
    let shares = mono::plan_shares_with_fixed(&costs, &fixed, budget);
    println!(
        "rstest {} — monorepo: {} projects, {budget} workers ({})",
        env!("CARGO_PKG_VERSION"),
        projects.len(),
        rels.iter()
            .zip(&shares)
            .map(|(r, s)| format!("{r}:-n{s}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let start = Instant::now();
    let exe = std::env::current_exe()?;

    // Launch every project concurrently; the shares cap total worker
    // count at the budget. Output prints in COMPLETION order.
    let (tx, rx) = std::sync::mpsc::channel::<(usize, String, i32)>();
    let mut launched = 0usize;
    let mut skipped_projects: Vec<usize> = Vec::new();
    for (i, (project, rel)) in projects.iter().zip(&rels).enumerate() {
        let impact = impacts
            .as_ref()
            .map(|v| v[i])
            .unwrap_or(mono::ChangeImpact::Direct);
        if impact == mono::ChangeImpact::Unaffected {
            skipped_projects.push(i);
            continue;
        }
        let slug = mono::slug(root, project);
        let mut cmd = std::process::Command::new(&exe);
        cmd.current_dir(project)
            .env("RSTEST_MONO_PROJECT", rel)
            // Children inherit the root's run uid (one testrun), passed
            // explicitly rather than through the parent's process env.
            .env("RSTEST_RUN_UID", run_uid)
            .arg("-n")
            .arg(shares[i].to_string())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        // Own flags that must travel to the child; output paths get the
        // project slug and anchor at the INVOCATION root.
        match (&cli.python, mono::project_python(project)) {
            (Some(p), _) => {
                cmd.arg("--python").arg(p);
            }
            // A project-local venv beats the inherited environment.
            (None, Some(p)) => {
                cmd.arg("--python").arg(p);
            }
            (None, None) => {}
        }
        if let Some(d) = &cli.dist {
            cmd.arg("--dist").arg(d);
        }
        // Per-project output style. Children write to a captured pipe (not a
        // tty), so bar/verbose render per-test lines and github emits its
        // `::error` annotations, all reprinted under the project header.
        if let Some(o) = &cli.output {
            cmd.arg("--output").arg(o);
        }
        if let Some(r) = &cli.reruns {
            cmd.arg("--reruns").arg(r.to_string());
        }
        if let Some(q) = &cli.quarantine {
            // Children run with cwd=project, so hand them an absolute path.
            // Patterns match each child's project-relative nodeids.
            cmd.arg("--quarantine")
                .arg(std::fs::canonicalize(q).unwrap_or_else(|_| q.clone()));
        }
        for pat in &cli.only_rerun {
            cmd.arg("--only-rerun").arg(pat);
        }
        if let Some(t) = &cli.worker_timeout {
            cmd.arg("--worker-timeout").arg(t.to_string());
        }
        if cli.doctor {
            cmd.arg("--doctor");
        }
        if let Some(rev) = &mono_changed {
            // Only directly-changed projects narrow further; a dependent
            // runs full (its own files didn't change).
            if impact == mono::ChangeImpact::Direct {
                cmd.arg(format!("--changed={rev}"));
                if cli.changed_strict {
                    cmd.arg("--changed-strict");
                }
            }
        }
        if let Some(p) = &cli.junitxml {
            cmd.arg("--junitxml")
                .arg(root.join(mono::suffixed(p, &slug)));
        }
        if cli.report_json.is_some() {
            // Children write to temp parts; the orchestrator merges them
            // into ONE document at the requested path after the run.
            cmd.arg("--report-json")
                .arg(report_part_path(&slug, run_uid));
        }
        if let Some(p) = &cli.doctor_json {
            cmd.arg("--doctor-json")
                .arg(root.join(mono::suffixed(p, &slug)));
        }
        if let Some(p) = &cli.doctor_md {
            cmd.arg("--doctor-md")
                .arg(root.join(mono::suffixed(p, &slug)));
        }
        // Each project gates its own doctor report; a breach fails that child's
        // exit code, which the orchestrator aggregates.
        for c in &cli.doctor_fail_on {
            cmd.arg("--doctor-fail-on").arg(c);
        }
        cmd.args(args);
        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("rstest: failed to launch project {rel}: {e}");
                let _ = tx.send((i, format!("launch failed: {e}\n"), 3));
                continue;
            }
        };
        launched += 1;
        let tx = tx.clone();
        std::thread::spawn(move || {
            let (out, status) = match child.wait_with_output() {
                Ok(o) => {
                    let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
                    text.push_str(&String::from_utf8_lossy(&o.stderr));
                    (text, o.status.code().unwrap_or(3))
                }
                Err(e) => (format!("wait failed: {e}\n"), 3),
            };
            let _ = tx.send((i, out, status));
        });
    }
    drop(tx);

    let mut results: Vec<Option<i32>> = vec![None; projects.len()];
    for (i, output, status) in rx {
        println!("\n=============== project: {} ===============", rels[i]);
        print!("{output}");
        results[i] = Some(status);
    }
    let _ = launched;

    println!("\n=============== monorepo summary ===============");
    let mut report_parts: Vec<(String, Option<PathBuf>, Option<i32>, bool)> = Vec::new();
    let mut statuses = Vec::new();
    for (i, (rel, status)) in rels.iter().zip(&results).enumerate() {
        if skipped_projects.contains(&i) {
            println!("  {rel:<40} skipped (no changes)");
            report_parts.push((rel.clone(), None, None, true));
            continue;
        }
        let status = status.unwrap_or(3);
        let slug = mono::slug(root, &projects[i]);
        report_parts.push((
            rel.clone(),
            Some(report_part_path(&slug, run_uid)),
            Some(status),
            false,
        ));
        statuses.push(status);
        let verdict = match status {
            0 => "ok".to_string(),
            5 => "no tests".to_string(),
            s => format!("FAILED (exit {s})"),
        };
        println!("  {rel:<40} {verdict}");
    }
    if statuses.is_empty() {
        println!("no projects affected by the change set");
        // Strict gating distinguishes "ran nothing" from "all passed".
        statuses.push(if cli.changed_strict { 5 } else { 0 });
    }
    let merged = pool::merge_statuses(&statuses);
    if let Some(out) = &cli.report_json {
        let out = if out.is_absolute() {
            out.clone()
        } else {
            root.join(out)
        };
        let started_at_epoch =
            crate::time::now_epoch_secs().saturating_sub(start.elapsed().as_secs());
        let run_meta = build_run_meta(start, merged, started_at_epoch, budget);
        if let Err(e) = mono::merge_reports(&report_parts, &run_meta, &out) {
            eprintln!("rstest: failed to write merged report: {e}");
        }
        for (_, part, _, _) in &report_parts {
            if let Some(p) = part {
                let _ = std::fs::remove_file(p);
            }
        }
    }
    println!(
        "{} projects in {:.2}s (exit {merged})",
        statuses.len(),
        start.elapsed().as_secs_f64()
    );
    Ok(merged)
}

/// Temp location for one project's report part during a monorepo run.
fn report_part_path(slug: &str, run_uid: &str) -> PathBuf {
    std::env::temp_dir().join(format!("rstest-mono-{run_uid}-{slug}.json"))
}

#[cfg(test)]
mod tests {
    use super::report_part_path;

    #[test]
    fn report_part_path_names_a_json_file_for_slug() {
        let p = report_part_path("collect", "testuid");
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("rstest-mono-"), "got {name}");
        assert!(name.contains("collect"), "got {name}");
        assert!(name.ends_with(".json"), "got {name}");
    }
}
