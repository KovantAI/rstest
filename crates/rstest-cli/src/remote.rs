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

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::cache;
use crate::flakes::FlakeStats;
use crate::report::Run;

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
    /// Segment ids already accumulated into this base.
    #[serde(default)]
    pub absorbed: HashSet<String>,
}

/// The merged result written into the local `.rstest_cache` on pull.
#[derive(Debug, Default, PartialEq)]
pub struct Merged {
    pub durations: HashMap<String, f64>,
    pub flakes: HashMap<String, FlakeStats>,
}

/// Merge a base (optional) and a set of segments into the local cache shape.
/// Segments already absorbed into the base are skipped; the rest are applied
/// oldest→newest so the newest duration wins and flake events accumulate over
/// the base totals exactly once. Pure — never writes back to the remote, so
/// repeated pulls of the same inputs are idempotent (the base is fixed; events
/// are re-summed fresh, not compounded, until a `compact` folds them in).
pub fn merge(base: Option<Base>, segments: Vec<Segment>) -> Merged {
    let base = base.unwrap_or_default();
    let mut durations = base.durations;
    let mut flakes = base.flakes;

    let mut fresh: Vec<Segment> = segments
        .into_iter()
        .filter(|s| !base.absorbed.contains(&s.id))
        .collect();
    fresh.sort_by_key(|s| s.generated_at);

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
    }
    Merged { durations, flakes }
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
    let Merged { durations, flakes } = merge(base, segments);
    let mut absorbed = prior_absorbed;
    absorbed.extend(seg_ids);
    Base {
        schema: BASE_SCHEMA,
        durations,
        flakes,
        absorbed,
    }
}

// ---- Run <-> segment / local-cache bridges ---------------------------------

/// Build this run's segment from the in-memory `Run` — its OWN measured
/// durations and flake/failure events, NOT the merged local cache (pushing the
/// merged state would re-publish everyone else's data). See the plan's
/// "push publishes THIS run's own contribution" note.
pub fn segment_from_run(id: String, generated_at: u64, run: &Run) -> Segment {
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
    }
}

/// Write a merged result into the local `.rstest_cache` (durations + flakes),
/// so the normal `durations::load` / `flakes::load` paths pick it up. Sparse
/// files are skipped, matching the modules' own behavior.
pub fn write_local(merged: &Merged) {
    if !merged.durations.is_empty() {
        if let Ok(bytes) = serde_json::to_vec(&merged.durations) {
            cache::write_atomic(&cache::file(crate::durations::FILE), &bytes);
        }
    }
    if !merged.flakes.is_empty() {
        if let Ok(bytes) = serde_json::to_vec(&merged.flakes) {
            cache::write_atomic(&cache::file(crate::flakes::FILE), &bytes);
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
    for id in &ids {
        if let Ok(bytes) = t.read_segment(id) {
            if let Ok(seg) = serde_json::from_slice::<Segment>(&bytes) {
                segments.push(seg);
            }
        }
    }
    let folded = segments.len();
    let new_base = compact(base, segments);
    t.write_base(&serde_json::to_vec(&new_base).context("serializing base.json")?)?;
    for id in &ids {
        let _ = t.delete_segment(id);
    }
    Ok(folded)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
