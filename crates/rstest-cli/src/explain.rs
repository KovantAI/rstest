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
//! the last incremental run (plus its source line), and its coverage footprint
//! (which files it covered). Not persisted locally yet — duration *history* /
//! variance, an ordered last-N-outcomes series, and per-test fixture cost — so
//! `explain` reports what exists and grows richer as more per-test data accrues.

use std::collections::BTreeSet;

use anyhow::Result;
use serde::Serialize;

use crate::reporting::color::Palette;
use crate::reporting::flakes::FlakeStats;
use crate::reporting::sink::Sink;
use crate::select::{CoverageIndex, COVERAGE_INDEX_FILE, COVERAGE_INDEX_SCHEMA};

const SCHEMA: u32 = 1;

#[derive(Serialize)]
struct Meta {
    runner: &'static str,
    kind: &'static str,
    schema: u32,
    rstest_version: &'static str,
}

/// The coverage footprint of one test: the source files it covered and the
/// total number of covered lines across them.
#[derive(Serialize)]
struct Coverage {
    file_count: usize,
    line_count: usize,
    /// Covered source files, sorted, cwd-relative (the coverage-index keys).
    files: Vec<String>,
}

/// The merged per-nodeid dossier. `null` fields are simply absent from every
/// cache (e.g. a never-flaked test has no `flakes` entry). `found` is false when
/// the nodeid appears in no cache at all.
#[derive(Serialize)]
struct ExplainReport {
    meta: Meta,
    nodeid: String,
    found: bool,
    /// Last recorded call-phase duration in seconds (latest value only; local
    /// caches keep no history).
    duration_seconds: Option<f64>,
    /// `"passed"` if the test was green on the last incremental run; `null`
    /// otherwise (absence is not proof of failure — see `flakes` for fail
    /// history).
    last_outcome: Option<&'static str>,
    /// Source def line recorded on the last incremental run, if known.
    source_line: Option<u64>,
    /// Cross-run flake/fail counts + last-event epoch, if the test has any.
    flakes: Option<FlakeStats>,
    /// Files this test covered, if a warm coverage index has it.
    coverage: Option<Coverage>,
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
    let source_line = lines.get(nodeid).copied();
    let coverage = load_coverage_index().and_then(|idx| footprint(&idx, nodeid));

    let found = duration_seconds.is_some()
        || flakes.is_some()
        || last_outcome.is_some()
        || source_line.is_some()
        || coverage.is_some();

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
}
