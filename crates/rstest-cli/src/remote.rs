//! Shared-cache backend: segmented merge-on-read.
//!
//! Each run publishes its OWN contribution as an immutable **segment**
//! (`segments/seg-<id>.json`); readers fetch a compacted `base.json` plus every
//! segment and merge them locally into the normal `.rstest_cache` files. No
//! single writer, no compare-and-swap — concurrent shards/PRs each drop a
//! uniquely-named segment and never conflict.
//!
//! Per-artifact merge (from the code map):
//! - **durations**: last value wins per nodeid (segments applied oldest→newest);
//!   good enough for scheduling.
//! - **flakes**: base totals + summed per-run *events* (+1 each), deduped by
//!   segment id via the base's `absorbed` set so a re-pull never double-counts.
//! - (coverage index: union per line — added when that artifact lands here.)
//!
//! This module is pure data + merge logic; transport (Phase 2) and CLI wiring
//! (Phase 3) live above it. Kept unit-testable with no IO.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::cache;
use crate::flakes::FlakeStats;
use crate::report::Run;
use crate::select::{CoverageIndex, COVERAGE_INDEX_FILE, COVERAGE_INDEX_SCHEMA};

pub const SEGMENT_SCHEMA: u32 = 1;
pub const BASE_SCHEMA: u32 = 1;

/// One run's contribution. `id` is unique (also the filename stem) so the same
/// segment folded into a base is recognised and never counted twice.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Segment {
    pub schema: u32,
    pub id: String,
    /// Epoch seconds — orders durations (newest wins) and stamps flake events.
    pub generated_at: u64,
    /// This run's measured per-test durations (nodeid -> seconds).
    #[serde(default)]
    pub durations: HashMap<String, f64>,
    /// This run's flake/failure events (one per affected test, not totals).
    #[serde(default)]
    pub flake_events: Vec<FlakeEvent>,
    /// This run's coverage-index slice (line->test map, hash-stamped per file).
    /// Empty for non-coverage runs; pre-coverage segments deserialize to empty.
    #[serde(default)]
    pub cov_index: CoverageIndex,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FlakeKind {
    /// Passed only after rerun(s).
    Flaky,
    /// Hard failure (quarantined failures included).
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FlakeEvent {
    pub nodeid: String,
    pub kind: FlakeKind,
}

/// Compacted accumulation: the merged state as of the last `compact`, plus the
/// set of segment ids already folded in (so pull skips them).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Base {
    #[serde(default)]
    pub schema: u32,
    #[serde(default)]
    pub durations: HashMap<String, f64>,
    #[serde(default)]
    pub flakes: HashMap<String, FlakeStats>,
    /// Merged coverage index folded into this base (hash-aware union).
    #[serde(default)]
    pub cov_index: CoverageIndex,
    /// Per-file `generated_at` of the segment that won each coverage file, so a
    /// later merge can compare a new segment against the base's real age (not 0)
    /// on a hash conflict. Remote-only; never written to the local cache.
    #[serde(default)]
    pub cov_ts: HashMap<String, u64>,
    /// Segment ids already accumulated into this base.
    #[serde(default)]
    pub absorbed: HashSet<String>,
}

/// The merged result written into the local `.rstest_cache` on pull.
#[derive(Debug, Default, PartialEq)]
pub struct Merged {
    pub durations: HashMap<String, f64>,
    pub flakes: HashMap<String, FlakeStats>,
    pub cov_index: CoverageIndex,
}

/// Merge a base (optional) and a set of segments into the local cache shape.
/// Segments already absorbed into the base are skipped; the rest are applied
/// oldest→newest so the newest duration wins and flake events accumulate over
/// the base totals exactly once. Pure — never writes back to the remote, so
/// repeated pulls of the same inputs are idempotent (the base is fixed; events
/// are re-summed fresh, not compounded, until a `compact` folds them in).
pub fn merge(base: Option<Base>, segments: Vec<Segment>) -> Merged {
    merge_inner(base, segments).0
}

/// Core merge, also returning the per-file coverage winner timestamps so
/// `compact` can persist them in the base (`merge` discards them). Seeds
/// coverage from the base's own `cov_ts` — a base file's real age, not 0 — so a
/// stale, un-absorbed older segment can't overwrite newer base content.
fn merge_inner(base: Option<Base>, segments: Vec<Segment>) -> (Merged, HashMap<String, u64>) {
    let base = base.unwrap_or_default();
    let mut durations = base.durations;
    let mut flakes = base.flakes;
    let mut cov = base.cov_index;
    let mut cov_ts = base.cov_ts;
    // Any base file lacking a recorded timestamp defaults to 0 (pre-cov_ts base).
    for k in cov.files.keys() {
        cov_ts.entry(k.clone()).or_insert(0);
    }

    let mut fresh: Vec<Segment> = segments
        .into_iter()
        .filter(|s| !base.absorbed.contains(&s.id))
        .collect();
    // Order by timestamp, then segment id as a deterministic tiebreak — two
    // shards stamped in the same epoch second must merge the same way
    // regardless of the (filesystem-dependent) order they were listed in.
    fresh.sort_by(|a, b| {
        a.generated_at
            .cmp(&b.generated_at)
            .then_with(|| a.id.cmp(&b.id))
    });

    for seg in fresh {
        for (nodeid, secs) in seg.durations {
            durations.insert(nodeid, secs); // oldest→newest order => newest wins
        }
        for ev in seg.flake_events {
            let e = flakes.entry(ev.nodeid).or_default();
            match ev.kind {
                FlakeKind::Flaky => e.flaky += 1,
                FlakeKind::Failed => e.failed += 1,
            }
            e.last_epoch = e.last_epoch.max(seg.generated_at);
        }
        merge_cov_slice(&mut cov, &mut cov_ts, seg.cov_index, seg.generated_at);
    }
    // Stamp the schema so a written merge loads back as a valid index (the base
    // may have carried schema 0 when no coverage segment ever contributed).
    if !cov.files.is_empty() {
        cov.schema = COVERAGE_INDEX_SCHEMA;
    }
    (
        Merged {
            durations,
            flakes,
            cov_index: cov,
        },
        cov_ts,
    )
}

/// Fold one segment's coverage slice into the accumulator with the hash-aware
/// rule: a file whose hash matches the current winner **unions** its line→test
/// sets (shards of the same run agree on content); a file with a different hash
/// **replaces** the winner only when newer (a content edit makes old line
/// numbers meaningless). `cov_ts` tracks the winning segment's timestamp per
/// file; base entries enter at ts 0.
fn merge_cov_slice(
    cov: &mut CoverageIndex,
    cov_ts: &mut HashMap<String, u64>,
    slice: CoverageIndex,
    at: u64,
) {
    // Skip empty (pre-coverage / non-coverage) and unrecognized-schema slices.
    if slice.schema != COVERAGE_INDEX_SCHEMA {
        return;
    }
    for (path, incoming) in slice.files {
        if incoming.hash.is_empty() {
            continue; // can't vouch for the lines without a content hash
        }
        match cov.files.get_mut(&path) {
            None => {
                cov.files.insert(path.clone(), incoming);
                cov_ts.insert(path, at);
            }
            Some(cur) => {
                let cur_ts = cov_ts.get(&path).copied().unwrap_or(0);
                if incoming.hash == cur.hash {
                    union_lines(&mut cur.lines, incoming.lines);
                    cov_ts.insert(path, cur_ts.max(at));
                } else if at > cur_ts || (at == cur_ts && incoming.hash > cur.hash) {
                    // Different content, newer (or a deterministic tiebreak for
                    // same-timestamp shards): drop the stale lines entirely.
                    *cur = incoming;
                    cov_ts.insert(path, at);
                }
                // else: older/stale content — dropped.
            }
        }
    }
}

/// Union `src` line→nodeid map into `dst`, keeping each line's nodeids sorted
/// and deduped.
fn union_lines(dst: &mut HashMap<u32, Vec<String>>, src: HashMap<u32, Vec<String>>) {
    for (line, ids) in src {
        let slot = dst.entry(line).or_default();
        let mut set: BTreeSet<String> = slot.drain(..).collect();
        set.extend(ids);
        *slot = set.into_iter().collect();
    }
}

/// Fold a base and all fresh segments into a NEW base (compaction). The result
/// carries every input segment id in `absorbed`, so a lingering copy of a
/// folded segment is skipped by a later `merge`/`compact` — compaction need not
/// be atomic against concurrent pushes.
pub fn compact(base: Option<Base>, segments: Vec<Segment>) -> Base {
    let prior_absorbed = base
        .as_ref()
        .map(|b| b.absorbed.clone())
        .unwrap_or_default();
    let seg_ids: HashSet<String> = segments.iter().map(|s| s.id.clone()).collect();
    let (
        Merged {
            durations,
            flakes,
            cov_index,
        },
        cov_ts,
    ) = merge_inner(base, segments);
    let mut absorbed = prior_absorbed;
    absorbed.extend(seg_ids);
    Base {
        schema: BASE_SCHEMA,
        durations,
        flakes,
        cov_index,
        cov_ts,
        absorbed,
    }
}

// ---- Run <-> segment / local-cache bridges ---------------------------------

/// Build this run's segment from the in-memory `Run` — its OWN measured
/// durations and flake/failure events, NOT the merged local cache (pushing the
/// merged state would re-publish everyone else's data). See the plan's
/// "push publishes THIS run's own contribution" note.
pub fn segment_from_run(
    id: String,
    generated_at: u64,
    run: &Run,
    cov_index: CoverageIndex,
) -> Segment {
    let durations = run.durations().map(|(k, v)| (k.clone(), v)).collect();
    let mut flake_events: Vec<FlakeEvent> = run
        .flaky
        .iter()
        .map(|(nodeid, _)| FlakeEvent {
            nodeid: nodeid.clone(),
            kind: FlakeKind::Flaky,
        })
        .collect();
    for nodeid in run.failed_nodeids() {
        flake_events.push(FlakeEvent {
            nodeid: nodeid.clone(),
            kind: FlakeKind::Failed,
        });
    }
    Segment {
        schema: SEGMENT_SCHEMA,
        id,
        generated_at,
        durations,
        flake_events,
        cov_index,
    }
}

/// Read this run's coverage-index slice from the local cache (honors
/// `RSTEST_CACHE`), for `--cache-push` to embed in its segment. Any error or a
/// schema mismatch yields an empty index (a non-coverage run pushes no slice).
pub fn load_local_cov_index() -> CoverageIndex {
    let Ok(bytes) = std::fs::read(cache::file(COVERAGE_INDEX_FILE)) else {
        return CoverageIndex::default();
    };
    match serde_json::from_slice::<CoverageIndex>(&bytes) {
        Ok(idx) if idx.schema == COVERAGE_INDEX_SCHEMA => idx,
        _ => CoverageIndex::default(),
    }
}

/// Write a merged result into the local `.rstest_cache` (durations + flakes +
/// coverage index), so the normal `durations::load` / `flakes::load` /
/// `load_coverage_index` paths pick it up. Durations and flakes are OVERLAID
/// onto the existing local files (remote wins on shared keys) so a pull augments
/// rather than discards local-only history that hasn't been pushed yet. Sparse
/// results are skipped, matching the modules' own behavior.
pub fn write_local(merged: &Merged) {
    if !merged.durations.is_empty() {
        let mut d = crate::durations::load();
        d.extend(merged.durations.iter().map(|(k, v)| (k.clone(), *v)));
        if let Ok(bytes) = serde_json::to_vec(&d) {
            cache::write_atomic(&cache::file(crate::durations::FILE), &bytes);
        }
    }
    if !merged.flakes.is_empty() {
        let mut f = crate::flakes::load();
        for (k, v) in &merged.flakes {
            f.insert(k.clone(), *v);
        }
        if let Ok(bytes) = serde_json::to_vec(&f) {
            cache::write_atomic(&cache::file(crate::flakes::FILE), &bytes);
        }
    }
    // The coverage index is regenerated each run and drives selection off the
    // merged view, so it is replaced (not overlaid) with the pulled union.
    if !merged.cov_index.files.is_empty() {
        if let Ok(bytes) = serde_json::to_vec(&merged.cov_index) {
            cache::write_atomic(&cache::file(COVERAGE_INDEX_FILE), &bytes);
        }
    }
}

// ---- Transport --------------------------------------------------------------

/// The remote as a flat blob store: a single `base.json` plus uniquely-named
/// immutable segments. Deliberately minimal so filesystem/dir today and
/// object-store/HTTP later share one merge layer above.
pub trait Transport {
    fn list_segment_ids(&self) -> Result<Vec<String>>;
    fn read_segment(&self, id: &str) -> Result<Vec<u8>>;
    fn read_base(&self) -> Result<Option<Vec<u8>>>;
    fn write_segment(&self, id: &str, bytes: &[u8]) -> Result<()>;
    fn write_base(&self, bytes: &[u8]) -> Result<()>;
    fn delete_segment(&self, id: &str) -> Result<()>;
}

/// Filesystem/network-dir backend: `<root>/base.json` + `<root>/segments/seg-<id>.json`.
/// Works over a local path, an NFS/EFS mount, or a directory another tool
/// (`aws s3 sync`, `download-artifact`) materializes.
pub struct DirTransport {
    root: PathBuf,
}

impl DirTransport {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
    fn segments_dir(&self) -> PathBuf {
        self.root.join("segments")
    }
    fn segment_path(&self, id: &str) -> PathBuf {
        self.segments_dir().join(format!("seg-{id}.json"))
    }
    fn base_path(&self) -> PathBuf {
        self.root.join("base.json")
    }
}

impl Transport for DirTransport {
    fn list_segment_ids(&self) -> Result<Vec<String>> {
        let dir = self.segments_dir();
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("listing {}", dir.display())),
        };
        let mut ids = Vec::new();
        for entry in rd.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if let Some(id) = name
                    .strip_prefix("seg-")
                    .and_then(|s| s.strip_suffix(".json"))
                {
                    ids.push(id.to_string());
                }
            }
        }
        Ok(ids)
    }
    fn read_segment(&self, id: &str) -> Result<Vec<u8>> {
        let p = self.segment_path(id);
        std::fs::read(&p).with_context(|| format!("reading segment {}", p.display()))
    }
    fn read_base(&self) -> Result<Option<Vec<u8>>> {
        match std::fs::read(self.base_path()) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context("reading base.json"),
        }
    }
    fn write_segment(&self, id: &str, bytes: &[u8]) -> Result<()> {
        cache::write_atomic(&self.segment_path(id), bytes);
        Ok(())
    }
    fn write_base(&self, bytes: &[u8]) -> Result<()> {
        cache::write_atomic(&self.base_path(), bytes);
        Ok(())
    }
    fn delete_segment(&self, id: &str) -> Result<()> {
        match std::fs::remove_file(self.segment_path(id)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).context("deleting segment"),
        }
    }
}

/// Construct a transport from a `--cache-remote` value. Today only a
/// filesystem path / `file://` URL; object-store schemes are a later add.
pub fn transport_for(remote: &str) -> Result<Box<dyn Transport>> {
    let path = remote.strip_prefix("file://").unwrap_or(remote);
    Ok(Box::new(DirTransport::new(path)))
}

// ---- pull / push / compact (IO orchestration over a Transport) --------------

/// Fetch base + every segment and merge. Unreadable segments are warned and
/// skipped (a corrupt blob must not fail the whole pull); a corrupt base is an
/// error (it would silently drop the whole accumulated history).
pub fn pull(t: &dyn Transport) -> Result<Merged> {
    let base = match t.read_base()? {
        Some(bytes) => Some(serde_json::from_slice::<Base>(&bytes).context("parsing base.json")?),
        None => None,
    };
    let mut segments = Vec::new();
    for id in t.list_segment_ids()? {
        match t.read_segment(&id) {
            Ok(bytes) => match serde_json::from_slice::<Segment>(&bytes) {
                Ok(seg) => segments.push(seg),
                Err(e) => eprintln!("rstest: cache: skipping unreadable segment {id}: {e}"),
            },
            Err(e) => eprintln!("rstest: cache: skipping unreadable segment {id}: {e}"),
        }
    }
    Ok(merge(base, segments))
}

/// Publish one segment (immutable, uniquely named — no read-modify-write).
pub fn push(t: &dyn Transport, seg: &Segment) -> Result<()> {
    // The id becomes a filename (`seg-<id>.json`); a separator or `..` would
    // escape the segments dir or break the list round-trip. Reject rather than
    // silently write somewhere that never lists back.
    if seg.id.is_empty() || seg.id.contains('/') || seg.id.contains('\\') || seg.id.contains("..") {
        anyhow::bail!(
            "invalid segment id {:?}: must be non-empty and free of path separators",
            seg.id
        );
    }
    let bytes = serde_json::to_vec(seg).context("serializing segment")?;
    t.write_segment(&seg.id, &bytes)
}

/// Fold base + all segments into a fresh base, then delete the folded segments.
/// Returns the number of segments folded. Delete failures are non-fatal (the
/// absorbed-id set keeps a lingering segment from double-counting anyway).
pub fn compact_remote(t: &dyn Transport) -> Result<usize> {
    let base = match t.read_base()? {
        Some(bytes) => Some(serde_json::from_slice::<Base>(&bytes).context("parsing base.json")?),
        None => None,
    };
    let ids = t.list_segment_ids()?;
    let mut segments = Vec::new();
    // Only ids we actually parse get folded — and only those get deleted, so a
    // corrupt / truncated / future-schema segment is left in place rather than
    // destroyed without ever being folded into the base.
    let mut folded_ids = Vec::new();
    for id in &ids {
        if let Ok(bytes) = t.read_segment(id) {
            if let Ok(seg) = serde_json::from_slice::<Segment>(&bytes) {
                segments.push(seg);
                folded_ids.push(id.clone());
            } else {
                eprintln!("rstest: cache: compact: keeping unparseable segment {id}");
            }
        }
    }
    let folded = segments.len();
    let new_base = compact(base, segments);
    t.write_base(&serde_json::to_vec(&new_base).context("serializing base.json")?)?;
    for id in &folded_ids {
        let _ = t.delete_segment(id);
    }
    Ok(folded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::select::CoverageFile;

    fn seg(id: &str, at: u64, durs: &[(&str, f64)], events: &[(&str, FlakeKind)]) -> Segment {
        Segment {
            schema: SEGMENT_SCHEMA,
            id: id.into(),
            generated_at: at,
            durations: durs.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            flake_events: events
                .iter()
                .map(|(n, k)| FlakeEvent {
                    nodeid: n.to_string(),
                    kind: *k,
                })
                .collect(),
            cov_index: CoverageIndex::default(),
        }
    }

    /// One file's coverage in a test: (path, hash, &[(line, &[nodeid])]).
    type CovFileSpec<'a> = (&'a str, &'a str, &'a [(u32, &'a [&'a str])]);

    /// Build a coverage-carrying segment from per-file specs.
    fn cov_seg(id: &str, at: u64, files: &[CovFileSpec]) -> Segment {
        let mut idx = CoverageIndex {
            schema: COVERAGE_INDEX_SCHEMA,
            files: HashMap::new(),
        };
        for (path, hash, lines) in files {
            let lm = lines
                .iter()
                .map(|(ln, ids)| (*ln, ids.iter().map(|s| s.to_string()).collect()))
                .collect();
            idx.files.insert(
                path.to_string(),
                CoverageFile {
                    hash: hash.to_string(),
                    lines: lm,
                },
            );
        }
        Segment {
            schema: SEGMENT_SCHEMA,
            id: id.into(),
            generated_at: at,
            durations: HashMap::new(),
            flake_events: Vec::new(),
            cov_index: idx,
        }
    }

    #[test]
    fn durations_newest_wins_across_segments() {
        // Two shards time the same test; the newer generated_at should win.
        let a = seg("s1", 100, &[("t::x", 1.0)], &[]);
        let b = seg("s2", 200, &[("t::x", 3.0), ("t::y", 2.0)], &[]);
        // Pass out of order to prove ordering is by generated_at, not arg order.
        let m = merge(None, vec![b, a]);
        assert_eq!(m.durations.get("t::x"), Some(&3.0));
        assert_eq!(m.durations.get("t::y"), Some(&2.0));
    }

    #[test]
    fn flake_events_accumulate_over_segments() {
        let a = seg("s1", 10, &[], &[("t::f", FlakeKind::Flaky)]);
        let b = seg(
            "s2",
            20,
            &[],
            &[("t::f", FlakeKind::Flaky), ("t::g", FlakeKind::Failed)],
        );
        let m = merge(None, vec![a, b]);
        let f = m.flakes.get("t::f").unwrap();
        assert_eq!((f.flaky, f.failed), (2, 0));
        assert_eq!(f.last_epoch, 20);
        let g = m.flakes.get("t::g").unwrap();
        assert_eq!((g.flaky, g.failed), (0, 1));
    }

    #[test]
    fn absorbed_segments_are_not_double_counted() {
        // s1 already folded into base; presenting it again must not re-add.
        let base = compact(
            None,
            vec![seg(
                "s1",
                10,
                &[("t::x", 1.0)],
                &[("t::f", FlakeKind::Flaky)],
            )],
        );
        assert!(base.absorbed.contains("s1"));
        let m = merge(
            Some(base.clone()),
            vec![
                seg("s1", 10, &[("t::x", 1.0)], &[("t::f", FlakeKind::Flaky)]), // re-presented
                seg("s2", 20, &[], &[("t::f", FlakeKind::Flaky)]),              // new
            ],
        );
        // f counted once from base (s1) + once from s2 = 2, NOT 3.
        assert_eq!(m.flakes.get("t::f").unwrap().flaky, 2);
    }

    #[test]
    fn pull_is_idempotent_across_repeated_merges() {
        let segs = vec![
            seg("s1", 10, &[("t::x", 1.0)], &[("t::f", FlakeKind::Flaky)]),
            seg("s2", 20, &[("t::x", 2.0)], &[("t::f", FlakeKind::Failed)]),
        ];
        let m1 = merge(None, segs.clone());
        let m2 = merge(None, segs);
        assert_eq!(m1, m2); // same inputs -> same output, no compounding
        assert_eq!(m1.durations.get("t::x"), Some(&2.0));
        let f = m1.flakes.get("t::f").unwrap();
        assert_eq!((f.flaky, f.failed), (1, 1));
    }

    #[test]
    fn compact_folds_base_plus_segments_and_marks_absorbed() {
        let base = compact(None, vec![seg("s1", 10, &[("t::x", 1.0)], &[])]);
        let base2 = compact(
            Some(base),
            vec![seg(
                "s2",
                20,
                &[("t::x", 5.0)],
                &[("t::f", FlakeKind::Flaky)],
            )],
        );
        assert_eq!(base2.durations.get("t::x"), Some(&5.0));
        assert_eq!(base2.flakes.get("t::f").unwrap().flaky, 1);
        assert!(base2.absorbed.contains("s1") && base2.absorbed.contains("s2"));
    }

    // ---- coverage-index merge ----------------------------------------------

    fn cov_lines(m: &Merged, path: &str, line: u32) -> Vec<String> {
        m.cov_index
            .files
            .get(path)
            .and_then(|f| f.lines.get(&line))
            .cloned()
            .unwrap_or_default()
    }

    #[test]
    fn cov_same_hash_unions_lines_across_shards() {
        // Two shards of ONE run (same hash H1) each cover different lines/tests
        // of the same file; their line->test maps must union.
        let a = cov_seg("shard1", 10, &[("mod.py", "H1", &[(1, &["t::a"])])]);
        let b = cov_seg(
            "shard2",
            10,
            &[("mod.py", "H1", &[(1, &["t::b"]), (2, &["t::c"])])],
        );
        let m = merge(None, vec![a, b]);
        assert_eq!(cov_lines(&m, "mod.py", 1), vec!["t::a", "t::b"]); // sorted+deduped
        assert_eq!(cov_lines(&m, "mod.py", 2), vec!["t::c"]);
        assert_eq!(m.cov_index.schema, COVERAGE_INDEX_SCHEMA);
    }

    #[test]
    fn cov_different_hash_newest_wins_dropping_stale() {
        // File edited between runs: H1@10 then H2@20 for the same path. Only the
        // newer content's lines survive; the stale H1 lines are dropped.
        let old = cov_seg("s1", 10, &[("mod.py", "H1", &[(1, &["t::old"])])]);
        let new = cov_seg("s2", 20, &[("mod.py", "H2", &[(5, &["t::new"])])]);
        let m = merge(None, vec![new, old]); // out of order: ordering is by generated_at
        assert_eq!(m.cov_index.files.get("mod.py").unwrap().hash, "H2");
        assert!(cov_lines(&m, "mod.py", 1).is_empty()); // stale gone
        assert_eq!(cov_lines(&m, "mod.py", 5), vec!["t::new"]);
    }

    #[test]
    fn cov_pull_is_idempotent() {
        let segs = vec![
            cov_seg("s1", 10, &[("mod.py", "H1", &[(1, &["t::a"])])]),
            cov_seg("s2", 20, &[("mod.py", "H2", &[(2, &["t::b"])])]),
        ];
        assert_eq!(merge(None, segs.clone()), merge(None, segs));
    }

    #[test]
    fn cov_absorbed_segment_not_reapplied_after_compact() {
        // Fold s1 (H2) into a base, then re-present s1 alongside an older s0 (H1).
        // The base already holds H2@10; the re-presented s1 is skipped (absorbed)
        // and the older H1 must NOT overwrite the newer base content.
        let base = compact(
            None,
            vec![cov_seg("s1", 10, &[("mod.py", "H2", &[(2, &["t::b"])])])],
        );
        assert!(base.cov_index.files.contains_key("mod.py"));
        let m = merge(
            Some(base),
            vec![
                cov_seg("s1", 10, &[("mod.py", "H2", &[(2, &["t::b"])])]), // re-presented
                cov_seg("s0", 5, &[("mod.py", "H1", &[(1, &["t::a"])])]),  // older, different hash
            ],
        );
        // Base (H2) wins over the older H1 segment; H1 line dropped, absorbed s1 not doubled.
        assert_eq!(m.cov_index.files.get("mod.py").unwrap().hash, "H2");
        assert_eq!(cov_lines(&m, "mod.py", 2), vec!["t::b"]);
        assert!(cov_lines(&m, "mod.py", 1).is_empty());
    }

    #[test]
    fn cov_base_baseline_unions_same_hash_segment() {
        // A compacted base carries mod.py@H1; a later segment with the SAME hash
        // adds a new line -> the base's lines accumulate rather than reset.
        let base = compact(
            None,
            vec![cov_seg("s1", 10, &[("mod.py", "H1", &[(1, &["t::a"])])])],
        );
        let m = merge(
            Some(base),
            vec![cov_seg("s2", 20, &[("mod.py", "H1", &[(2, &["t::b"])])])],
        );
        assert_eq!(cov_lines(&m, "mod.py", 1), vec!["t::a"]);
        assert_eq!(cov_lines(&m, "mod.py", 2), vec!["t::b"]);
    }

    #[test]
    fn cov_empty_and_pre_coverage_segments_contribute_nothing() {
        // A pre-coverage segment (no cov_index / schema 0) and an empty-hash file
        // are both ignored; a plain durations/flakes segment carries no coverage.
        let pre = seg("s1", 10, &[("t::x", 1.0)], &[]); // cov_index default (schema 0)
        let empty_hash = cov_seg("s2", 20, &[("mod.py", "", &[(1, &["t::a"])])]);
        let m = merge(None, vec![pre, empty_hash]);
        assert!(m.cov_index.files.is_empty());
    }

    #[test]
    fn cov_pre_coverage_segment_json_deserializes() {
        // A segment serialized before the cov_index field existed still parses,
        // defaulting to an empty index (backward compatibility, no schema bump).
        let json = r#"{"schema":1,"id":"old","generated_at":1,"durations":{"t::x":1.0},"flake_events":[]}"#;
        let s: Segment = serde_json::from_str(json).unwrap();
        assert_eq!(s.cov_index, CoverageIndex::default());
        assert_eq!(s.durations.get("t::x"), Some(&1.0));
    }

    fn tmp_dir(label: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rstest-remote-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn dir_transport_push_list_pull_roundtrip() {
        let root = tmp_dir("roundtrip");
        let t = DirTransport::new(&root);
        // Empty remote -> empty merge.
        assert_eq!(pull(&t).unwrap(), Merged::default());
        // Two "shards" each push their own segment.
        push(
            &t,
            &seg(
                "shard1",
                10,
                &[("t::a", 1.0)],
                &[("t::f", FlakeKind::Flaky)],
            ),
        )
        .unwrap();
        push(
            &t,
            &seg(
                "shard2",
                20,
                &[("t::b", 2.0)],
                &[("t::f", FlakeKind::Failed)],
            ),
        )
        .unwrap();
        let mut ids = t.list_segment_ids().unwrap();
        ids.sort();
        assert_eq!(ids, vec!["shard1".to_string(), "shard2".to_string()]);
        // A third job pulls the merged union.
        let m = pull(&t).unwrap();
        assert_eq!(m.durations.get("t::a"), Some(&1.0));
        assert_eq!(m.durations.get("t::b"), Some(&2.0));
        let f = m.flakes.get("t::f").unwrap();
        assert_eq!((f.flaky, f.failed), (1, 1));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn compact_remote_writes_base_deletes_segments_no_double_count() {
        let root = tmp_dir("compact");
        let t = DirTransport::new(&root);
        push(
            &t,
            &seg("s1", 10, &[("t::a", 1.0)], &[("t::f", FlakeKind::Flaky)]),
        )
        .unwrap();
        push(
            &t,
            &seg("s2", 20, &[("t::a", 3.0)], &[("t::f", FlakeKind::Flaky)]),
        )
        .unwrap();
        assert_eq!(compact_remote(&t).unwrap(), 2);
        // Segments gone, base present.
        assert!(t.list_segment_ids().unwrap().is_empty());
        assert!(t.read_base().unwrap().is_some());
        // Pull off the base alone reproduces the merged state.
        let m = pull(&t).unwrap();
        assert_eq!(m.durations.get("t::a"), Some(&3.0));
        assert_eq!(m.flakes.get("t::f").unwrap().flaky, 2);
        // A lingering copy of an already-folded segment must not double-count.
        push(
            &t,
            &seg("s1", 10, &[("t::a", 1.0)], &[("t::f", FlakeKind::Flaky)]),
        )
        .unwrap();
        assert_eq!(pull(&t).unwrap().flakes.get("t::f").unwrap().flaky, 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn dir_transport_coverage_slices_union_on_pull() {
        // Two shards push partial coverage slices (same file+hash, disjoint
        // lines); a pull merges them into the full line->test index — the whole
        // point of folding coverage into the shared cache.
        let root = tmp_dir("cov-roundtrip");
        let t = DirTransport::new(&root);
        push(
            &t,
            &cov_seg("shard1", 10, &[("mod.py", "H1", &[(1, &["t::a"])])]),
        )
        .unwrap();
        push(
            &t,
            &cov_seg("shard2", 10, &[("mod.py", "H1", &[(2, &["t::b"])])]),
        )
        .unwrap();
        let m = pull(&t).unwrap();
        assert_eq!(cov_lines(&m, "mod.py", 1), vec!["t::a"]);
        assert_eq!(cov_lines(&m, "mod.py", 2), vec!["t::b"]);
        // Compaction folds the slices into the base and survives a re-pull.
        assert_eq!(compact_remote(&t).unwrap(), 2);
        let m2 = pull(&t).unwrap();
        assert_eq!(cov_lines(&m2, "mod.py", 1), vec!["t::a"]);
        assert_eq!(cov_lines(&m2, "mod.py", 2), vec!["t::b"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn compact_remote_keeps_unparseable_segments() {
        // A corrupt segment must not be folded-then-deleted (that would destroy
        // data never folded into the base). It survives; valid ones fold + prune.
        let root = tmp_dir("compact-corrupt");
        let t = DirTransport::new(&root);
        push(&t, &seg("good", 10, &[("t::a", 1.0)], &[])).unwrap();
        let corrupt = root.join("segments").join("seg-bad.json");
        std::fs::create_dir_all(corrupt.parent().unwrap()).unwrap();
        std::fs::write(&corrupt, b"{ not json").unwrap();
        assert_eq!(compact_remote(&t).unwrap(), 1); // only "good" folded
        assert!(corrupt.exists(), "unparseable segment must be kept");
        assert_eq!(t.list_segment_ids().unwrap(), vec!["bad".to_string()]);
        assert_eq!(pull(&t).unwrap().durations.get("t::a"), Some(&1.0));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn durations_equal_timestamp_deterministic_by_id() {
        // Same nodeid + same generated_at, different value/id: higher id applies
        // last and wins, independent of input (filesystem-list) order.
        let a = seg("id-a", 100, &[("t::x", 1.0)], &[]);
        let b = seg("id-b", 100, &[("t::x", 2.0)], &[]);
        let m1 = merge(None, vec![a.clone(), b.clone()]);
        let m2 = merge(None, vec![b, a]);
        assert_eq!(m1.durations.get("t::x"), Some(&2.0));
        assert_eq!(m1, m2);
    }

    #[test]
    fn push_rejects_unsafe_segment_id() {
        let root = tmp_dir("bad-id");
        let t = DirTransport::new(&root);
        assert!(push(&t, &seg("../escape", 1, &[], &[])).is_err());
        assert!(push(&t, &seg("a/b", 1, &[], &[])).is_err());
        assert!(push(&t, &seg("a\\b", 1, &[], &[])).is_err());
        assert!(push(&t, &seg("", 1, &[], &[])).is_err());
        assert!(push(&t, &seg("run1-1of2", 1, &[("t::a", 1.0)], &[])).is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }
}
