//! Post-run gates, reports, and closing output for a single-project run:
//! the doctor gate, junit/html reports, merged lastfailed cache, duration
//! regression gate, coverage combine, cache push, report-json, incremental
//! green-set record, and leak gate ([`run_post_gates`]), plus the closing
//! summary render ([`finalize_output`]).

use std::io::{IsTerminal, Write};
use std::time::Instant;

use anyhow::Result;

use super::{PostRun, RunConfig};
use crate::cli::Cli;
use crate::reporting::ci::{
    buildkite_flaky_annotate, print_azure_annotations, print_github_annotations,
};
use crate::reporting::sink::Sink;
use crate::reporting::{color, flakes, html, junit, progress, report, status};
use crate::scheduling::{durations, pool, proto, worker};
use crate::{cache, coverage_skip, doctor, incremental, remote, select};

/// Assemble the report-json run metadata; `duration_seconds`/`argv` are the same
/// for every run path, only exit status / start epoch / worker count vary.
pub(super) fn build_run_meta(
    start: Instant,
    exitstatus: i32,
    started_at_epoch: u64,
    workers: usize,
) -> report::RunMeta {
    report::RunMeta {
        exitstatus,
        duration_seconds: start.elapsed().as_secs_f64(),
        started_at_epoch,
        workers,
        argv: std::env::args().collect(),
    }
}

/// Write the optional junit/html run reports. Extracted from `execute` so the
/// report side-effects are covered by in-process unit tests (rust-unit), not
/// only incidentally by the e2e gate.
fn write_run_reports(
    junitxml: Option<&std::path::Path>,
    html: Option<&std::path::Path>,
    run: &report::Run,
    suite_seconds: f64,
    meta: &report::RunMeta,
) -> Result<()> {
    if let Some(path) = junitxml {
        junit::write(path, run, suite_seconds)?;
    }
    if let Some(path) = html {
        html::write(path, run, meta)?;
    }
    Ok(())
}

/// The merged lastfailed map written into pytest's cache after a pool run.
/// Each mode keys outcomes "nodeid [gwN]"; lastfailed needs the plain nodeids
/// (deduped, since a test may fail on several workers). BTreeMap => stable,
/// deduped keys with no extra pass.
fn merged_lastfailed(run: &report::Run) -> std::collections::BTreeMap<String, bool> {
    run.failed_nodeids()
        .map(|id| {
            let plain = id.rsplit_once(" [gw").map(|(p, _)| p).unwrap_or(id);
            (plain.to_string(), true)
        })
        .collect()
}

/// Warn (to `w`) that `--doctor-fail-on` can't evaluate under passthrough IO
/// (-s/--pdb/--co): those run a single interactive session with no doctor
/// instrumentation, so the gate would silently pass. Fires only when a gate is
/// set AND the run is passthrough.
fn warn_doctor_gate_passthrough(w: &mut dyn Write, gate_empty: bool, passthrough: bool) {
    if !gate_empty && passthrough {
        let _ = writeln!(
            w,
            "rstest: --doctor-fail-on is ignored under -s/--pdb/--co \
             (no doctor instrumentation in an interactive single session)"
        );
    }
}

/// The `--durations-regress` ratio must be strictly > 1.0: a test is a
/// regression only when it is *slower* than baseline by that factor.
fn validate_regress_ratio(ratio: f64) -> Result<()> {
    if ratio <= 1.0 {
        anyhow::bail!("--durations-regress ratio must be > 1.0, got {ratio}");
    }
    Ok(())
}

/// Reconcile the coverage-reporting subprocess result into the run exit status.
/// `status` is `Ok(success)` once the child exited, `Err(msg)` if it never ran.
/// A covtool failure only turns an otherwise-green run red; it never lowers a
/// non-zero status. A spawn error warns (to `w`) but doesn't fail the run.
fn reconcile_cov_status(w: &mut dyn Write, status: Result<bool, String>, exitstatus: i32) -> i32 {
    match status {
        Ok(false) if exitstatus == 0 => 1,
        Ok(_) => exitstatus,
        Err(e) => {
            let _ = writeln!(w, "rstest: coverage reporting failed to run: {e}");
            exitstatus
        }
    }
}

/// Report a `--cache-push` outcome (to `w`): a success line with the segment's
/// counts, or a warning on failure. A push failure never fails an otherwise-green
/// run — it is reported, not gated.
fn report_push_result(w: &mut dyn Write, result: Result<()>, seg: &remote::Segment, remote: &str) {
    match result {
        Ok(()) => {
            let _ = writeln!(
                w,
                "rstest: cache: pushed segment ({} duration(s), {} event(s), {} covered file(s)) to {remote}",
                seg.durations.len(),
                seg.flake_events.len(),
                seg.cov_index.files.len()
            );
        }
        Err(e) => {
            let _ = writeln!(w, "rstest: cache: push failed: {e:#}");
        }
    }
}

/// Write the optional `--report-json` snapshot. Extracted (like
/// [`write_run_reports`]) so the report side-effect is covered in-process.
fn write_report_json(
    path: Option<&std::path::Path>,
    run: &report::Run,
    meta: &report::RunMeta,
) -> Result<()> {
    if let Some(path) = path {
        run.write_snapshot(path, meta)?;
    }
    Ok(())
}

/// The bar-mode closing "Results (…s): <bar> n/n" line. Pulls the pass/fail/
/// other tallies out of `counts` the way pytest-sugar's segmented bar does
/// (fail = failed+errors+collect_errors; other = skipped+xfailed+xpassed).
fn results_bar_line(
    counts: &std::collections::BTreeMap<&'static str, u64>,
    elapsed: f64,
    palette: &color::Palette,
) -> String {
    let g = counts["passed"];
    let r = counts["failed"] + counts["errors"] + counts["collect_errors"];
    let y = counts["skipped"] + counts["xfailed"] + counts["xpassed"];
    let n = g + r + y;
    format!(
        "\nResults ({elapsed:.2}s):\n  {} {n}/{n}",
        status::summary_bar(g as usize, r as usize, y as usize, palette),
    )
}

/// TeamCity flaky service messages (to `w`), one per flaky test; nothing when no
/// test needed a rerun.
fn write_teamcity_flaky(w: &mut dyn Write, flaky: &[(String, u32)]) {
    let msgs = progress::teamcity_flaky_messages(flaky);
    if !msgs.is_empty() {
        let _ = writeln!(w, "{msgs}");
    }
}

/// Post-run gates and side-effects, in order: the doctor report + `--doctor-fail-on`
/// gate, junit/html reports, merged lastfailed cache, duration-regression gate,
/// coverage combine/report, duration+flake save, `--cache-push`, report-json,
/// the `--incremental` green-set record, and the `--fail-on-leak` gate. Returns
/// the reconciled process exit status (starting from the pool's, raised by any
/// gate breach).
pub(super) fn run_post_gates(
    cfg: &RunConfig,
    cli: &Cli,
    outcome: &mut pool::PoolOutcome,
    args: &[String],
    post: &PostRun,
    sink: &mut Sink,
) -> Result<i32> {
    let RunConfig {
        n,
        passthrough,
        mode,
        doctor,
        ref doctor_gate,
        ref dist_name,
        ref python,
        ref scope,
        ..
    } = *cfg;
    let palette = sink.palette();
    let PostRun {
        start,
        started_epoch,
        run_uid,
        cache_remote,
        shard,
        since_green,
        head,
        env_fp,
        incremental_active,
        config_fp,
        prev_index,
        baseline,
    } = *post;
    // A passthrough-IO run (-s/--pdb/--co) skips doctor instrumentation, so the
    // gate can't evaluate; say so instead of a silent false green.
    warn_doctor_gate_passthrough(sink.err(), doctor_gate.is_empty(), passthrough);
    let mut doctor_gate_failed = false;
    if (cli.doctor
        || cli.doctor_json.is_some()
        || cli.doctor_md.is_some()
        || !doctor_gate.is_empty())
        && !passthrough
    {
        let report = doctor::analyze(
            &outcome.run,
            &merge_fixtures(std::mem::take(&mut outcome.fixtures)),
            start.elapsed().as_secs_f64(),
            n,
        );
        // In json mode stdout is a pure NDJSON stream, so the doctor's human
        // report would corrupt it; --doctor-json still writes to its file.
        if cli.doctor && mode != progress::Mode::Json {
            doctor::render(sink, &report);
        }
        if let Some(path) = &cli.doctor_json {
            doctor::write_json(path, &report)?;
        }
        if let Some(path) = &cli.doctor_md {
            doctor::write_markdown(path, &report)?;
        }
        doctor::append_ci_summary(sink, &report)?;
        if !doctor_gate.is_empty() {
            let gate = doctor::evaluate(&report, doctor_gate);
            for s in &gate.skipped {
                sink.warn(&format!("rstest: --doctor-fail-on: {s}"));
            }
            if gate.breaches.is_empty() {
                sink.warn(&format!(
                    "rstest: --doctor-fail-on: all {} condition(s) passed",
                    doctor_gate.len()
                ));
            } else {
                // stderr, not stdout: --output json/tap keep stdout a pure
                // machine stream, and the failure block must not corrupt it
                // (same reason the human doctor render is gated above).
                sink.warn(&format!(
                    "\n{}",
                    palette.bold_red("=========== doctor gate failures ===========")
                ));
                for b in &gate.breaches {
                    sink.warn(&format!("  {b}"));
                }
                doctor_gate_failed = true;
            }
        }
    }
    write_run_reports(
        cli.junitxml.as_deref(),
        cli.html.as_deref(),
        &outcome.run,
        start.elapsed().as_secs_f64(),
        &build_run_meta(start, outcome.exitstatus, started_epoch, n),
    )?;
    // Merged lastfailed: workers' own writes are blocked in pool mode
    // (each knows only its failures); write the union into pytest's cache
    // so a follow-up `--lf` behaves exactly as after a serial run.
    if let Some(cache_dir) = &outcome.cache_dir {
        let failed = merged_lastfailed(&outcome.run);
        let dir = std::path::Path::new(cache_dir).join("v/cache");
        // Only write when serialization succeeds: a serialize error must not
        // clobber pytest's lastfailed cache with an empty `{}`.
        if let (Ok(()), Ok(bytes)) = (std::fs::create_dir_all(&dir), serde_json::to_vec(&failed)) {
            let _ = std::fs::write(dir.join("lastfailed"), bytes);
        }
    }
    // Duration regression gate: must compare BEFORE durations::save
    // overwrites the baseline with this run's times.
    let mut duration_regressions = 0usize;
    if let Some(ratio) = cli.durations_regress {
        validate_regress_ratio(ratio)?;
        let baseline = durations::load();
        if baseline.is_empty() {
            sink.warn(
                "rstest: --durations-regress: no duration baseline yet \
                 (.rstest_cache/durations.json); comparison skipped",
            );
        } else {
            let rows = durations::regressions(&outcome.run, &baseline, ratio);
            if rows.is_empty() {
                sink.warn(&format!(
                    "rstest: --durations-regress: no regressions (>= {ratio}x baseline)"
                ));
            } else {
                sink.out_line(&format!(
                    "\n{}",
                    palette.bold_red(&format!(
                        "=========== duration regressions (>= {ratio}x baseline) ==========="
                    ))
                ));
                for (nodeid, old, new) in &rows {
                    sink.out_line(&format!("  {old:7.2}s -> {new:7.2}s  {nodeid}"));
                }
                duration_regressions = rows.len();
            }
        }
    }
    // Before publishing, drop any coverage index left by --cache-pull: only an
    // index covtool writes for THIS run may be pushed. Unconditional on push (not
    // gated by --cov) so a run that produces no fresh index — no --cov at all, no
    // --cov-context, or an empty shard — pushes an empty slice rather than
    // re-publishing the pulled merged index as its own. Selection already
    // consumed the pulled index earlier, so removing it now is safe.
    if cli.cache_push {
        let _ = std::fs::remove_file(cache::file(select::COVERAGE_INDEX_FILE));
    }
    // Coverage: workers save suffixed data files (pytest-cov worker mode);
    // the orchestrator plays the xdist-master role, so combine and report.
    // Runs BEFORE the cache-push below so this run's coverage-index slice is
    // materialized (covtool overwrites the local index) in time to be published.
    let mut exitstatus = outcome.exitstatus;
    if !passthrough && args.iter().any(|a| a == "--cov" || a.starts_with("--cov=")) {
        sink.out_line("");
        let status = std::process::Command::new(python)
            .args(["-m", "rstest_worker.covtool"])
            .args(args)
            .env("PYTHONPATH", worker::worker_pythonpath())
            // Same cache dir the Rust side reads (cache::dir()) so the index
            // lands where load_coverage_index / --cache-push look for it.
            .env("RSTEST_CACHE", cache::dir())
            .status();
        exitstatus = reconcile_cov_status(
            sink.err(),
            status.map(|s| s.success()).map_err(|e| e.to_string()),
            exitstatus,
        );
    }
    // Each-mode ids carry the [gwN] suffix and every test ran N times, so
    // they would poison the duration cache used for LPT scheduling.
    if dist_name != "each" {
        durations::save(&outcome.run);
        // Whole-suite wall (fixtures included) for the monorepo planner: a
        // fixture-bound project has near-zero call time in durations.json but
        // real elapsed cost here, so weighting by call time alone starves it
        // to one worker on the warm run. See `mono::project_cost`.
        durations::save_wall(start.elapsed().as_secs_f64());
        // Flake history rides the same cadence (and the same [gwN]-key
        // poisoning concern rules out each-mode).
        flakes::record(&outcome.run);
        // --cache-push: publish THIS run's contribution as one immutable
        // segment (from the in-memory Run, not the merged local cache). A push
        // failure warns but never fails an otherwise-green run.
        if cli.cache_push {
            let remote = cache_remote.unwrap(); // validated at entry
            let uid = run_uid;
            let shard_suffix = shard.map(|(k, n)| format!("-{k}of{n}")).unwrap_or_default();
            // This run's coverage slice (covtool wrote it just above); empty for
            // non-coverage runs. Published as the segment's cov_index.
            let cov = remote::load_local_cov_index();
            let seg = remote::segment_from_run(
                format!("{uid}{shard_suffix}"),
                started_epoch,
                &outcome.run,
                cov,
            );
            let result = remote::transport_for(remote).and_then(|t| remote::push(t.as_ref(), &seg));
            report_push_result(sink.err(), result, &seg, remote);
        }
    }
    write_report_json(
        cli.report_json.as_deref(),
        &outcome.run,
        &build_run_meta(start, outcome.exitstatus, started_epoch, n),
    )?;
    if duration_regressions > 0 {
        sink.warn(&format!(
            "rstest: {duration_regressions} duration regression{} vs baseline (--durations-regress)",
            if duration_regressions > 1 { "s" } else { "" }
        ));
        if exitstatus == 0 {
            exitstatus = 1;
        }
    }
    if doctor_gate_failed {
        sink.warn("rstest: --doctor-fail-on: threshold breach (see doctor gate failures above)");
        if exitstatus == 0 {
            exitstatus = 1;
        }
    }
    // Incremental testing: a fully green run advances the baseline to the commit
    // we ran at, so the next --since-green run only re-selects changes made after
    // it. Recorded only on green (exitstatus 0) — a failing test keeps being
    // selected until it passes.
    if since_green && exitstatus == 0 {
        if let Some(h) = head {
            incremental::record_green(&std::env::current_dir()?, h, env_fp);
        }
    }
    // --incremental: persist this run's green set (tests that ran green + the
    // carried-forward cached passes) so the next run skips what stays unchanged.
    // Recorded regardless of exit status — failures simply aren't in the green
    // set, so they re-run next time. Best-effort.
    if incremental_active {
        // Fold cached tests' prior coverage back into the index covtool just
        // rewrote (skipped tests produced none), so they stay skippable.
        let cached = outcome.run.cached_nodeids();
        if !cached.is_empty() {
            let mut new_index = remote::load_local_cov_index();
            coverage_skip::carry_forward(prev_index, &mut new_index, &cached);
            coverage_skip::write_index(&new_index);
        }
        // Restore cached (not-run) entries' def line from the baseline before
        // reading it back — so it persists into this run's recorded lines and
        // every artifact reflects the real line, not a blank.
        outcome.run.backfill_cached_linenos(&baseline.test_lines);
        coverage_skip::record(
            scope,
            config_fp,
            outcome.run.green_nodeids(),
            outcome.run.green_linenos(),
        );
    }
    // --fail-on-leak: gate on any test that leaked a thread/fd. Printed on
    // stderr so --output json/tap keep stdout a pure machine stream.
    if cli.fail_on_leak && passthrough {
        // Passthrough (-s/--pdb/--co) has no worker instrumentation, so no
        // deltas are measured. Warn instead of silently exiting 0 (matches the
        // --quarantine passthrough behavior).
        sink.warn(
            "rstest: --fail-on-leak has no effect in passthrough mode \
             (-s/--pdb/--co); ignoring",
        );
    } else if cli.fail_on_leak {
        let leaks = doctor::detect_leaks(&outcome.run);
        if leaks.is_empty() {
            // Note the blind spot: the first test each worker runs is an
            // unchecked warm-up (first-touch imports aren't a per-test leak),
            // so a clean gate does not prove those tests are leak-free.
            sink.warn(
                "rstest: --fail-on-leak: no thread/fd leaks detected \
                 (first test per worker runs as an unchecked warm-up)",
            );
        } else {
            // Under --doctor the RESOURCE LEAKS section already listed these;
            // only gate + summarize here to avoid printing the table twice.
            if !doctor {
                sink.warn(&format!(
                    "\n{}",
                    palette.bold_red("=========== resource leaks ===========")
                ));
                for l in leaks.iter().take(20) {
                    sink.warn(&format!("  {}  {}", doctor::leak_delta(l), l.nodeid));
                }
            }
            sink.warn(&format!(
                "rstest: --fail-on-leak: {} test(s) leaked threads/fds",
                leaks.len()
            ));
            if exitstatus == 0 {
                exitstatus = 1;
            }
        }
    }
    Ok(exitstatus)
}

pub(super) fn finalize_output(
    outcome: &mut pool::PoolOutcome,
    passthrough: bool,
    mode: progress::Mode,
    durations: Option<(usize, f64)>,
    very_verbose: bool,
    start: Instant,
    sink: &mut Sink,
) {
    let palette = sink.palette();
    // Loaded before this run's events are recorded, so the history
    // annotations say "before this run".
    let flake_history = flakes::load();
    if !passthrough && mode == progress::Mode::Json {
        // Pure NDJSON: close the stream with a session-finish envelope
        // (counts + duration + exit status). No human summary/failures.
        outcome.prog.finish(sink);
        let envelope = serde_json::json!({
            "event": "sessionfinish",
            "exitstatus": outcome.exitstatus,
            "duration": (start.elapsed().as_secs_f64() * 100.0).round() / 100.0,
            "counts": outcome.run.counts(),
        });
        sink.out_line(&envelope.to_string());
    } else if !passthrough && mode == progress::Mode::Tap {
        // Pure TAP: close the stream with the trailing plan. Failure text
        // already rode along as `#` diagnostics; no human summary.
        outcome.prog.finish(sink);
        outcome.prog.tap_plan(sink);
    } else if !passthrough {
        outcome.prog.finish(sink);
        let wrap = match mode {
            progress::Mode::Gitlab => report::FailureWrap::GitlabSection,
            progress::Mode::Buildkite => report::FailureWrap::BuildkiteGroup,
            _ => report::FailureWrap::Plain,
        };
        // Bar mode already inlines each failure as it happens; re-printing
        // the batched block would duplicate it.
        if mode != progress::Mode::Bar {
            outcome.run.print_failures(sink, wrap);
        }
        outcome.run.print_quarantined(sink, &flake_history);
        outcome.run.print_flaky(sink, &flake_history, wrap);
        print_warnings_summary(sink.out(), &outcome.warnings, &palette);
        if let Some((dn, dmin)) = durations {
            outcome.run.print_durations(dn, dmin, very_verbose, sink);
        }
        let warn_total: u64 = outcome.warnings.iter().map(|w| w.count).sum();
        let warn_part = if warn_total > 0 {
            format!(", {warn_total} warnings")
        } else {
            String::new()
        };
        let elapsed = start.elapsed().as_secs_f64();
        // Bar mode closes with pytest-sugar's segmented results bar above
        // the stable summary line (which tooling/CI greps, so keep it intact).
        // The bar gives its own visual break; other modes get a blank line.
        if mode == progress::Mode::Bar && std::io::stdout().is_terminal() {
            let line = results_bar_line(&outcome.run.counts(), elapsed, &palette);
            sink.out_line(&line);
        } else {
            sink.out_line("");
        }
        let cached = outcome.run.cached_count();
        let cached_note = if cached > 0 {
            format!(" ({cached} cached)")
        } else {
            String::new()
        };
        let summary = format!(
            "{}{warn_part} in {elapsed:.2}s{cached_note}",
            outcome.run.summary_line()
        );
        let summary = if outcome.run.all_passed() {
            palette.green(&summary)
        } else {
            palette.red(&summary)
        };
        sink.out_line(&summary);
        // CI-native surfaces emitted from the aggregate at end-of-run. Failures
        // already rode along above, so these add each platform's flake signal
        // (GitHub/Azure annotations here; TeamCity as live service messages).
        match mode {
            progress::Mode::Github => print_github_annotations(sink, &outcome.run),
            progress::Mode::Azure => print_azure_annotations(sink, &outcome.run),
            progress::Mode::Buildkite => buildkite_flaky_annotate(sink, &outcome.run),
            progress::Mode::Teamcity => write_teamcity_flaky(sink.out(), &outcome.run.flaky),
            _ => {}
        }
    }
}

/// Sum identical fixtures reported by multiple workers.
fn merge_fixtures(all: Vec<proto::FixtureStat>) -> Vec<proto::FixtureStat> {
    use std::collections::BTreeMap;
    let mut merged: BTreeMap<(String, String), proto::FixtureStat> = BTreeMap::new();
    for f in all {
        merged
            .entry((f.name.clone(), f.scope.clone()))
            .and_modify(|m| {
                m.count += f.count;
                m.total += f.total;
            })
            .or_insert(f);
    }
    merged.into_values().collect()
}

/// pytest-style warnings summary: grouped by location, deduped, counted.
/// Writes to `w` (stdout at the call site) so the merge/plural formatting is
/// unit-testable.
fn print_warnings_summary(
    w: &mut dyn Write,
    warnings: &[proto::WarningEntry],
    palette: &color::Palette,
) {
    if warnings.is_empty() {
        return;
    }
    use std::collections::BTreeMap;
    let mut merged: BTreeMap<(&str, u64, &str, &str), u64> = BTreeMap::new();
    for entry in warnings {
        *merged
            .entry((
                &entry.filename,
                entry.lineno,
                &entry.category,
                &entry.message,
            ))
            .or_default() += entry.count;
    }
    let _ = writeln!(
        w,
        "\n{}",
        palette.yellow("=========== warnings summary ===========")
    );
    for ((filename, lineno, category, message), count) in &merged {
        let times = if *count > 1 {
            format!("  ({count} occurrences)")
        } else {
            String::new()
        };
        let _ = writeln!(w, "{filename}:{lineno}: {category}{times}");
        for line in message.lines().take(3) {
            let _ = writeln!(w, "  {line}");
        }
    }
    let _ = writeln!(
        w,
        "{}",
        palette.yellow("-- use -W error::... to turn warnings into errors --")
    );
}

/// Compile the --quarantine file into one matcher: exact nodeids or `*`
/// globs, one per line, `#` comments and blanks skipped.
pub(super) fn quarantine_matcher(
    path: &std::path::Path,
    sink: &mut Sink,
) -> Result<regex::RegexSet> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("--quarantine: cannot read {}: {e}", path.display()))?;
    let patterns: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| {
            format!(
                "^{}$",
                l.split('*')
                    .map(regex::escape)
                    .collect::<Vec<_>>()
                    .join(".*")
            )
        })
        .collect();
    if patterns.is_empty() {
        sink.warn(&format!(
            "rstest: --quarantine: {} lists no patterns",
            path.display()
        ));
    }
    Ok(regex::RegexSet::new(patterns)?)
}

#[cfg(test)]
mod tests {
    use super::{
        build_run_meta, merge_fixtures, merged_lastfailed, print_warnings_summary,
        quarantine_matcher, reconcile_cov_status, report_push_result, results_bar_line,
        validate_regress_ratio, warn_doctor_gate_passthrough, write_report_json, write_run_reports,
        write_teamcity_flaky,
    };
    use crate::reporting::color::Palette;
    use crate::reporting::report::Run;
    use crate::reporting::sink::Sink;
    use crate::scheduling::proto::{FixtureStat, WarningEntry};
    use std::time::Instant;

    // Color-disabled palette: deterministic strings, no tty/env dependence.
    fn plain_palette() -> Palette {
        Palette::detect(&["--color=no".to_string()])
    }

    fn utf8(buf: Vec<u8>) -> String {
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn merged_lastfailed_strips_worker_suffix_and_dedups() {
        use crate::scheduling::proto::Report;
        let fail = |nodeid: &str| Report {
            nodeid: nodeid.into(),
            when: "call".into(),
            outcome: "failed".into(),
            duration: 0.1,
            longrepr: None,
            wasxfail: false,
            skip_reason: None,
            cpu: None,
            sections: Vec::new(),
            lineno: None,
            thread_delta: None,
            fd_delta: None,
        };
        let mut run = Run::default();
        // Same test failing on two workers => one plain key after merge.
        run.record(Some(0), fail("t.py::a [gw0]"));
        run.record(Some(1), fail("t.py::a [gw1]"));
        run.record(Some(0), fail("t.py::b [gw0]"));
        // A nodeid with no worker suffix passes through untouched.
        run.record(None, fail("t.py::c"));

        let merged = merged_lastfailed(&run);
        let keys: Vec<&String> = merged.keys().collect();
        assert_eq!(keys, vec!["t.py::a", "t.py::b", "t.py::c"]);
        assert!(merged.values().all(|&v| v));
    }

    #[test]
    fn build_run_meta_passes_through_fields() {
        let m = build_run_meta(Instant::now(), 7, 1_700_000_000, 4);
        assert_eq!(m.exitstatus, 7);
        assert_eq!(m.workers, 4);
        assert_eq!(m.started_at_epoch, 1_700_000_000);
        assert!(m.duration_seconds >= 0.0);
        assert!(!m.argv.is_empty());
    }

    #[test]
    fn write_run_reports_writes_requested_formats_only() {
        let run = Run::default();
        let meta = build_run_meta(Instant::now(), 0, 1_700_000_000, 2);
        let base = std::env::temp_dir().join(format!("rstest-reports-{}", std::process::id()));
        let xml = base.with_extension("xml");
        let html = base.with_extension("html");

        // Neither requested => no files, no error.
        write_run_reports(None, None, &run, 1.0, &meta).unwrap();
        assert!(!xml.exists() && !html.exists());

        // Both requested => both written.
        write_run_reports(Some(&xml), Some(&html), &run, 1.0, &meta).unwrap();
        assert!(xml.exists(), "junit report not written");
        assert!(html.exists(), "html report not written");

        let _ = std::fs::remove_file(&xml);
        let _ = std::fs::remove_file(&html);
    }

    #[test]
    fn merge_fixtures_sums_by_name_and_scope() {
        let stat = |name: &str, scope: &str, count, total| FixtureStat {
            name: name.into(),
            scope: scope.into(),
            count,
            total,
        };
        let merged = merge_fixtures(vec![
            stat("db", "session", 2, 1.0),
            stat("db", "session", 3, 0.5),   // same key => summed
            stat("db", "function", 1, 0.25), // different scope => distinct
            stat("cache", "session", 4, 2.0),
        ]);
        assert_eq!(merged.len(), 3);
        let db_session = merged
            .iter()
            .find(|f| f.name == "db" && f.scope == "session")
            .unwrap();
        assert_eq!(db_session.count, 5);
        assert!((db_session.total - 1.5).abs() < 1e-9);
    }

    fn write_quarantine(suffix: &str, body: &str) -> std::path::PathBuf {
        // Unique per-test name (pid + suffix) so parallel tests never collide.
        let path = std::env::temp_dir().join(format!(
            "rstest-quarantine-test-{}-{suffix}.txt",
            std::process::id()
        ));
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn quarantine_matcher_handles_exact_globs_comments_blanks() {
        let path = write_quarantine(
            "mixed",
            "# a comment\n\ntest_foo.py::test_a\ntest_bar.py::*\n",
        );
        let set = quarantine_matcher(&path, &mut Sink::captured().0).unwrap();
        assert_eq!(set.len(), 2); // comment + blank line skipped
        assert!(set.is_match("test_foo.py::test_a")); // exact
        assert!(!set.is_match("test_foo.py::test_ab")); // anchored: no substring match
        assert!(set.is_match("test_bar.py::test_z")); // glob
        assert!(!set.is_match("other.py::test_a"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn quarantine_matcher_empty_when_only_comments() {
        let path = write_quarantine("empty", "# nothing here\n\n");
        let set = quarantine_matcher(&path, &mut Sink::captured().0).unwrap();
        assert_eq!(set.len(), 0);
        assert!(!set.is_match("test_foo.py::test_a"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn quarantine_matcher_errors_on_missing_file() {
        let path = std::env::temp_dir().join("rstest-quarantine-does-not-exist-xyz.txt");
        assert!(quarantine_matcher(&path, &mut Sink::captured().0).is_err());
    }

    #[test]
    fn warn_doctor_gate_passthrough_fires_only_when_gated_and_passthrough() {
        // Gate set + passthrough: the one case that warns.
        let mut buf = Vec::new();
        warn_doctor_gate_passthrough(&mut buf, false, true);
        assert!(utf8(buf).contains("--doctor-fail-on is ignored under -s/--pdb/--co"));
        // No gate, or not passthrough => silent.
        let mut buf = Vec::new();
        warn_doctor_gate_passthrough(&mut buf, true, true);
        assert!(buf.is_empty());
        let mut buf = Vec::new();
        warn_doctor_gate_passthrough(&mut buf, false, false);
        assert!(buf.is_empty());
    }

    #[test]
    fn validate_regress_ratio_requires_greater_than_one() {
        assert!(validate_regress_ratio(1.5).is_ok());
        // 1.0 and below are rejected (a regression must be strictly slower).
        assert!(validate_regress_ratio(1.0).is_err());
        assert!(validate_regress_ratio(0.5).is_err());
        let err = validate_regress_ratio(0.9).unwrap_err().to_string();
        assert!(err.contains("must be > 1.0"), "got {err}");
    }

    #[test]
    fn reconcile_cov_status_only_raises_a_green_run() {
        let mut sink = Vec::new();
        // covtool failed and the run was green => red.
        assert_eq!(reconcile_cov_status(&mut sink, Ok(false), 0), 1);
        // covtool failed but the run was already red => unchanged.
        assert_eq!(reconcile_cov_status(&mut sink, Ok(false), 2), 2);
        // covtool succeeded => status untouched.
        assert_eq!(reconcile_cov_status(&mut sink, Ok(true), 0), 0);
        assert!(sink.is_empty());
        // Spawn error warns but never fails the run.
        let mut buf = Vec::new();
        assert_eq!(
            reconcile_cov_status(&mut buf, Err("no python".into()), 0),
            0
        );
        assert!(utf8(buf).contains("coverage reporting failed to run: no python"));
    }

    fn segment(durations: usize, events: usize) -> crate::remote::Segment {
        use crate::remote::{FlakeEvent, FlakeKind, Segment};
        Segment {
            schema: 1,
            id: "seg".into(),
            generated_at: 0,
            durations: (0..durations).map(|i| (format!("t{i}"), 0.1)).collect(),
            flake_events: (0..events)
                .map(|i| FlakeEvent {
                    nodeid: format!("t{i}"),
                    kind: FlakeKind::Flaky,
                })
                .collect(),
            cov_index: Default::default(),
        }
    }

    #[test]
    fn report_push_result_prints_counts_on_ok_and_warns_on_err() {
        // Ok: a success line carrying the segment's counts.
        let mut buf = Vec::new();
        report_push_result(&mut buf, Ok(()), &segment(2, 1), "s3://bucket");
        let out = utf8(buf);
        assert!(out.contains("pushed segment (2 duration(s), 1 event(s), 0 covered file(s))"));
        assert!(out.contains("s3://bucket"));
        // Err: a warning, never a panic or gate.
        let mut buf = Vec::new();
        report_push_result(
            &mut buf,
            Err(anyhow::anyhow!("network down")),
            &segment(0, 0),
            "s3://bucket",
        );
        assert!(utf8(buf).contains("cache: push failed: network down"));
    }

    #[test]
    fn write_report_json_writes_only_when_requested() {
        let run = Run::default();
        let meta = build_run_meta(Instant::now(), 0, 1_700_000_000, 2);
        // None => no write, no error.
        write_report_json(None, &run, &meta).unwrap();
        // Some => snapshot written to the path.
        let path =
            std::env::temp_dir().join(format!("rstest-reportjson-{}.json", std::process::id()));
        write_report_json(Some(&path), &run, &meta).unwrap();
        assert!(path.exists(), "report-json snapshot not written");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn results_bar_line_tallies_pass_fail_other() {
        let mut counts: std::collections::BTreeMap<&'static str, u64> = [
            ("passed", 3),
            ("failed", 1),
            ("errors", 1),
            ("collect_errors", 0),
            ("skipped", 2),
            ("xfailed", 0),
            ("xpassed", 0),
        ]
        .into();
        // 3 passed, 2 fail (failed+errors), 2 other (skipped) => 7 total.
        let line = results_bar_line(&counts, 1.23, &plain_palette());
        assert!(line.contains("Results (1.23s):"), "got {line}");
        assert!(line.contains("7/7"), "got {line}");
        // Zeroing everything gives 0/0 without panicking on the index lookups.
        for v in counts.values_mut() {
            *v = 0;
        }
        assert!(results_bar_line(&counts, 0.0, &plain_palette()).contains("0/0"));
    }

    #[test]
    fn write_teamcity_flaky_emits_only_for_reruns() {
        // A flaky test => a service message on the stream.
        let mut buf = Vec::new();
        write_teamcity_flaky(&mut buf, &[("t.py::a".to_string(), 2)]);
        let out = utf8(buf);
        assert!(out.contains("##teamcity[message"), "got {out}");
        assert!(out.contains("flaky: t.py::a"), "got {out}");
        // No flaky tests => nothing written.
        let mut buf = Vec::new();
        write_teamcity_flaky(&mut buf, &[]);
        assert!(buf.is_empty());
    }

    #[test]
    fn print_warnings_summary_merges_and_pluralizes() {
        let warn = |message: &str, count| WarningEntry {
            when: "runtest".into(),
            category: "DeprecationWarning".into(),
            message: message.into(),
            filename: "t.py".into(),
            lineno: 12,
            count,
        };
        let mut buf = Vec::new();
        // Two entries at the same (file,line,category,message) => merged to 3
        // occurrences; a distinct one prints without the count suffix.
        print_warnings_summary(
            &mut buf,
            &[warn("old api", 2), warn("old api", 1), warn("other", 1)],
            &plain_palette(),
        );
        let out = utf8(buf);
        assert!(out.contains("warnings summary"), "got {out}");
        assert!(
            out.contains("t.py:12: DeprecationWarning  (3 occurrences)"),
            "got {out}"
        );
        // The single-occurrence entry has no "(N occurrences)" suffix.
        assert!(
            out.lines().any(|l| l == "t.py:12: DeprecationWarning"),
            "got {out}"
        );
        assert!(out.contains("-- use -W error::"), "got {out}");

        // Empty input => nothing at all.
        let mut buf = Vec::new();
        print_warnings_summary(&mut buf, &[], &plain_palette());
        assert!(buf.is_empty());
    }
}
