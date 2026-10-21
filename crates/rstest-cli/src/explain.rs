//! `rstest explain <nodeid>`: print one test's dossier from the caches without
//! running anything.
//!
//! rstest accretes rich per-test data across runs — the duration cache, the
//! flake/fail log, the last-green outcome set, the coverage index — but every
//! surface renders it suite-wide. `explain` answers "tell me everything about
//! THIS test" by reading those caches and merging the entries keyed on one
//! nodeid. Run-less and interpreter-free, like `cache-compact` / `shard-verify`.
//!
//! What the local caches can source today: the last recorded call-phase
//! duration, flake/fail counts + last-event epoch, whether the test was green on
//! the last incremental run (plus its source line), its coverage footprint
//! (which files it covered), and its recent run history (outcome series,
//! duration spread, the commit it last passed and first failed at).

use std::collections::BTreeSet;

use anyhow::Result;
use serde::Serialize;

use crate::reporting::color::Palette;
use crate::reporting::flakes::FlakeStats;
use crate::reporting::history::{Entry, RunMeta};
use crate::reporting::sink::Sink;
use crate::select::{CoverageIndex, COVERAGE_INDEX_FILE, COVERAGE_INDEX_SCHEMA};

const SCHEMA: u32 = 1;

/// Envelope metadata for the explain document.
#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
struct Meta {
    /// Constant producer tag: always `"rstest"`.
    runner: &'static str,
    /// Constant discriminator: always `"explain"`.
    kind: &'static str,
    /// Document schema version.
    schema: u32,
    /// The rstest version that wrote the document.
    rstest_version: &'static str,
}

/// The coverage footprint of one test: the source files it covered and the
/// total number of covered lines across them.
#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
struct Coverage {
    /// Number of source files the test covered.
    file_count: usize,
    /// Covered lines, summed across those files.
    line_count: usize,
    /// Covered source files, sorted, rootdir-relative (the coverage-index keys).
    files: Vec<String>,
}

/// The test's recent runs from the per-test history (oldest first).
#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
struct HistorySummary {
    /// Number of recorded runs.
    runs: usize,
    /// Outcome per run, oldest first: `passed`, `failed`, `errors`, `skipped`,
    /// `xfailed`, `xpassed`, `quarantined`, `flaky` or `crashed`.
    outcomes: Vec<String>,
    /// Median call-phase seconds over passing runs.
    call_median: Option<f64>,
    /// 95th percentile (nearest rank) call-phase seconds over passing runs.
    call_p95: Option<f64>,
    /// Median setup-phase seconds over passing runs.
    setup_median: Option<f64>,
    /// Commit of the most recent passing run.
    last_pass_commit: Option<String>,
    /// When the latest run failed: commit of the first run in that failing streak.
    first_fail_commit: Option<String>,
}

/// The merged per-nodeid dossier. `null` fields are simply absent from every
/// cache (e.g. a never-flaked test has no `flakes` entry). `found` is false when
/// the nodeid appears in no cache at all.
#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub(crate) struct ExplainReport {
    /// Envelope: producer, document kind, schema version and rstest version.
    meta: Meta,
    /// The node id that was explained, as given.
    nodeid: String,
    /// Whether the node id appears in any cache.
    found: bool,
    /// Last recorded call-phase duration in seconds (see `history` for the
    /// spread).
    duration_seconds: Option<f64>,
    /// `"passed"` if the test was green on the last incremental run; `null`
    /// otherwise (absence is not proof of failure; see `flakes` for fail
    /// history).
    last_outcome: Option<&'static str>,
    /// Source def line (1-based) recorded on the last incremental run, if known.
    source_line: Option<u64>,
    /// Cross-run flake/fail counts + last-event epoch, if the test has any.
    flakes: Option<FlakeStats>,
    /// Files this test covered, if a warm coverage index has it.
    coverage: Option<Coverage>,
    /// Recent runs, if the per-test history has any.
    history: Option<HistorySummary>,
}

fn is_pass(outcome: &str) -> bool {
    matches!(outcome, "passed" | "flaky")
}

fn is_fail(outcome: &str) -> bool {
    matches!(outcome, "failed" | "errors" | "crashed" | "quarantined")
}

/// Middle value (mean of the two middles for an even count). `v` sorted.
fn median(v: &[f64]) -> Option<f64> {
    let n = v.len();
    match n {
        0 => None,
        _ if n % 2 == 1 => Some(v[n / 2]),
        _ => Some((v[n / 2 - 1] + v[n / 2]) / 2.0),
    }
}

/// Nearest-rank 95th percentile. `v` sorted.
fn p95(v: &[f64]) -> Option<f64> {
    let rank = (v.len() as f64 * 0.95).ceil() as usize;
    v.get(rank.checked_sub(1)?).copied()
}

/// Seconds rounded to 4 places (the report's precision; drops f32 noise).
fn round4(x: f32) -> f64 {
    (f64::from(x) * 1e4).round() / 1e4
}

fn summarize(series: &[(&RunMeta, &Entry)]) -> Option<HistorySummary> {
    if series.is_empty() {
        return None;
    }
    let passing: Vec<&Entry> = series
        .iter()
        .map(|(_, e)| *e)
        .filter(|e| is_pass(&e.outcome))
        .collect();
    let mut call: Vec<f64> = passing.iter().filter_map(|e| e.call.map(round4)).collect();
    let mut setup: Vec<f64> = passing.iter().map(|e| round4(e.setup)).collect();
    call.sort_by(f64::total_cmp);
    setup.sort_by(f64::total_cmp);
    let last_pass_commit = series
        .iter()
        .rev()
        .find(|(_, e)| is_pass(&e.outcome))
        .and_then(|(r, _)| r.commit.clone());
    let first_fail_commit = series
        .iter()
        .rev()
        .take_while(|(_, e)| is_fail(&e.outcome))
        .last()
        .and_then(|(r, _)| r.commit.clone());
    Some(HistorySummary {
        runs: series.len(),
        outcomes: series.iter().map(|(_, e)| e.outcome.clone()).collect(),
        call_median: median(&call),
        call_p95: p95(&call),
        setup_median: median(&setup),
        last_pass_commit,
        first_fail_commit,
    })
}

/// One character per run for the human strip.
fn glyph(outcome: &str) -> char {
    if is_pass(outcome) {
        if outcome == "flaky" {
            '~'
        } else {
            '✓'
        }
    } else if is_fail(outcome) {
        '✗'
    } else {
        '-'
    }
}

/// Read the coverage index straight from the cache dir (schema-checked). `None`
/// when missing / corrupt / stale — a cold index just means no footprint.
fn load_coverage_index() -> Option<CoverageIndex> {
    let bytes = std::fs::read(crate::cache::file(COVERAGE_INDEX_FILE)).ok()?;
    let idx: CoverageIndex = serde_json::from_slice(&bytes).ok()?;
    (idx.schema == COVERAGE_INDEX_SCHEMA).then_some(idx)
}

/// The coverage footprint of `nodeid`: which files it covered and how many lines
/// total. The index is keyed file -> line -> [nodeids], so this inverts it for
/// the one test. `None` when the test covered nothing (or the index is cold).
fn footprint(index: &CoverageIndex, nodeid: &str) -> Option<Coverage> {
    let mut files: BTreeSet<&str> = BTreeSet::new();
    let mut line_count = 0usize;
    for (file, cov) in &index.files {
        for ids in cov.lines.values() {
            if ids.iter().any(|id| id == nodeid) {
                files.insert(file.as_str());
                line_count += 1;
            }
        }
    }
    if files.is_empty() {
        return None;
    }
    Some(Coverage {
        file_count: files.len(),
        line_count,
        files: files.into_iter().map(str::to_string).collect(),
    })
}

/// Gather every cache into one dossier for `nodeid`. Pure over the on-disk
/// caches; `found` is false only when the id is absent from all of them.
fn gather(nodeid: &str) -> ExplainReport {
    let scope = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

    let duration_seconds = crate::scheduling::durations::load().get(nodeid).copied();
    let flakes = crate::reporting::flakes::load().get(nodeid).copied();
    let (green, lines) = crate::coverage_skip::load_raw(&scope);
    let last_outcome = green.contains(nodeid).then_some("passed");
    // The outcomes store keeps pytest's 0-based `location` line; report it
    // 1-based like every other human-facing surface (e.g. CI annotations).
    let source_line = lines.get(nodeid).map(|l| l + 1);
    let coverage = load_coverage_index().and_then(|idx| footprint(&idx, nodeid));
    let history = summarize(&crate::reporting::history::load().series(nodeid));

    let found = duration_seconds.is_some()
        || flakes.is_some()
        || last_outcome.is_some()
        || source_line.is_some()
        || coverage.is_some()
        || history.is_some();

    ExplainReport {
        meta: Meta {
            runner: "rstest",
            kind: "explain",
            schema: SCHEMA,
            rstest_version: env!("CARGO_PKG_VERSION"),
        },
        nodeid: nodeid.to_string(),
        found,
        duration_seconds,
        last_outcome,
        source_line,
        flakes,
        coverage,
        history,
    }
}

/// Up to `n` cached nodeids that contain `needle` as a substring, sorted — the
/// "did you mean" list when an exact id matched nothing. Unions the keys of the
/// duration and flake caches (the two suite-wide ones every project has).
fn suggestions(needle: &str, n: usize) -> Vec<String> {
    let mut hits: BTreeSet<String> = BTreeSet::new();
    for id in crate::scheduling::durations::load().into_keys() {
        if id.contains(needle) {
            hits.insert(id);
        }
    }
    for id in crate::reporting::flakes::load().into_keys() {
        if id.contains(needle) {
            hits.insert(id);
        }
    }
    hits.into_iter().take(n).collect()
}

/// Render an epoch as a coarse "Nd ago" / "Nh ago" relative to `now`. A future
/// or zeroed epoch (clock skew) reads as "just now" rather than a negative age.
fn ago(epoch: u64, now: u64) -> String {
    let secs = now.saturating_sub(epoch);
    if secs < 60 {
        "just now".to_string()
    } else if secs < 60 * 60 {
        format!("{}m ago", secs / 60)
    } else if secs < 24 * 60 * 60 {
        format!("{}h ago", secs / (60 * 60))
    } else {
        format!("{}d ago", secs / (24 * 60 * 60))
    }
}

pub fn run_explain(sink: &mut Sink, nodeid: &str, json: bool) -> Result<i32> {
    let report = gather(nodeid);
    if json {
        let mut bytes = serde_json::to_vec_pretty(&report)?;
        bytes.push(b'\n');
        sink.out().write_all(&bytes)?;
        // A missing test is not an error for tooling (it may probe ids); the
        // `found` flag carries that, and the exit code stays 0 with --json.
        return Ok(0);
    }
    render(sink, &report);
    // Human mode: a nodeid the caches have never heard of exits non-zero so a
    // typo'd id in a shell one-liner is noticed.
    Ok(if report.found { 0 } else { 1 })
}

fn render(sink: &mut Sink, r: &ExplainReport) {
    let pal = sink.palette();
    if !r.found {
        render_not_found(sink, pal, &r.nodeid);
        return;
    }
    sink.out_line(&format!("test: {}", r.nodeid));
    if let Some(line) = r.source_line {
        sink.out_line(&pal.dim(&format!("  {}:{line}", crate::text::nodeid_file(&r.nodeid))));
    }
    sink.out_line("");

    // Duration.
    match r.duration_seconds {
        Some(d) => sink.out_line(&format!("  duration   {d:.4}s (last recorded)")),
        None => sink.out_line(&format!("  duration   {}", pal.dim("no cached timing"))),
    }

    // Outcome.
    match r.last_outcome {
        Some(_) => sink.out_line(&format!(
            "  outcome    {} last incremental run",
            pal.green("passed")
        )),
        None => sink.out_line(&format!("  outcome    {}", pal.dim("no last-green record"))),
    }

    // Flake / fail history.
    match &r.flakes {
        Some(f) => {
            let now = crate::time::now_epoch_secs();
            let mut parts = Vec::new();
            if f.flaky > 0 {
                parts.push(pal.yellow(&format!("flaked {}x", f.flaky)));
            }
            if f.failed > 0 {
                parts.push(pal.red(&format!("failed {}x", f.failed)));
            }
            if parts.is_empty() {
                parts.push("no events".to_string());
            }
            sink.out_line(&format!(
                "  flakes     {} (last {})",
                parts.join(", "),
                ago(f.last_epoch, now),
            ));
        }
        None => sink.out_line(&format!(
            "  flakes     {}",
            pal.dim("no flake/fail history")
        )),
    }

    // Recent runs.
    match &r.history {
        Some(h) => {
            let strip: String = h.outcomes.iter().map(|o| glyph(o)).collect();
            sink.out_line(&format!(
                "  history    {strip} ({} runs, oldest first)",
                h.runs
            ));
            if let (Some(m), Some(p)) = (h.call_median, h.call_p95) {
                sink.out_line(&format!("             call median {m:.4}s, p95 {p:.4}s"));
            }
            let short = |c: &str| c.chars().take(10).collect::<String>();
            if let Some(c) = &h.last_pass_commit {
                sink.out_line(&format!("             last passed at {}", short(c)));
            }
            if let Some(c) = &h.first_fail_commit {
                sink.out_line(&format!(
                    "             {} since {}",
                    pal.red("failing"),
                    short(c)
                ));
            }
        }
        None => sink.out_line(&format!("  history    {}", pal.dim("no recorded runs"))),
    }

    // Coverage footprint.
    match &r.coverage {
        Some(c) => {
            sink.out_line(&format!(
                "  coverage   covers {} file(s), {} line(s):",
                c.file_count, c.line_count,
            ));
            for f in &c.files {
                sink.out_line(&format!("               {f}"));
            }
        }
        None => sink.out_line(&format!(
            "  coverage   {}",
            pal.dim("cold — run with --cov-context=test to populate the index"),
        )),
    }
}

fn render_not_found(sink: &mut Sink, pal: Palette, nodeid: &str) {
    sink.warn(&format!(
        "{} explain: no cached data for {nodeid}",
        pal.outcome("FAILED"),
    ));
    let hints = suggestions(nodeid, 5);
    if !hints.is_empty() {
        sink.warn("  did you mean:");
        for h in &hints {
            sink.warn(&format!("    {h}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::select::CoverageFile;
    use std::collections::HashMap;

    fn index_with(nodeid: &str) -> CoverageIndex {
        let mut lines: HashMap<u32, Vec<String>> = HashMap::new();
        lines.insert(1, vec![nodeid.to_string(), "other::t".to_string()]);
        lines.insert(2, vec![nodeid.to_string()]);
        let mut files = HashMap::new();
        files.insert(
            "src/a.py".to_string(),
            CoverageFile {
                hash: "h".to_string(),
                lines,
            },
        );
        // A second file the test never touched.
        let mut other = HashMap::new();
        other.insert(9, vec!["other::t".to_string()]);
        files.insert(
            "src/b.py".to_string(),
            CoverageFile {
                hash: "h2".to_string(),
                lines: other,
            },
        );
        CoverageIndex {
            schema: COVERAGE_INDEX_SCHEMA,
            files,
        }
    }

    #[test]
    fn footprint_inverts_index_for_one_test() {
        let idx = index_with("t/x.py::test_a");
        let cov = footprint(&idx, "t/x.py::test_a").expect("covers something");
        assert_eq!(cov.file_count, 1);
        assert_eq!(cov.files, vec!["src/a.py".to_string()]);
        // Two lines in src/a.py carried this nodeid.
        assert_eq!(cov.line_count, 2);
    }

    #[test]
    fn footprint_none_when_test_covers_nothing() {
        let idx = index_with("t/x.py::test_a");
        assert!(footprint(&idx, "t/x.py::test_absent").is_none());
    }

    #[test]
    fn ago_buckets() {
        let now = 100 * 24 * 60 * 60;
        assert_eq!(ago(now, now), "just now");
        assert_eq!(ago(now - 120, now), "2m ago");
        assert_eq!(ago(now - 3 * 60 * 60, now), "3h ago");
        assert_eq!(ago(now - 5 * 24 * 60 * 60, now), "5d ago");
        // Future epoch (skew) never goes negative.
        assert_eq!(ago(now + 999, now), "just now");
    }

    #[test]
    fn ago_bucket_boundaries() {
        let now = 100 * 24 * 60 * 60;
        assert_eq!(ago(now - 59, now), "just now");
        assert_eq!(ago(now - 60, now), "1m ago");
        assert_eq!(ago(now - (60 * 60 - 1), now), "59m ago");
        assert_eq!(ago(now - 60 * 60, now), "1h ago");
        assert_eq!(ago(now - (24 * 60 * 60 - 1), now), "23h ago");
        assert_eq!(ago(now - 24 * 60 * 60, now), "1d ago");
        // A zeroed epoch is a real (huge) age, not a panic.
        assert_eq!(ago(0, now), "100d ago");
    }

    #[test]
    fn footprint_counts_lines_across_files_and_sorts_them() {
        let id = "t/x.py::test_a";
        let mut idx = index_with(id);
        let mut lines = HashMap::new();
        lines.insert(3, vec![id.to_string()]);
        lines.insert(4, vec!["other::t".to_string()]);
        idx.files.insert(
            "src/0_first.py".to_string(),
            CoverageFile {
                hash: "h3".to_string(),
                lines,
            },
        );
        let cov = footprint(&idx, id).expect("covers something");
        assert_eq!(cov.file_count, 2);
        assert_eq!(cov.line_count, 3);
        assert_eq!(cov.files, vec!["src/0_first.py", "src/a.py"]);
    }

    #[test]
    fn footprint_matches_nodeid_exactly_not_by_prefix() {
        // `test_a` must not claim lines covered by `test_a[1]` or `test_ab`.
        let mut idx = index_with("t/x.py::test_a[1]");
        idx.files
            .get_mut("src/b.py")
            .unwrap()
            .lines
            .insert(10, vec!["t/x.py::test_ab".to_string()]);
        assert!(footprint(&idx, "t/x.py::test_a").is_none());
    }

    #[test]
    fn footprint_none_on_empty_index() {
        let idx = CoverageIndex {
            schema: COVERAGE_INDEX_SCHEMA,
            files: HashMap::new(),
        };
        assert!(footprint(&idx, "t/x.py::test_a").is_none());
    }

    fn report(nodeid: &str) -> ExplainReport {
        ExplainReport {
            meta: Meta {
                runner: "rstest",
                kind: "explain",
                schema: SCHEMA,
                rstest_version: env!("CARGO_PKG_VERSION"),
            },
            nodeid: nodeid.to_string(),
            found: true,
            duration_seconds: None,
            last_outcome: None,
            source_line: None,
            flakes: None,
            coverage: None,
            history: None,
        }
    }

    fn full_report() -> ExplainReport {
        let now = crate::time::now_epoch_secs();
        ExplainReport {
            duration_seconds: Some(0.12345),
            last_outcome: Some("passed"),
            source_line: Some(42),
            flakes: Some(FlakeStats {
                flaky: 2,
                failed: 1,
                last_epoch: now.saturating_sub(3 * 60 * 60),
                last_failed_epoch: 0,
            }),
            coverage: Some(Coverage {
                file_count: 2,
                line_count: 7,
                files: vec!["src/a.py".to_string(), "src/b.py".to_string()],
            }),
            history: Some(HistorySummary {
                runs: 4,
                outcomes: ["passed", "flaky", "failed", "failed"]
                    .map(String::from)
                    .to_vec(),
                call_median: Some(0.1),
                call_p95: Some(0.3),
                setup_median: Some(0.0),
                last_pass_commit: Some("0123456789abcdef".into()),
                first_fail_commit: Some("fedcba9876543210".into()),
            }),
            ..report("tests/test_x.py::TestC::test_m[a::b]")
        }
    }

    fn rendered(r: &ExplainReport) -> (String, String) {
        let (mut sink, cap) = Sink::captured();
        render(&mut sink, r);
        (cap.out(), cap.err())
    }

    #[test]
    fn render_full_dossier() {
        let (out, err) = rendered(&full_report());
        assert!(err.is_empty(), "err={err}");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "test: tests/test_x.py::TestC::test_m[a::b]");
        // The source location uses the file part only (param `::` not split).
        assert_eq!(lines[1], "  tests/test_x.py:42");
        assert_eq!(lines[2], "");
        assert_eq!(lines[3], "  duration   0.1235s (last recorded)");
        assert_eq!(lines[4], "  outcome    passed last incremental run");
        assert_eq!(lines[5], "  flakes     flaked 2x, failed 1x (last 3h ago)");
        assert_eq!(lines[6], "  history    ✓~✗✗ (4 runs, oldest first)");
        assert_eq!(lines[7], "             call median 0.1000s, p95 0.3000s");
        assert_eq!(lines[8], "             last passed at 0123456789");
        assert_eq!(lines[9], "             failing since fedcba9876");
        assert_eq!(lines[10], "  coverage   covers 2 file(s), 7 line(s):");
        assert_eq!(lines[11], "               src/a.py");
        assert_eq!(lines[12], "               src/b.py");
        assert_eq!(lines.len(), 13, "out={out}");
    }

    #[test]
    fn render_placeholders_for_absent_sections() {
        // Found via duration alone: every other section prints its placeholder,
        // and no source-location line appears without a recorded def line.
        let r = ExplainReport {
            duration_seconds: Some(1.0),
            ..report("t/x.py::test_a")
        };
        let (out, _) = rendered(&r);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "test: t/x.py::test_a");
        assert_eq!(lines[1], "");
        assert_eq!(lines[2], "  duration   1.0000s (last recorded)");
        assert_eq!(lines[3], "  outcome    no last-green record");
        assert_eq!(lines[4], "  flakes     no flake/fail history");
        assert_eq!(lines[5], "  history    no recorded runs");
        assert!(lines[6].starts_with("  coverage   cold"), "{}", lines[6]);
        assert!(lines[6].contains("--cov-context=test"));
        assert_eq!(lines.len(), 7, "out={out}");
    }

    #[test]
    fn render_no_cached_timing() {
        let r = ExplainReport {
            last_outcome: Some("passed"),
            ..report("t/x.py::test_a")
        };
        let (out, _) = rendered(&r);
        assert!(out.contains("  duration   no cached timing\n"), "out={out}");
    }

    #[test]
    fn render_flakes_only_failed_or_only_flaky_or_neither() {
        let now = crate::time::now_epoch_secs();
        let flakes = |flaky, failed| ExplainReport {
            flakes: Some(FlakeStats {
                flaky,
                failed,
                last_epoch: now,
                last_failed_epoch: 0,
            }),
            ..report("t/x.py::test_a")
        };
        let (out, _) = rendered(&flakes(0, 4));
        assert!(
            out.contains("  flakes     failed 4x (last just now)\n"),
            "out={out}"
        );
        let (out, _) = rendered(&flakes(3, 0));
        assert!(
            out.contains("  flakes     flaked 3x (last just now)\n"),
            "out={out}"
        );
        // An entry with zeroed counts (legal on disk) still renders sanely.
        let (out, _) = rendered(&flakes(0, 0));
        assert!(
            out.contains("  flakes     no events (last just now)\n"),
            "out={out}"
        );
    }

    #[test]
    fn render_not_found_goes_to_stderr_only() {
        // A needle no real cache could contain, so `suggestions` adds nothing.
        let r = ExplainReport {
            found: false,
            ..report("zz/\u{1f600}_nope.py::never_seen")
        };
        let (out, err) = rendered(&r);
        assert!(out.is_empty(), "out={out}");
        assert!(
            err.contains("explain: no cached data for zz/\u{1f600}_nope.py::never_seen"),
            "err={err}"
        );
        assert!(err.contains("FAILED"), "err={err}");
        assert!(!err.contains("did you mean"), "err={err}");
    }

    #[test]
    fn json_shape_is_stable() {
        let v = serde_json::to_value(full_report()).unwrap();
        assert_eq!(v["meta"]["runner"], "rstest");
        assert_eq!(v["meta"]["kind"], "explain");
        assert_eq!(v["meta"]["schema"], SCHEMA);
        assert_eq!(v["meta"]["rstest_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(v["nodeid"], "tests/test_x.py::TestC::test_m[a::b]");
        assert_eq!(v["found"], true);
        assert_eq!(v["duration_seconds"], 0.12345);
        assert_eq!(v["last_outcome"], "passed");
        assert_eq!(v["source_line"], 42);
        assert_eq!(v["flakes"]["flaky"], 2);
        assert_eq!(v["flakes"]["failed"], 1);
        assert!(v["flakes"]["last_epoch"].is_u64());
        assert_eq!(v["coverage"]["file_count"], 2);
        assert_eq!(v["coverage"]["line_count"], 7);
        assert_eq!(v["coverage"]["files"][1], "src/b.py");
        assert_eq!(v["history"]["runs"], 4);
        assert_eq!(v["history"]["outcomes"][1], "flaky");
        assert_eq!(v["history"]["call_median"], 0.1);
        assert_eq!(v["history"]["call_p95"], 0.3);
        assert_eq!(v["history"]["last_pass_commit"], "0123456789abcdef");
        assert_eq!(v["history"]["first_fail_commit"], "fedcba9876543210");
    }

    #[test]
    fn json_absent_fields_are_null_not_omitted() {
        // Tooling keys on every field existing; absence must serialize as null.
        let v = serde_json::to_value(ExplainReport {
            found: false,
            ..report("t/x.py::test_a")
        })
        .unwrap();
        let obj = v.as_object().unwrap();
        for k in [
            "duration_seconds",
            "last_outcome",
            "source_line",
            "flakes",
            "coverage",
            "history",
        ] {
            assert!(obj.get(k).is_some_and(|x| x.is_null()), "{k}: {v}");
        }
        assert_eq!(v["found"], false);
    }

    fn run(seq: u32, commit: Option<&str>) -> RunMeta {
        RunMeta {
            seq,
            id: format!("r{seq}"),
            epoch: 0,
            commit: commit.map(String::from),
            env: String::new(),
            workers: 1,
        }
    }

    fn ent(outcome: &str, call: Option<f32>) -> Entry {
        Entry {
            run: 0,
            outcome: outcome.into(),
            call,
            setup: 0.01,
        }
    }

    #[test]
    fn summarize_empty_is_none() {
        assert!(summarize(&[]).is_none());
    }

    #[test]
    fn summarize_stats_use_passing_runs_only() {
        let runs: Vec<RunMeta> = (0..5).map(|i| run(i, Some(&format!("c{i}")))).collect();
        let es = [
            ent("passed", Some(0.1)),
            ent("passed", Some(0.3)),
            // A fail-fast call's time must not drag the median down.
            ent("failed", Some(0.0001)),
            ent("flaky", Some(0.2)),
            ent("skipped", None),
        ];
        let series: Vec<_> = runs.iter().zip(es.iter()).collect();
        let h = summarize(&series).unwrap();
        assert_eq!(h.runs, 5);
        assert_eq!(
            h.outcomes,
            ["passed", "passed", "failed", "flaky", "skipped"]
        );
        assert_eq!(h.call_median, Some(0.2));
        assert_eq!(h.call_p95, Some(0.3));
        assert_eq!(h.setup_median, Some(0.01));
        assert_eq!(h.last_pass_commit.as_deref(), Some("c3"));
        // Latest run is not a failure: no failing streak.
        assert_eq!(h.first_fail_commit, None);
    }

    #[test]
    fn summarize_finds_start_of_trailing_failure_streak() {
        let runs: Vec<RunMeta> = (0..5).map(|i| run(i, Some(&format!("c{i}")))).collect();
        let es = [
            ent("failed", None),
            ent("passed", Some(0.1)),
            ent("errors", None),
            ent("crashed", None),
            ent("failed", None),
        ];
        let series: Vec<_> = runs.iter().zip(es.iter()).collect();
        let h = summarize(&series).unwrap();
        assert_eq!(h.last_pass_commit.as_deref(), Some("c1"));
        assert_eq!(h.first_fail_commit.as_deref(), Some("c2"));
        // Never passed: no call stats, no last pass.
        let h = summarize(&series[2..]).unwrap();
        assert_eq!(h.call_median, None);
        assert_eq!(h.call_p95, None);
        assert_eq!(h.last_pass_commit, None);
        assert_eq!(h.first_fail_commit.as_deref(), Some("c2"));
    }

    #[test]
    fn median_and_p95() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[1.0, 2.0, 4.0, 8.0]), Some(3.0));
        assert_eq!(p95(&[]), None);
        assert_eq!(p95(&[5.0]), Some(5.0));
        let v: Vec<f64> = (1..=20).map(f64::from).collect();
        assert_eq!(p95(&v), Some(19.0));
    }

    #[test]
    fn glyphs() {
        let s: String = [
            "passed",
            "flaky",
            "failed",
            "quarantined",
            "skipped",
            "xfailed",
        ]
        .iter()
        .map(|o| glyph(o))
        .collect();
        assert_eq!(s, "✓~✗✗--");
    }
}
