//! Monorepo driver: sequential/parallel session groups, one child `rstest`
//! process per subproject, results merged into a single summary and report.

use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;

use super::gates::build_run_meta;
use super::{head_to_none, parse_numprocesses, resolve_changed_base};
use crate::cli::{needs_passthrough_io, Cli};
use crate::reporting::sink::Sink;
use crate::scheduling::pool;
use crate::{doctor, mono, select};

/// Sequential session groups, one per subproject (monorepo P0).
pub(super) fn execute_monorepo(
    cli: &Cli,
    args: &[String],
    root: &std::path::Path,
    projects: Vec<PathBuf>,
    run_uid: &str,
    sink: &mut Sink,
) -> Result<i32> {
    if needs_passthrough_io(args) {
        anyhow::bail!(
            "--pdb/-s/--co need a single pytest session; run inside one project \
             of this monorepo (for --collect-only --report-json discovery, run \
             it once per project)"
        );
    }
    // Validate --doctor-fail-on once here so a malformed condition fails fast
    // at the root, not as N separate child aborts (children re-validate too).
    doctor::parse_conditions(&cli.doctor_fail_on, sink)?;
    validate_monorepo_flags(cli.watch, cli.output.as_deref())?;
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
    let mono_changed = resolve_changed_base(cli, sink)?;
    let impacts: Option<Vec<mono::ChangeImpact>> = match &mono_changed {
        Some(rev) => {
            let rev = head_to_none(rev);
            let changed = select::changed_files_from_git(rev)?;
            let impacts =
                mono::classify_changes(root, &projects, &changed, cli.changed_strict, sink);
            let skipped = impacts
                .iter()
                .filter(|i| **i == mono::ChangeImpact::Unaffected)
                .count();
            sink.warn(&format!(
                "rstest: --changed: {} changed file(s) -> {} of {} projects affected",
                changed.len(),
                projects.len() - skipped,
                projects.len()
            ));
            Some(impacts)
        }
        None => None,
    };
    let costs: Vec<Option<f64>> = projects.iter().map(|p| mono::project_cost(p)).collect();
    // A project pinning its own numprocesses (e.g. 0 for an
    // order-sensitive suite that needs pytest-exact mode) keeps it.
    let fixed: Vec<Option<usize>> = projects.iter().map(|p| mono::project_fixed_n(p)).collect();
    let shares = mono::plan_shares_with_fixed(&costs, &fixed, budget);
    sink.out_line(&format!(
        "rstest {} — monorepo: {} projects, {budget} workers ({})",
        env!("CARGO_PKG_VERSION"),
        projects.len(),
        rels.iter()
            .zip(&shares)
            .map(|(r, s)| format!("{r}:-n{s}"))
            .collect::<Vec<_>>()
            .join(", ")
    ));
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
        // Only directly-changed projects narrow further; a dependent runs its
        // full suite (its own files didn't change).
        let changed = mono_changed
            .as_deref()
            .filter(|_| impact == mono::ChangeImpact::Direct)
            .map(|rev| (rev, cli.changed_strict));
        let spec = ChildSpec {
            share: shares[i],
            root,
            slug: &slug,
            run_uid,
            cli_python: cli.python.as_deref(),
            project_python: mono::project_python(project),
            dist: cli.dist.as_deref(),
            output: cli.output.as_deref(),
            reruns: cli.reruns,
            quarantine: cli.quarantine.as_deref(),
            only_rerun: &cli.only_rerun,
            worker_timeout: cli.worker_timeout,
            doctor: cli.doctor,
            changed,
            junitxml: cli.junitxml.as_deref(),
            report_json: cli.report_json.is_some(),
            doctor_json: cli.doctor_json.as_deref(),
            doctor_md: cli.doctor_md.as_deref(),
            doctor_fail_on: &cli.doctor_fail_on,
        };
        let mut cmd = std::process::Command::new(&exe);
        cmd.current_dir(project)
            .env("RSTEST_MONO_PROJECT", rel)
            // Children inherit the root's run uid (one testrun), passed
            // explicitly rather than through the parent's process env.
            .env("RSTEST_RUN_UID", run_uid)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .args(build_child_args(&spec))
            .args(args);
        if spawn_child_stream(cmd, i, rel, &tx, sink.err()) {
            launched += 1;
        }
    }
    drop(tx);

    let mut results: Vec<Option<i32>> = vec![None; projects.len()];
    for (i, output, status) in rx {
        sink.out_line(&format!(
            "\n=============== project: {} ===============",
            rels[i]
        ));
        sink.out_inline(&output);
        results[i] = Some(status);
    }
    let _ = launched;

    sink.out_line("\n=============== monorepo summary ===============");
    let mut report_parts: Vec<(String, Option<PathBuf>, Option<i32>, bool)> = Vec::new();
    let mut statuses = Vec::new();
    for (i, (rel, status)) in rels.iter().zip(&results).enumerate() {
        if skipped_projects.contains(&i) {
            sink.out_line(&format!("  {rel:<40} skipped (no changes)"));
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
        sink.out_line(&format!("  {rel:<40} {}", verdict_label(status)));
    }
    if statuses.is_empty() {
        sink.out_line("no projects affected by the change set");
        // Strict gating distinguishes "ran nothing" from "all passed".
        statuses.push(if cli.changed_strict { 5 } else { 0 });
    }
    let merged = pool::merge_statuses(&statuses);
    if let Some(out) = &cli.report_json {
        let out = resolve_report_out(out, root);
        let started_at_epoch =
            crate::time::now_epoch_secs().saturating_sub(start.elapsed().as_secs());
        let run_meta = build_run_meta(start, merged, started_at_epoch, budget);
        write_merged_report(sink.err(), &report_parts, &run_meta, &out);
    }
    sink.out_line(&format!(
        "{} projects in {:.2}s (exit {merged})",
        statuses.len(),
        start.elapsed().as_secs_f64()
    ));
    Ok(merged)
}

/// Temp location for one project's report part during a monorepo run.
fn report_part_path(slug: &str, run_uid: &str) -> PathBuf {
    std::env::temp_dir().join(format!("rstest-mono-{run_uid}-{slug}.json"))
}

/// Reject flags that can't be honored at a monorepo root. `--watch` needs a
/// single project; `--output json`/`tap` are live single-session streams that
/// can't be merged across projects (use `--report-json`/`--junitxml`).
fn validate_monorepo_flags(watch: bool, output: Option<&str>) -> Result<()> {
    if watch {
        anyhow::bail!("--watch at a monorepo root is not supported yet; run inside a project");
    }
    // The monorepo orchestrator prints per-project banners and a summary around
    // captured child output, so a clean NDJSON stream isn't possible. The merged
    // --report-json document is the machine-readable surface.
    if output == Some("json") {
        anyhow::bail!(
            "--output json streams live per-session results and can't be merged \
             across a monorepo's projects; use --report-json <path> for one merged \
             machine-readable document, or run --output json inside a single project"
        );
    }
    // Same problem for TAP: each child would emit its own version header,
    // numbering, and plan; concatenated, that is not one valid stream.
    if output == Some("tap") {
        anyhow::bail!(
            "--output tap can't be merged across a monorepo's projects; use \
             --junitxml for per-project machine-readable results, or run \
             --output tap inside a single project"
        );
    }
    Ok(())
}

/// The per-project inputs the orchestrator translates into one child `rstest`
/// invocation. Kept as a struct (not a long argument list) so [`build_child_args`]
/// stays a pure, unit-testable translation.
struct ChildSpec<'a> {
    share: usize,
    root: &'a std::path::Path,
    slug: &'a str,
    run_uid: &'a str,
    /// Explicit `--python` from the root invocation (wins over the project venv).
    cli_python: Option<&'a str>,
    /// A project-local venv discovered by [`mono::project_python`].
    project_python: Option<PathBuf>,
    dist: Option<&'a str>,
    output: Option<&'a str>,
    reruns: Option<u32>,
    quarantine: Option<&'a std::path::Path>,
    only_rerun: &'a [String],
    worker_timeout: Option<u64>,
    doctor: bool,
    /// `(rev, strict)` only for a directly-changed project under `--changed`.
    changed: Option<(&'a str, bool)>,
    junitxml: Option<&'a std::path::Path>,
    /// Whether the root asked for `--report-json` (the child writes a temp part).
    report_json: bool,
    doctor_json: Option<&'a std::path::Path>,
    doctor_md: Option<&'a std::path::Path>,
    doctor_fail_on: &'a [String],
}

/// Build the args appended to a child `rstest` process (after the env/pipe
/// setup, before the forwarded pytest `args`): `-n <share>` plus every root
/// flag that must travel to the child. Output paths get the project slug and
/// anchor at the invocation root; `--quarantine` is made absolute (children run
/// with `cwd = project`); `--report-json` points at a temp part the orchestrator
/// merges after the run.
fn build_child_args(spec: &ChildSpec) -> Vec<String> {
    let mut a: Vec<String> = vec!["-n".into(), spec.share.to_string()];
    // Explicit --python beats the project-local venv beats the inherited env.
    let python = spec
        .cli_python
        .map(str::to_string)
        .or_else(|| spec.project_python.as_deref().map(path_to_string));
    if let Some(p) = python {
        pair(&mut a, "--python", p);
    }
    if let Some(d) = spec.dist {
        pair(&mut a, "--dist", d.into());
    }
    if let Some(o) = spec.output {
        pair(&mut a, "--output", o.into());
    }
    if let Some(r) = spec.reruns {
        pair(&mut a, "--reruns", r.to_string());
    }
    if let Some(q) = spec.quarantine {
        // Children run with cwd=project, so hand them an absolute path;
        // patterns match each child's project-relative nodeids.
        pair(
            &mut a,
            "--quarantine",
            path_to_string(&std::fs::canonicalize(q).unwrap_or_else(|_| q.to_path_buf())),
        );
    }
    for pat in spec.only_rerun {
        pair(&mut a, "--only-rerun", pat.clone());
    }
    if let Some(t) = spec.worker_timeout {
        pair(&mut a, "--worker-timeout", t.to_string());
    }
    if spec.doctor {
        a.push("--doctor".into());
    }
    if let Some((rev, strict)) = spec.changed {
        a.push(format!("--changed={rev}"));
        if strict {
            a.push("--changed-strict".into());
        }
    }
    if let Some(p) = spec.junitxml {
        pair(
            &mut a,
            "--junitxml",
            path_to_string(&spec.root.join(mono::suffixed(p, spec.slug))),
        );
    }
    if spec.report_json {
        pair(
            &mut a,
            "--report-json",
            path_to_string(&report_part_path(spec.slug, spec.run_uid)),
        );
    }
    if let Some(p) = spec.doctor_json {
        pair(
            &mut a,
            "--doctor-json",
            path_to_string(&spec.root.join(mono::suffixed(p, spec.slug))),
        );
    }
    if let Some(p) = spec.doctor_md {
        pair(
            &mut a,
            "--doctor-md",
            path_to_string(&spec.root.join(mono::suffixed(p, spec.slug))),
        );
    }
    // Each project gates its own doctor report; a breach fails that child's
    // exit code, which the orchestrator aggregates.
    for c in spec.doctor_fail_on {
        pair(&mut a, "--doctor-fail-on", c.clone());
    }
    a
}

fn pair(args: &mut Vec<String>, flag: &str, value: String) {
    args.push(flag.into());
    args.push(value);
}

fn path_to_string(p: &std::path::Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Spawn one child and stream its captured output + exit code onto `tx` from a
/// worker thread. Returns whether it launched: a spawn failure reports a
/// synthetic exit 3 on the channel (so the project still appears in the summary)
/// and returns `false`.
fn spawn_child_stream(
    mut cmd: std::process::Command,
    i: usize,
    rel: &str,
    tx: &std::sync::mpsc::Sender<(usize, String, i32)>,
    err: &mut dyn std::io::Write,
) -> bool {
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(err, "rstest: failed to launch project {rel}: {e}");
            let _ = tx.send((i, format!("launch failed: {e}\n"), 3));
            return false;
        }
    };
    let tx = tx.clone();
    std::thread::spawn(move || {
        let (out, status) = child_output(child.wait_with_output());
        let _ = tx.send((i, out, status));
    });
    true
}

/// Normalize a finished child into `(combined output, exit code)`: stdout then
/// stderr concatenated, the process exit code (or 3 if it was signalled). A wait
/// error becomes a synthetic exit 3 with the error as the output.
fn child_output(result: std::io::Result<std::process::Output>) -> (String, i32) {
    match result {
        Ok(o) => {
            let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&o.stderr));
            (text, o.status.code().unwrap_or(3))
        }
        Err(e) => (format!("wait failed: {e}\n"), 3),
    }
}

/// One project's summary verdict from its child exit code.
fn verdict_label(status: i32) -> String {
    match status {
        0 => "ok".to_string(),
        5 => "no tests".to_string(),
        s => format!("FAILED (exit {s})"),
    }
}

/// Resolve the merged `--report-json` output path: an absolute path is used
/// as-is; a relative one anchors at the invocation root (not a child's cwd).
fn resolve_report_out(out: &std::path::Path, root: &std::path::Path) -> PathBuf {
    if out.is_absolute() {
        out.to_path_buf()
    } else {
        root.join(out)
    }
}

/// Write the merged monorepo report and clean up the per-project temp parts. A
/// merge/write failure warns (to `w`) but never fails the run — the exit code is
/// already decided by the child statuses.
fn write_merged_report(
    w: &mut dyn std::io::Write,
    report_parts: &[(String, Option<PathBuf>, Option<i32>, bool)],
    run_meta: &crate::reporting::report::RunMeta,
    out: &std::path::Path,
) {
    if let Err(e) = mono::merge_reports(report_parts, run_meta, out) {
        let _ = writeln!(w, "rstest: failed to write merged report: {e}");
    }
    for (_, part, _, _) in report_parts {
        if let Some(p) = part {
            let _ = std::fs::remove_file(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_child_args, child_output, report_part_path, resolve_report_out, spawn_child_stream,
        validate_monorepo_flags, verdict_label, ChildSpec,
    };
    use std::path::{Path, PathBuf};

    #[test]
    fn report_part_path_names_a_json_file_for_slug() {
        let p = report_part_path("collect", "testuid");
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("rstest-mono-"), "got {name}");
        assert!(name.contains("collect"), "got {name}");
        assert!(name.ends_with(".json"), "got {name}");
    }

    #[test]
    fn validate_monorepo_flags_rejects_watch_and_unmergeable_streams() {
        // The allowed shape: no watch, a mergeable/streaming-agnostic output.
        assert!(validate_monorepo_flags(false, None).is_ok());
        assert!(validate_monorepo_flags(false, Some("bar")).is_ok());
        // --watch needs a single project.
        assert!(validate_monorepo_flags(true, None)
            .unwrap_err()
            .to_string()
            .contains("--watch at a monorepo root"));
        // json/tap can't be merged across projects.
        assert!(validate_monorepo_flags(false, Some("json"))
            .unwrap_err()
            .to_string()
            .contains("--output json"));
        assert!(validate_monorepo_flags(false, Some("tap"))
            .unwrap_err()
            .to_string()
            .contains("--output tap"));
    }

    // A minimal spec with everything off; tests flip on just what they exercise.
    fn bare_spec<'a>(
        root: &'a Path,
        slug: &'a str,
        only_rerun: &'a [String],
        fail_on: &'a [String],
    ) -> ChildSpec<'a> {
        ChildSpec {
            share: 3,
            root,
            slug,
            run_uid: "uid",
            cli_python: None,
            project_python: None,
            dist: None,
            output: None,
            reruns: None,
            quarantine: None,
            only_rerun,
            worker_timeout: None,
            doctor: false,
            changed: None,
            junitxml: None,
            report_json: false,
            doctor_json: None,
            doctor_md: None,
            doctor_fail_on: fail_on,
        }
    }

    // Adjacent (flag, value) lookup in the arg vector.
    fn val_after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .map(String::as_str)
    }

    #[test]
    fn build_child_args_emits_share_and_nothing_else_when_bare() {
        let root = Path::new("/repo");
        let args = build_child_args(&bare_spec(root, "libs-a", &[], &[]));
        assert_eq!(args, vec!["-n".to_string(), "3".to_string()]);
    }

    #[test]
    fn build_child_args_translates_every_root_flag() {
        let root = Path::new("/repo");
        let only = vec!["reA".to_string(), "reB".to_string()];
        let fail = vec!["p95<1.0".to_string()];
        let junit = PathBuf::from("out.xml");
        let dj = PathBuf::from("d.json");
        let dm = PathBuf::from("d.md");
        // Nonexistent quarantine path: canonicalize fails, the raw path is the
        // fallback (both the call and the unwrap_or branch execute).
        let quar = PathBuf::from("/nope/quarantine.txt");
        let mut spec = bare_spec(root, "libs-a", &only, &fail);
        spec.cli_python = Some("/venv/bin/python");
        spec.project_python = Some(PathBuf::from("/proj/.venv/bin/python"));
        spec.dist = Some("loadscope");
        spec.output = Some("github");
        spec.reruns = Some(2);
        spec.quarantine = Some(&quar);
        spec.worker_timeout = Some(30);
        spec.doctor = true;
        spec.changed = Some(("HEAD~2", true));
        spec.junitxml = Some(&junit);
        spec.report_json = true;
        spec.doctor_json = Some(&dj);
        spec.doctor_md = Some(&dm);

        let args = build_child_args(&spec);

        assert_eq!(&args[0..2], &["-n".to_string(), "3".to_string()]);
        // Explicit --python wins over the project venv.
        assert_eq!(val_after(&args, "--python"), Some("/venv/bin/python"));
        assert_eq!(val_after(&args, "--dist"), Some("loadscope"));
        assert_eq!(val_after(&args, "--output"), Some("github"));
        assert_eq!(val_after(&args, "--reruns"), Some("2"));
        assert_eq!(
            val_after(&args, "--quarantine"),
            Some("/nope/quarantine.txt")
        );
        assert_eq!(val_after(&args, "--worker-timeout"), Some("30"));
        assert!(args.iter().any(|a| a == "--doctor"));
        assert!(args.iter().any(|a| a == "--changed=HEAD~2"));
        assert!(args.iter().any(|a| a == "--changed-strict"));
        // Output paths anchor at the root with the slug suffix.
        assert!(val_after(&args, "--junitxml").unwrap().contains("libs-a"));
        assert!(val_after(&args, "--report-json")
            .unwrap()
            .contains("libs-a"));
        assert!(val_after(&args, "--doctor-json")
            .unwrap()
            .contains("libs-a"));
        assert!(val_after(&args, "--doctor-md").unwrap().contains("libs-a"));
        // Repeated flags: one per pattern / condition.
        let rerun_vals: Vec<&String> = args
            .iter()
            .enumerate()
            .filter(|(i, a)| *a == "--only-rerun" && i + 1 < args.len())
            .map(|(i, _)| &args[i + 1])
            .collect();
        assert_eq!(rerun_vals, vec!["reA", "reB"]);
        assert_eq!(val_after(&args, "--doctor-fail-on"), Some("p95<1.0"));
    }

    #[test]
    fn build_child_args_falls_back_to_project_venv() {
        let root = Path::new("/repo");
        let mut spec = bare_spec(root, "libs-a", &[], &[]);
        // No explicit --python => the project-local venv is used.
        spec.project_python = Some(PathBuf::from("/proj/.venv/bin/python"));
        let args = build_child_args(&spec);
        assert_eq!(val_after(&args, "--python"), Some("/proj/.venv/bin/python"));
    }

    #[test]
    fn build_child_args_omits_changed_for_dependent_projects() {
        let root = Path::new("/repo");
        // A dependent (changed: None) runs its full suite: no --changed passed.
        let args = build_child_args(&bare_spec(root, "libs-a", &[], &[]));
        assert!(!args.iter().any(|a| a.starts_with("--changed")));
    }

    #[test]
    fn build_child_args_passes_changed_without_strict() {
        let root = Path::new("/repo");
        let mut spec = bare_spec(root, "libs-a", &[], &[]);
        // Directly-changed but not --changed-strict: rev travels, strict flag
        // does not.
        spec.changed = Some(("HEAD~1", false));
        let args = build_child_args(&spec);
        assert!(args.iter().any(|a| a == "--changed=HEAD~1"));
        assert!(!args.iter().any(|a| a == "--changed-strict"));
    }

    #[test]
    fn child_output_concatenates_stdout_stderr_on_success() {
        // A wait error is the Err arm; success is the Ok arm. Construct a real
        // Output via a trivially-successful command (unix: `sh -c`).
        #[cfg(unix)]
        {
            let out = std::process::Command::new("/bin/sh")
                .args(["-c", "printf OUT; printf ERR >&2; exit 0"])
                .output();
            let (text, code) = child_output(out);
            assert_eq!(code, 0);
            assert!(text.contains("OUT") && text.contains("ERR"), "got {text}");
        }
    }

    #[test]
    #[cfg(unix)]
    fn spawn_child_stream_streams_a_successful_child() {
        // A real, portable, fast success: streams output + exit code onto the
        // channel and reports launched.
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", "printf hello; exit 0"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let (tx, rx) = std::sync::mpsc::channel();
        assert!(spawn_child_stream(cmd, 2, "libs-a", &tx, &mut Vec::new()));
        drop(tx);
        let (i, out, code) = rx.recv().unwrap();
        assert_eq!((i, code), (2, 0));
        assert!(out.contains("hello"), "got {out}");
    }

    #[test]
    fn child_output_reports_wait_error_as_exit_3() {
        let (out, code) = child_output(Err(std::io::Error::other("broke")));
        assert_eq!(code, 3);
        assert!(out.contains("wait failed: broke"), "got {out}");
    }

    #[test]
    fn spawn_child_stream_reports_launch_failure_on_the_channel() {
        // A binary that cannot exist => spawn fails, project still summarized.
        let cmd = std::process::Command::new("/nonexistent/rstest-child-xyz");
        let (tx, rx) = std::sync::mpsc::channel();
        let launched = spawn_child_stream(cmd, 7, "libs-a", &tx, &mut Vec::new());
        assert!(!launched);
        drop(tx);
        let (i, out, code) = rx.recv().unwrap();
        assert_eq!((i, code), (7, 3));
        assert!(out.contains("launch failed"), "got {out}");
    }

    #[test]
    fn verdict_label_maps_exit_codes() {
        assert_eq!(verdict_label(0), "ok");
        assert_eq!(verdict_label(5), "no tests");
        assert_eq!(verdict_label(1), "FAILED (exit 1)");
        assert_eq!(verdict_label(3), "FAILED (exit 3)");
    }

    #[test]
    fn resolve_report_out_anchors_relative_paths_at_root() {
        let root = Path::new("/repo");
        // Relative => joined onto the invocation root.
        assert_eq!(
            resolve_report_out(Path::new("report.json"), root),
            PathBuf::from("/repo/report.json")
        );
        // Absolute => used verbatim.
        let abs = if cfg!(windows) {
            PathBuf::from(r"C:\out\report.json")
        } else {
            PathBuf::from("/out/report.json")
        };
        assert_eq!(resolve_report_out(&abs, root), abs);
    }

    #[test]
    fn write_merged_report_warns_when_the_write_fails() {
        // An out path in a nonexistent directory makes merge_reports fail to
        // write; the helper warns (to the buffer) rather than panicking.
        let meta = crate::reporting::report::RunMeta {
            exitstatus: 0,
            duration_seconds: 0.0,
            started_at_epoch: 0,
            workers: 1,
            argv: vec![],
        };
        let out = Path::new("/nonexistent-dir-xyz-12345/merged.json");
        let parts = vec![("libs-a".to_string(), None, Some(0), false)];
        let mut buf = Vec::new();
        super::write_merged_report(&mut buf, &parts, &meta, out);
        assert!(String::from_utf8(buf)
            .unwrap()
            .contains("failed to write merged report"));
    }

    #[test]
    fn write_merged_report_succeeds_and_removes_parts() {
        let meta = crate::reporting::report::RunMeta {
            exitstatus: 0,
            duration_seconds: 0.0,
            started_at_epoch: 0,
            workers: 1,
            argv: vec![],
        };
        // A real (writable) out and a temp part that exists: merge succeeds and
        // the part is cleaned up afterward.
        let dir = std::env::temp_dir();
        let part = dir.join(format!("rstest-mono-part-{}.json", std::process::id()));
        std::fs::write(&part, b"{}").unwrap();
        let out = dir.join(format!("rstest-mono-merged-{}.json", std::process::id()));
        let parts = vec![("libs-a".to_string(), Some(part.clone()), Some(0), false)];
        let mut buf = Vec::new();
        super::write_merged_report(&mut buf, &parts, &meta, &out);
        assert!(
            buf.is_empty(),
            "unexpected warning: {:?}",
            String::from_utf8(buf)
        );
        assert!(out.exists(), "merged report not written");
        assert!(!part.exists(), "temp part not cleaned up");
        std::fs::remove_file(&out).ok();
    }
}
