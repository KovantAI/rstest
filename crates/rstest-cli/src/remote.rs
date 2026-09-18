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
use crate::reporting::flakes::FlakeStats;
use crate::reporting::report::Run;
use crate::reporting::sink::Sink;
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
        let mut d = crate::scheduling::durations::load();
        d.extend(merged.durations.iter().map(|(k, v)| (k.clone(), *v)));
        if let Ok(bytes) = serde_json::to_vec(&d) {
            let _ = cache::write_atomic(&cache::file(crate::scheduling::durations::FILE), &bytes);
        }
    }
    if !merged.flakes.is_empty() {
        let mut f = crate::reporting::flakes::load();
        for (k, v) in &merged.flakes {
            f.insert(k.clone(), *v);
        }
        if let Ok(bytes) = serde_json::to_vec(&f) {
            let _ = cache::write_atomic(&cache::file(crate::reporting::flakes::FILE), &bytes);
        }
    }
    // The coverage index is regenerated each run and drives selection off the
    // merged view, so it is replaced (not overlaid) with the pulled union.
    if !merged.cov_index.files.is_empty() {
        if let Ok(bytes) = serde_json::to_vec(&merged.cov_index) {
            let _ = cache::write_atomic(&cache::file(COVERAGE_INDEX_FILE), &bytes);
        }
    }
}

// ---- Transport --------------------------------------------------------------

/// The remote as a flat blob store: a single `base.json` plus uniquely-named
/// immutable segments. Deliberately minimal so filesystem/dir today and
/// object-store/HTTP later share one merge layer above.
pub trait Transport {
    /// List the ids of all segments currently present on the remote.
    fn list_segment_ids(&self) -> Result<Vec<String>>;
    /// Read one segment's raw bytes by id, or `None` if it is absent (e.g. a
    /// concurrent compaction pruned it between the list and this read). A
    /// transport/auth failure is an `Err`, never `None`, so callers can tell a
    /// benign race from a real error instead of silently dropping history.
    fn read_segment(&self, id: &str) -> Result<Option<Vec<u8>>>;
    /// Read the `base.json` blob, or `None` if the remote has no base yet.
    fn read_base(&self) -> Result<Option<Vec<u8>>>;
    /// Write a new immutable segment under `id` (ids are unique per run, so
    /// concurrent writers never conflict).
    fn write_segment(&self, id: &str, bytes: &[u8]) -> Result<()>;
    /// Overwrite `base.json` (compaction only).
    fn write_base(&self, bytes: &[u8]) -> Result<()>;
    /// Delete a segment by id (compaction, after folding it into base).
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
    fn read_segment(&self, id: &str) -> Result<Option<Vec<u8>>> {
        let p = self.segment_path(id);
        match std::fs::read(&p) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading segment {}", p.display())),
        }
    }
    fn read_base(&self) -> Result<Option<Vec<u8>>> {
        match std::fs::read(self.base_path()) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context("reading base.json"),
        }
    }
    fn write_segment(&self, id: &str, bytes: &[u8]) -> Result<()> {
        let p = self.segment_path(id);
        cache::write_atomic(&p, bytes).with_context(|| format!("writing segment {}", p.display()))
    }
    fn write_base(&self, bytes: &[u8]) -> Result<()> {
        cache::write_atomic(&self.base_path(), bytes).context("writing base.json")
    }
    fn delete_segment(&self, id: &str) -> Result<()> {
        match std::fs::remove_file(self.segment_path(id)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).context("deleting segment"),
        }
    }
}

// ---- CliTransport: s3:// / gs:// via the cloud CLI --------------------------

/// A captured cloud-CLI invocation result. `success` is the process exit
/// status; `stdout` carries object bytes on a read; `stderr` is inspected to
/// tell a missing object from a real (auth/network) failure.
pub struct CliOutput {
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

/// Spawns cloud-CLI subprocesses. Behind a trait so `CliTransport` is
/// unit-testable without a real `aws`/`gcloud` binary or a real bucket.
pub trait CommandRunner {
    /// Run `program args...`, feeding `stdin` when present, and capture the
    /// result. An `Err` is reserved for the spawn itself failing; a non-zero
    /// exit is reported via `CliOutput::success == false`.
    fn run(&self, program: &str, args: &[String], stdin: Option<&[u8]>) -> Result<CliOutput>;
    /// Whether `program` resolves on `PATH` (cheap, no spawn).
    fn program_exists(&self, program: &str) -> bool;
}

/// Real runner over `std::process::Command`.
struct RealRunner;

impl CommandRunner for RealRunner {
    fn run(&self, program: &str, args: &[String], stdin: Option<&[u8]>) -> Result<CliOutput> {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            });
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning `{program}`"))?;
        // Feed stdin on a separate thread so a child that writes to stdout
        // before draining stdin (a payload larger than the pipe buffer) can't
        // deadlock against a blocking write here. Dropping the handle at the
        // end signals EOF. A write error (e.g. the CLI exited early and closed
        // the pipe) surfaces via the child's exit status + stderr, so we don't
        // propagate it separately.
        let writer = stdin.map(|data| {
            let mut si = child.stdin.take().expect("stdin piped");
            let data = data.to_vec();
            std::thread::spawn(move || {
                let _ = si.write_all(&data);
            })
        });
        let out = child
            .wait_with_output()
            .with_context(|| format!("waiting for `{program}`"))?;
        if let Some(w) = writer {
            let _ = w.join();
        }
        Ok(CliOutput {
            success: out.status.success(),
            stdout: out.stdout,
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
    fn program_exists(&self, program: &str) -> bool {
        let Some(paths) = std::env::var_os("PATH") else {
            return false;
        };
        // On Windows the CLIs land as `aws.exe` / `gcloud.cmd` / `gsutil.cmd`, so
        // a bare-name probe would falsely report them absent; try each PATHEXT
        // suffix in addition to the bare name.
        let exts = program_extensions();
        std::env::split_paths(&paths).any(|dir| {
            exts.iter()
                .any(|ext| dir.join(format!("{program}{ext}")).is_file())
        })
    }
}

/// Executable-name suffixes to try when probing `PATH`. On Windows this is the
/// bare name plus each `PATHEXT` entry (`aws` also matches `aws.exe`); on other
/// platforms just the bare name.
fn program_extensions() -> Vec<String> {
    if cfg!(windows) {
        let raw = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
        let mut exts = vec![String::new()];
        exts.extend(
            raw.split(';')
                .filter(|e| !e.is_empty())
                .map(|e| e.to_string()),
        );
        exts
    } else {
        vec![String::new()]
    }
}

#[derive(Clone, Copy)]
enum CloudTool {
    Aws,
    Gcloud,
    Gsutil,
}

/// Object-store transport that drives the cloud CLI already installed and
/// authenticated on the runner (the same tool a hand-rolled `aws s3 sync` used).
/// No SDK, no async runtime — credentials come from the process environment.
pub struct CliTransport {
    tool: CloudTool,
    /// Bucket+prefix root, trailing slash trimmed (e.g. `s3://bucket/prefix`).
    root: String,
    runner: Box<dyn CommandRunner>,
}

/// A read either returned bytes or the object was absent.
enum ReadOutcome {
    Found(Vec<u8>),
    Missing,
}

/// Whether stderr means "no such object" rather than a real failure, across
/// `aws`/`gcloud`/`gsutil`. Compared case-insensitively. A connectivity /
/// auth / permission failure is never "object absent" — it's checked first so
/// its message (which may itself contain "not found") can't mask a real error
/// as a missing object and silently read as an empty cache.
fn looks_like_not_found(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    const HARD: &[&str] = &[
        "could not connect",
        "could not resolve",
        "connection",
        "timed out",
        "timeout",
        "unable to locate credentials",
        "access denied",
        "accessdenied",
        "forbidden",
        "unauthorized",
        "network",
    ];
    if HARD.iter().any(|m| s.contains(m)) {
        return false;
    }
    const MISSING: &[&str] = &[
        "nosuchkey",
        "not found",
        "does not exist",
        "matched no objects",
        "no url matched",
        "no urls matched",
    ];
    MISSING.iter().any(|m| s.contains(m))
}

impl CliTransport {
    /// Build from an `s3://` / `gs://` remote, probing for the needed CLI. For
    /// `gs://`, `gcloud storage` is preferred with a fallback to `gsutil`.
    pub fn for_remote(remote: &str) -> Result<Self> {
        Self::for_remote_with(remote, Box::new(RealRunner))
    }

    fn for_remote_with(remote: &str, runner: Box<dyn CommandRunner>) -> Result<Self> {
        let root = remote.trim_end_matches('/').to_string();
        if remote.starts_with("s3://") {
            if !runner.program_exists("aws") {
                anyhow::bail!("cache remote {remote} needs the `aws` CLI on PATH (authenticated)");
            }
            Ok(Self {
                tool: CloudTool::Aws,
                root,
                runner,
            })
        } else if remote.starts_with("gs://") {
            let tool = if runner.program_exists("gcloud") {
                CloudTool::Gcloud
            } else if runner.program_exists("gsutil") {
                CloudTool::Gsutil
            } else {
                anyhow::bail!(
                    "cache remote {remote} needs the `gcloud` or `gsutil` CLI on PATH (authenticated)"
                );
            };
            Ok(Self { tool, root, runner })
        } else {
            anyhow::bail!("CliTransport: unsupported remote {remote}");
        }
    }

    fn program(&self) -> &str {
        match self.tool {
            CloudTool::Aws => "aws",
            CloudTool::Gcloud => "gcloud",
            CloudTool::Gsutil => "gsutil",
        }
    }

    fn base_url(&self) -> String {
        format!("{}/base.json", self.root)
    }
    fn segment_url(&self, id: &str) -> String {
        format!("{}/segments/seg-{id}.json", self.root)
    }
    fn segments_prefix(&self) -> String {
        format!("{}/segments/", self.root)
    }

    fn owned(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }
    fn list_args(&self, prefix: &str) -> Vec<String> {
        match self.tool {
            CloudTool::Aws => Self::owned(&["s3", "ls", prefix]),
            CloudTool::Gcloud => Self::owned(&["storage", "ls", prefix]),
            CloudTool::Gsutil => Self::owned(&["ls", prefix]),
        }
    }
    fn read_args(&self, url: &str) -> Vec<String> {
        match self.tool {
            CloudTool::Aws => Self::owned(&["s3", "cp", url, "-"]),
            CloudTool::Gcloud => Self::owned(&["storage", "cat", url]),
            CloudTool::Gsutil => Self::owned(&["cat", url]),
        }
    }
    fn write_args(&self, url: &str) -> Vec<String> {
        match self.tool {
            CloudTool::Aws => Self::owned(&["s3", "cp", "-", url]),
            CloudTool::Gcloud => Self::owned(&["storage", "cp", "-", url]),
            CloudTool::Gsutil => Self::owned(&["cp", "-", url]),
        }
    }
    fn delete_args(&self, url: &str) -> Vec<String> {
        match self.tool {
            CloudTool::Aws => Self::owned(&["s3", "rm", url]),
            CloudTool::Gcloud => Self::owned(&["storage", "rm", url]),
            CloudTool::Gsutil => Self::owned(&["rm", url]),
        }
    }

    fn read_url(&self, url: &str) -> Result<ReadOutcome> {
        let out = self
            .runner
            .run(self.program(), &self.read_args(url), None)?;
        if out.success {
            Ok(ReadOutcome::Found(out.stdout))
        } else if looks_like_not_found(&out.stderr) {
            Ok(ReadOutcome::Missing)
        } else {
            anyhow::bail!("reading {url}: {}", out.stderr.trim())
        }
    }
    fn write_url(&self, url: &str, bytes: &[u8]) -> Result<()> {
        let out = self
            .runner
            .run(self.program(), &self.write_args(url), Some(bytes))?;
        if out.success {
            Ok(())
        } else {
            anyhow::bail!("writing {url}: {}", out.stderr.trim())
        }
    }
}

impl Transport for CliTransport {
    fn list_segment_ids(&self) -> Result<Vec<String>> {
        let prefix = self.segments_prefix();
        let out = self
            .runner
            .run(self.program(), &self.list_args(&prefix), None)?;
        if !out.success {
            // An empty prefix lists as either exit-0-empty or a not-found /
            // empty-stderr non-zero (aws `s3 ls` of an absent prefix); treat
            // both as "no segments yet". A real error carries stderr.
            let se = out.stderr.trim();
            if se.is_empty() || looks_like_not_found(se) {
                return Ok(Vec::new());
            }
            anyhow::bail!("listing {prefix}: {se}");
        }
        // Whitespace-tokenize the listing (aws `s3 ls` columns, gcloud/gsutil
        // full URLs) and keep only `seg-<id>.json` basenames — date/size/PRE
        // tokens never match the prefix filter.
        let text = String::from_utf8_lossy(&out.stdout);
        let mut ids = Vec::new();
        for tok in text.split_whitespace() {
            let name = tok.rsplit('/').next().unwrap_or(tok);
            if let Some(id) = name
                .strip_prefix("seg-")
                .and_then(|s| s.strip_suffix(".json"))
            {
                ids.push(id.to_string());
            }
        }
        Ok(ids)
    }
    fn read_segment(&self, id: &str) -> Result<Option<Vec<u8>>> {
        match self.read_url(&self.segment_url(id))? {
            ReadOutcome::Found(b) => Ok(Some(b)),
            ReadOutcome::Missing => Ok(None),
        }
    }
    fn read_base(&self) -> Result<Option<Vec<u8>>> {
        match self.read_url(&self.base_url())? {
            ReadOutcome::Found(b) => Ok(Some(b)),
            ReadOutcome::Missing => Ok(None),
        }
    }
    fn write_segment(&self, id: &str, bytes: &[u8]) -> Result<()> {
        self.write_url(&self.segment_url(id), bytes)
    }
    fn write_base(&self, bytes: &[u8]) -> Result<()> {
        self.write_url(&self.base_url(), bytes)
    }
    fn delete_segment(&self, id: &str) -> Result<()> {
        let url = self.segment_url(id);
        let out = self
            .runner
            .run(self.program(), &self.delete_args(&url), None)?;
        if out.success || looks_like_not_found(&out.stderr) {
            Ok(())
        } else {
            anyhow::bail!("deleting {url}: {}", out.stderr.trim())
        }
    }
}

// ---- HttpTransport: http:// / https:// (feature `http-cache`) ---------------

/// One HTTP response reduced to what the transport needs: status + body. A
/// non-2xx status is data here (e.g. 404 → "no such object"), not an error;
/// only a connect/transport failure surfaces as `Err`.
#[cfg(feature = "http-cache")]
pub struct HttpResp {
    pub status: u16,
    pub body: Vec<u8>,
}

/// GET/PUT/DELETE over HTTP. Behind a trait so `HttpTransport` is unit-testable
/// without a live server; `ureq` lives only in `RealHttpClient`.
#[cfg(feature = "http-cache")]
pub trait HttpClient {
    fn get(&self, url: &str) -> Result<HttpResp>;
    fn put(&self, url: &str, body: &[u8]) -> Result<HttpResp>;
    fn delete(&self, url: &str) -> Result<HttpResp>;
}

/// Shared-cache transport over a plain HTTP(S) endpoint. Reads/writes
/// `<root>/base.json` and `<root>/segments/seg-<id>.json`; **enumeration
/// requires the endpoint to answer `GET <root>/segments/` with a JSON array of
/// names** (filenames or full keys/URLs) — the HTTP listing contract, since
/// bare GET/PUT can't list a collection.
#[cfg(feature = "http-cache")]
pub struct HttpTransport {
    root: String,
    client: Box<dyn HttpClient>,
}

#[cfg(feature = "http-cache")]
impl HttpTransport {
    pub fn new(remote: &str, client: Box<dyn HttpClient>) -> Self {
        Self {
            root: remote.trim_end_matches('/').to_string(),
            client,
        }
    }
    fn base_url(&self) -> String {
        format!("{}/base.json", self.root)
    }
    fn segment_url(&self, id: &str) -> String {
        format!("{}/segments/seg-{id}.json", self.root)
    }
    fn segments_url(&self) -> String {
        format!("{}/segments/", self.root)
    }
}

#[cfg(feature = "http-cache")]
fn is_2xx(status: u16) -> bool {
    (200..300).contains(&status)
}

#[cfg(feature = "http-cache")]
impl Transport for HttpTransport {
    fn list_segment_ids(&self) -> Result<Vec<String>> {
        let url = self.segments_url();
        let r = self.client.get(&url)?;
        if r.status == 404 {
            return Ok(Vec::new());
        }
        if !is_2xx(r.status) {
            anyhow::bail!("listing {url}: HTTP {}", r.status);
        }
        let names: Vec<String> = serde_json::from_slice(&r.body).with_context(|| {
            format!(
                "listing {url}: expected a JSON array of segment names \
                 (the HTTP cache listing contract)"
            )
        })?;
        let mut ids = Vec::new();
        for name in names {
            let base = name.rsplit('/').next().unwrap_or(&name);
            if let Some(id) = base
                .strip_prefix("seg-")
                .and_then(|s| s.strip_suffix(".json"))
            {
                ids.push(id.to_string());
            }
        }
        Ok(ids)
    }
    fn read_segment(&self, id: &str) -> Result<Option<Vec<u8>>> {
        let url = self.segment_url(id);
        let r = self.client.get(&url)?;
        match r.status {
            404 => Ok(None),
            s if is_2xx(s) => Ok(Some(r.body)),
            s => anyhow::bail!("reading {url}: HTTP {s}"),
        }
    }
    fn read_base(&self) -> Result<Option<Vec<u8>>> {
        let url = self.base_url();
        let r = self.client.get(&url)?;
        match r.status {
            404 => Ok(None),
            s if is_2xx(s) => Ok(Some(r.body)),
            s => anyhow::bail!("reading {url}: HTTP {s}"),
        }
    }
    fn write_segment(&self, id: &str, bytes: &[u8]) -> Result<()> {
        let url = self.segment_url(id);
        let r = self.client.put(&url, bytes)?;
        if is_2xx(r.status) {
            Ok(())
        } else {
            anyhow::bail!("writing {url}: HTTP {}", r.status)
        }
    }
    fn write_base(&self, bytes: &[u8]) -> Result<()> {
        let url = self.base_url();
        let r = self.client.put(&url, bytes)?;
        if is_2xx(r.status) {
            Ok(())
        } else {
            anyhow::bail!("writing {url}: HTTP {}", r.status)
        }
    }
    fn delete_segment(&self, id: &str) -> Result<()> {
        let url = self.segment_url(id);
        let r = self.client.delete(&url)?;
        if is_2xx(r.status) || r.status == 404 {
            Ok(())
        } else {
            anyhow::bail!("deleting {url}: HTTP {}", r.status)
        }
    }
}

/// `ureq`-backed client. Bearer auth from `RSTEST_CACHE_REMOTE_TOKEN` (blank =
/// none). Blocking, no async runtime.
#[cfg(feature = "http-cache")]
struct RealHttpClient {
    agent: ureq::Agent,
    auth: Option<String>,
}

#[cfg(feature = "http-cache")]
impl RealHttpClient {
    fn from_env() -> Self {
        let auth = std::env::var("RSTEST_CACHE_REMOTE_TOKEN")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|t| format!("Bearer {t}"));
        Self {
            agent: ureq::agent(),
            auth,
        }
    }
    fn with_auth(&self, req: ureq::Request) -> ureq::Request {
        match &self.auth {
            Some(v) => req.set("Authorization", v),
            None => req,
        }
    }
    /// Execute a request, folding a non-2xx *status* into `HttpResp` (only a
    /// transport-level failure is an `Err`).
    fn exec(req: ureq::Request, body: Option<&[u8]>) -> Result<HttpResp> {
        use std::io::Read;
        let outcome = match body {
            Some(b) => req.send_bytes(b),
            None => req.call(),
        };
        match outcome {
            Ok(resp) => {
                let status = resp.status();
                let mut buf = Vec::new();
                resp.into_reader()
                    .read_to_end(&mut buf)
                    .context("reading HTTP response body")?;
                Ok(HttpResp { status, body: buf })
            }
            Err(ureq::Error::Status(code, resp)) => {
                let mut buf = Vec::new();
                let _ = resp.into_reader().read_to_end(&mut buf);
                Ok(HttpResp {
                    status: code,
                    body: buf,
                })
            }
            Err(e) => Err(anyhow::Error::new(e).context("HTTP request failed")),
        }
    }
}

#[cfg(feature = "http-cache")]
impl HttpClient for RealHttpClient {
    fn get(&self, url: &str) -> Result<HttpResp> {
        Self::exec(self.with_auth(self.agent.get(url)), None)
    }
    fn put(&self, url: &str, body: &[u8]) -> Result<HttpResp> {
        Self::exec(self.with_auth(self.agent.put(url)), Some(body))
    }
    fn delete(&self, url: &str) -> Result<HttpResp> {
        Self::exec(self.with_auth(self.agent.delete(url)), None)
    }
}

#[cfg(feature = "http-cache")]
fn http_transport(remote: &str) -> Result<Box<dyn Transport>> {
    Ok(Box::new(HttpTransport::new(
        remote,
        Box::new(RealHttpClient::from_env()),
    )))
}

#[cfg(not(feature = "http-cache"))]
fn http_transport(remote: &str) -> Result<Box<dyn Transport>> {
    anyhow::bail!(
        "cache remote {remote} needs the `http-cache` build feature, \
         which is not compiled into this binary"
    )
}

// ---- transport selection ----------------------------------------------------

/// Construct a transport from a `--cache-remote` value. A bare path or
/// `file://` is a directory; `s3://` / `gs://` drive the cloud CLI;
/// `http(s)://` uses the HTTP transport (feature `http-cache`); any other
/// `scheme://` is rejected loudly (never silently written to a junk local dir).
pub fn transport_for(remote: &str) -> Result<Box<dyn Transport>> {
    if let Some(path) = remote.strip_prefix("file://") {
        return Ok(Box::new(DirTransport::new(path)));
    }
    match remote.split_once("://") {
        Some(("s3", _)) | Some(("gs", _)) => Ok(Box::new(CliTransport::for_remote(remote)?)),
        Some(("http", _)) | Some(("https", _)) => http_transport(remote),
        Some((scheme, _)) => anyhow::bail!(
            "cache remote scheme `{scheme}://` is not supported \
             (supported: a directory path, `file://`, `s3://`, `gs://`, \
             `http(s)://`); for another object store, materialize a directory \
             first and pass `--cache-remote ./dir`"
        ),
        None => Ok(Box::new(DirTransport::new(remote))),
    }
}

// ---- pull / push / compact (IO orchestration over a Transport) --------------

/// Fetch base + every segment and merge. Unreadable segments are warned and
/// skipped (a corrupt blob must not fail the whole pull); a corrupt base is an
/// error (it would silently drop the whole accumulated history).
pub fn pull(t: &dyn Transport, sink: &mut Sink) -> Result<Merged> {
    let base = match t.read_base()? {
        Some(bytes) => Some(serde_json::from_slice::<Base>(&bytes).context("parsing base.json")?),
        None => None,
    };
    let mut segments = Vec::new();
    for id in t.list_segment_ids()? {
        // A transport/auth error propagates (a flaky remote must fail loudly,
        // not silently yield a partial baseline); `None` (segment pruned by a
        // concurrent compaction mid-pull) and a corrupt blob are skipped.
        if let Some(bytes) = t
            .read_segment(&id)
            .with_context(|| format!("reading segment {id} while pulling shared cache"))?
        {
            match serde_json::from_slice::<Segment>(&bytes) {
                Ok(seg) => segments.push(seg),
                Err(e) => sink.warn(&format!(
                    "rstest: cache: skipping unreadable segment {id}: {e}"
                )),
            }
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

/// Segment retention: which loose segments a compaction keeps *un-folded*.
/// `keep_last`: retain the newest N segments; `max_age`: retain any younger
/// than this many seconds. A segment retained by either rule stays loose;
/// everything else is folded into the base and pruned. Both `None` folds all
/// (the historical behavior).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetentionPolicy {
    pub keep_last: Option<usize>,
    pub max_age: Option<u64>,
}

/// Pure policy: from `(id, generated_at)` pairs, return the ids to FOLD into
/// the base (everything the policy does NOT retain). Recency ranking sorts by
/// `generated_at` desc, then id desc as a deterministic tiebreak, so two
/// same-second segments fold the same way regardless of listing order. No IO
/// / no clock — testable in isolation.
pub fn select_segments_to_fold(
    segs: &[(String, u64)],
    now: u64,
    policy: &RetentionPolicy,
) -> Vec<String> {
    let mut ranked: Vec<&(String, u64)> = segs.iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.0.cmp(&a.0)));
    let keep_n = policy.keep_last.unwrap_or(0);
    let mut fold = Vec::new();
    for (rank, (id, ts)) in ranked.iter().enumerate() {
        let retained_by_count = rank < keep_n;
        // saturating_sub: a future/zeroed timestamp (clock skew) reads as age 0
        // and is retained, never force-folded.
        let retained_by_age = policy
            .max_age
            .is_some_and(|max| now.saturating_sub(*ts) <= max);
        if !(retained_by_count || retained_by_age) {
            fold.push((*id).clone());
        }
    }
    fold
}

/// Fold base + the policy-selected segments into a fresh base, then delete just
/// those folded. Returns `(folded, retained)` counts. `now` stamps the age
/// window (epoch seconds). Delete failures are non-fatal (the absorbed-id set
/// keeps a lingering segment from double-counting anyway).
///
/// Compaction is a non-atomic read-modify-write of `base.json`, so two runners
/// can race (auto-compaction makes this routine). To avoid deleting a segment
/// whose data a concurrent writer dropped, deletes are gated on the *persisted*
/// base: after the write we re-read `base.json` and prune only segments its
/// `absorbed` set actually contains. A segment the winning base didn't absorb
/// stays loose (harmless — `absorbed` prevents double-counting — where deleting
/// it would be data loss).
pub fn compact_remote_with(
    t: &dyn Transport,
    sink: &mut Sink,
    now: u64,
    policy: &RetentionPolicy,
) -> Result<(usize, usize)> {
    let base = match t.read_base()? {
        Some(bytes) => Some(serde_json::from_slice::<Base>(&bytes).context("parsing base.json")?),
        None => None,
    };
    let ids = t.list_segment_ids()?;
    // Only ids we actually parse are candidates — and only folded ones get
    // deleted, so a corrupt / truncated / future-schema segment is left in
    // place rather than destroyed without ever being folded into the base.
    let mut parsed: Vec<(String, Segment)> = Vec::new();
    for id in &ids {
        // A transport error propagates (auto-compaction catches it as non-fatal;
        // manual `cache-compact` exits non-zero). `None` = pruned by a concurrent
        // compaction between the list and this read; a corrupt blob is kept.
        if let Some(bytes) = t
            .read_segment(id)
            .with_context(|| format!("reading segment {id} while compacting"))?
        {
            match serde_json::from_slice::<Segment>(&bytes) {
                Ok(seg) => parsed.push((id.clone(), seg)),
                Err(_) => sink.warn(&format!(
                    "rstest: cache: compact: keeping unparseable segment {id}"
                )),
            }
        }
    }
    let ts_pairs: Vec<(String, u64)> = parsed
        .iter()
        .map(|(id, s)| (id.clone(), s.generated_at))
        .collect();
    let fold_ids: HashSet<String> = select_segments_to_fold(&ts_pairs, now, policy)
        .into_iter()
        .collect();
    let mut segments = Vec::new();
    let mut folded_ids = Vec::new();
    let mut retained = 0usize;
    for (id, seg) in parsed {
        if fold_ids.contains(&id) {
            segments.push(seg);
            folded_ids.push(id);
        } else {
            retained += 1;
        }
    }
    let folded = segments.len();
    let new_base = compact(base, segments);
    t.write_base(&serde_json::to_vec(&new_base).context("serializing base.json")?)?;
    // Re-read the persisted base and prune only segments it absorbed, so a
    // concurrent compaction that overwrote our base with a different fold set
    // can't leave a folded segment both deleted here and absent there. On any
    // read/parse failure, keep everything loose — a lingering segment is safe
    // (absorbed prevents double-counting), a deleted-but-unabsorbed one is not.
    let durable: HashSet<String> = match t.read_base() {
        Ok(Some(bytes)) => serde_json::from_slice::<Base>(&bytes)
            .map(|b| b.absorbed)
            .unwrap_or_default(),
        _ => HashSet::new(),
    };
    for id in &folded_ids {
        if durable.contains(id) {
            let _ = t.delete_segment(id);
        }
    }
    Ok((folded, retained))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::select::CoverageFile;

    /// Fold-all compaction (no retention window) — the common case in tests.
    fn compact_all(t: &dyn Transport, sink: &mut Sink) -> Result<usize> {
        compact_remote_with(t, sink, 0, &RetentionPolicy::default()).map(|(folded, _)| folded)
    }

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
    fn real_runner_spawn_failure_is_an_err() {
        // A program that can't resolve => spawn fails => Err (reserved for the
        // spawn itself, distinct from a non-zero exit).
        let r = RealRunner;
        let bogus = "rstest-definitely-not-a-real-binary-xyz".to_string();
        assert!(r.run(&bogus, &[], None).is_err());
    }

    #[test]
    fn real_runner_program_exists_probes_path() {
        // Walks every PATH entry (and, on Windows, every PATHEXT suffix) and
        // finds no match for a bogus name. Avoids mutating the process-global
        // PATH, which would race the rest of the parallel suite.
        let r = RealRunner;
        assert!(
            !r.program_exists("rstest-definitely-not-a-real-binary-xyz"),
            "a missing program must not resolve"
        );
    }

    #[test]
    fn program_extensions_bare_name_on_unix() {
        // Non-Windows: a single empty suffix, so `aws` probes exactly `aws`.
        let exts = program_extensions();
        assert!(exts.contains(&String::new()));
        #[cfg(not(windows))]
        assert_eq!(exts.len(), 1);
    }

    #[test]
    fn dir_transport_push_list_pull_roundtrip() {
        let root = tmp_dir("roundtrip");
        let t = DirTransport::new(&root);
        // Empty remote -> empty merge.
        assert_eq!(
            pull(&t, &mut Sink::captured().0).unwrap(),
            Merged::default()
        );
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
        let m = pull(&t, &mut Sink::captured().0).unwrap();
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
        assert_eq!(compact_all(&t, &mut Sink::captured().0).unwrap(), 2);
        // Segments gone, base present.
        assert!(t.list_segment_ids().unwrap().is_empty());
        assert!(t.read_base().unwrap().is_some());
        // Pull off the base alone reproduces the merged state.
        let m = pull(&t, &mut Sink::captured().0).unwrap();
        assert_eq!(m.durations.get("t::a"), Some(&3.0));
        assert_eq!(m.flakes.get("t::f").unwrap().flaky, 2);
        // A lingering copy of an already-folded segment must not double-count.
        push(
            &t,
            &seg("s1", 10, &[("t::a", 1.0)], &[("t::f", FlakeKind::Flaky)]),
        )
        .unwrap();
        assert_eq!(
            pull(&t, &mut Sink::captured().0)
                .unwrap()
                .flakes
                .get("t::f")
                .unwrap()
                .flaky,
            2
        );
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
        let m = pull(&t, &mut Sink::captured().0).unwrap();
        assert_eq!(cov_lines(&m, "mod.py", 1), vec!["t::a"]);
        assert_eq!(cov_lines(&m, "mod.py", 2), vec!["t::b"]);
        // Compaction folds the slices into the base and survives a re-pull.
        assert_eq!(compact_all(&t, &mut Sink::captured().0).unwrap(), 2);
        let m2 = pull(&t, &mut Sink::captured().0).unwrap();
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
        assert_eq!(compact_all(&t, &mut Sink::captured().0).unwrap(), 1); // only "good" folded
        assert!(corrupt.exists(), "unparseable segment must be kept");
        assert_eq!(t.list_segment_ids().unwrap(), vec!["bad".to_string()]);
        assert_eq!(
            pull(&t, &mut Sink::captured().0)
                .unwrap()
                .durations
                .get("t::a"),
            Some(&1.0)
        );
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

    /// A DirTransport that fails one write op, to prove write failures surface
    /// as `Err` (not a silently-swallowed no-op).
    enum FailAt {
        Base,
        Segment,
    }
    struct FailingTransport {
        inner: DirTransport,
        fail: FailAt,
    }
    impl Transport for FailingTransport {
        fn list_segment_ids(&self) -> Result<Vec<String>> {
            self.inner.list_segment_ids()
        }
        fn read_segment(&self, id: &str) -> Result<Option<Vec<u8>>> {
            self.inner.read_segment(id)
        }
        fn read_base(&self) -> Result<Option<Vec<u8>>> {
            self.inner.read_base()
        }
        fn write_segment(&self, id: &str, b: &[u8]) -> Result<()> {
            if matches!(self.fail, FailAt::Segment) {
                anyhow::bail!("simulated segment write failure");
            }
            self.inner.write_segment(id, b)
        }
        fn write_base(&self, b: &[u8]) -> Result<()> {
            if matches!(self.fail, FailAt::Base) {
                anyhow::bail!("simulated base write failure");
            }
            self.inner.write_base(b)
        }
        fn delete_segment(&self, id: &str) -> Result<()> {
            self.inner.delete_segment(id)
        }
    }

    #[test]
    fn push_surfaces_write_failure() {
        // A failed segment write must be reported, not swallowed as success.
        let root = tmp_dir("push-write-fail");
        let t = FailingTransport {
            inner: DirTransport::new(&root),
            fail: FailAt::Segment,
        };
        assert!(push(&t, &seg("s1", 10, &[("t::a", 1.0)], &[])).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- retention policy ---------------------------------------------------

    fn pairs(v: &[(&str, u64)]) -> Vec<(String, u64)> {
        v.iter().map(|(id, ts)| (id.to_string(), *ts)).collect()
    }

    #[test]
    fn retention_fold_all_when_unset() {
        let segs = pairs(&[("a", 10), ("b", 20), ("c", 30)]);
        let mut fold = select_segments_to_fold(&segs, 100, &RetentionPolicy::default());
        fold.sort();
        assert_eq!(fold, vec!["a", "b", "c"]);
    }

    #[test]
    fn retention_keep_last_retains_newest_n() {
        let segs = pairs(&[("a", 10), ("b", 20), ("c", 30), ("d", 40)]);
        let fold = select_segments_to_fold(
            &segs,
            100,
            &RetentionPolicy {
                keep_last: Some(2),
                max_age: None,
            },
        );
        // Newest two (d@40, c@30) retained; older folded.
        let mut fold = fold;
        fold.sort();
        assert_eq!(fold, vec!["a", "b"]);
    }

    #[test]
    fn retention_max_age_retains_young() {
        let segs = pairs(&[("old", 10), ("mid", 50), ("new", 90)]);
        // now=100, max_age=60 -> retain age<=60 (mid@50 age50, new@90 age10);
        // old@10 age90 folded.
        let fold = select_segments_to_fold(
            &segs,
            100,
            &RetentionPolicy {
                keep_last: None,
                max_age: Some(60),
            },
        );
        assert_eq!(fold, vec!["old"]);
    }

    #[test]
    fn retention_count_or_age_union_retains() {
        // keep_last=1 retains new@90; max_age=15 also retains new; old+mid folded.
        let segs = pairs(&[("old", 10), ("mid", 50), ("new", 90)]);
        let mut fold = select_segments_to_fold(
            &segs,
            100,
            &RetentionPolicy {
                keep_last: Some(1),
                max_age: Some(15),
            },
        );
        fold.sort();
        assert_eq!(fold, vec!["mid", "old"]);
    }

    #[test]
    fn retention_future_timestamp_is_retained() {
        // Clock skew: ts>now -> saturating_sub age 0 -> retained by any age rule.
        let segs = pairs(&[("future", 200)]);
        let fold = select_segments_to_fold(
            &segs,
            100,
            &RetentionPolicy {
                keep_last: None,
                max_age: Some(1),
            },
        );
        assert!(fold.is_empty());
    }

    #[test]
    fn compact_remote_with_keeps_recent_folds_old() {
        // Two old + one new segment; keep_last=1 folds the two old into base and
        // deletes them, leaving the newest loose. A pull still reproduces all.
        let root = tmp_dir("compact-retain");
        let t = DirTransport::new(&root);
        push(&t, &seg("old1", 10, &[("t::a", 1.0)], &[])).unwrap();
        push(&t, &seg("old2", 20, &[("t::a", 2.0)], &[])).unwrap();
        push(&t, &seg("new", 30, &[("t::a", 3.0)], &[])).unwrap();
        let (folded, retained) = compact_remote_with(
            &t,
            &mut Sink::captured().0,
            100,
            &RetentionPolicy {
                keep_last: Some(1),
                max_age: None,
            },
        )
        .unwrap();
        assert_eq!((folded, retained), (2, 1));
        assert_eq!(t.list_segment_ids().unwrap(), vec!["new".to_string()]);
        // Base (old1+old2) + loose "new" merge back to the newest duration.
        let m = pull(&t, &mut Sink::captured().0).unwrap();
        assert_eq!(m.durations.get("t::a"), Some(&3.0));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn compact_delete_gated_on_persisted_absorbed_set() {
        // Simulate a concurrent compaction winning the base write: the base
        // that ends up persisted does NOT list our folded ids in `absorbed`.
        // Deletes must be withheld so the folded segments stay loose (no data
        // loss) rather than being pruned into oblivion.
        struct RewriteBaseTransport {
            inner: DirTransport,
        }
        impl Transport for RewriteBaseTransport {
            fn list_segment_ids(&self) -> Result<Vec<String>> {
                self.inner.list_segment_ids()
            }
            fn read_segment(&self, id: &str) -> Result<Option<Vec<u8>>> {
                self.inner.read_segment(id)
            }
            fn read_base(&self) -> Result<Option<Vec<u8>>> {
                self.inner.read_base()
            }
            fn write_segment(&self, id: &str, b: &[u8]) -> Result<()> {
                self.inner.write_segment(id, b)
            }
            fn write_base(&self, _b: &[u8]) -> Result<()> {
                // A concurrent winner overwrote the base with an empty absorbed
                // set (it folded a different segment set than we did).
                let empty = compact(None, Vec::new());
                self.inner.write_base(&serde_json::to_vec(&empty).unwrap())
            }
            fn delete_segment(&self, id: &str) -> Result<()> {
                self.inner.delete_segment(id)
            }
        }
        let root = tmp_dir("compact-race-guard");
        let t = RewriteBaseTransport {
            inner: DirTransport::new(&root),
        };
        push(&t, &seg("s1", 10, &[("t::a", 1.0)], &[])).unwrap();
        push(&t, &seg("s2", 20, &[("t::b", 2.0)], &[])).unwrap();
        let (folded, _) =
            compact_remote_with(&t, &mut Sink::captured().0, 0, &RetentionPolicy::default())
                .unwrap();
        assert_eq!(folded, 2);
        // Persisted base didn't absorb s1/s2 -> deletes withheld -> both kept.
        let mut ids = t.list_segment_ids().unwrap();
        ids.sort();
        assert_eq!(ids, vec!["s1".to_string(), "s2".to_string()]);
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- CliTransport (s3:// / gs://) --------------------------------------

    use std::cell::RefCell;

    /// A stubbed cloud CLI: `programs` are the binaries that "exist"; `handler`
    /// maps each (program, args, stdin) to a canned `CliOutput`; `calls`
    /// records every invocation for assertions.
    type StubHandler = Box<dyn Fn(&str, &[String], Option<&[u8]>) -> CliOutput>;
    type StubCall = (String, Vec<String>, Option<Vec<u8>>);
    struct StubRunner {
        programs: Vec<String>,
        handler: StubHandler,
        calls: RefCell<Vec<StubCall>>,
    }
    impl StubRunner {
        fn new(
            programs: &[&str],
            handler: impl Fn(&str, &[String], Option<&[u8]>) -> CliOutput + 'static,
        ) -> Box<Self> {
            Box::new(StubRunner {
                programs: programs.iter().map(|s| s.to_string()).collect(),
                handler: Box::new(handler),
                calls: RefCell::new(Vec::new()),
            })
        }
    }
    impl CommandRunner for StubRunner {
        fn run(&self, program: &str, args: &[String], stdin: Option<&[u8]>) -> Result<CliOutput> {
            self.calls.borrow_mut().push((
                program.into(),
                args.to_vec(),
                stdin.map(|b| b.to_vec()),
            ));
            Ok((self.handler)(program, args, stdin))
        }
        fn program_exists(&self, program: &str) -> bool {
            self.programs.iter().any(|p| p == program)
        }
    }
    fn ok_out(stdout: &[u8]) -> CliOutput {
        CliOutput {
            success: true,
            stdout: stdout.to_vec(),
            stderr: String::new(),
        }
    }
    fn err_out(stderr: &str) -> CliOutput {
        CliOutput {
            success: false,
            stdout: Vec::new(),
            stderr: stderr.into(),
        }
    }

    #[test]
    fn pull_fails_on_transport_error_but_skips_missing_and_corrupt() {
        // A listed segment's read outcome drives pull: a transport/auth error
        // must fail the pull (no silent partial baseline); a `None` (pruned by a
        // concurrent compaction mid-pull) or a corrupt blob is skipped.
        enum Mode {
            Err,
            None,
            Corrupt,
        }
        struct Stub(Mode);
        impl Transport for Stub {
            fn list_segment_ids(&self) -> Result<Vec<String>> {
                Ok(vec!["s1".into()])
            }
            fn read_segment(&self, _id: &str) -> Result<Option<Vec<u8>>> {
                match self.0 {
                    Mode::Err => anyhow::bail!("boom-network"),
                    Mode::None => Ok(None),
                    Mode::Corrupt => Ok(Some(b"{ not json".to_vec())),
                }
            }
            fn read_base(&self) -> Result<Option<Vec<u8>>> {
                Ok(None)
            }
            fn write_segment(&self, _: &str, _: &[u8]) -> Result<()> {
                Ok(())
            }
            fn write_base(&self, _: &[u8]) -> Result<()> {
                Ok(())
            }
            fn delete_segment(&self, _: &str) -> Result<()> {
                Ok(())
            }
        }
        assert!(pull(&Stub(Mode::Err), &mut Sink::captured().0).is_err());
        assert!(pull(&Stub(Mode::None), &mut Sink::captured().0).is_ok());
        assert!(pull(&Stub(Mode::Corrupt), &mut Sink::captured().0).is_ok());
    }

    #[test]
    fn not_found_detection_ignores_connectivity_and_auth() {
        // Genuine object-absent messages across the three CLIs.
        assert!(looks_like_not_found(
            "fatal error: An error occurred (NoSuchKey)"
        ));
        assert!(looks_like_not_found("CommandException: No URLs matched"));
        assert!(looks_like_not_found("the object does not exist"));
        // Connectivity / auth / permission failures are NOT "missing object",
        // even when the text happens to contain a not-found-ish phrase.
        assert!(!looks_like_not_found(
            "Could not connect to the endpoint URL"
        ));
        assert!(!looks_like_not_found("Unable to locate credentials"));
        assert!(!looks_like_not_found("An error occurred (AccessDenied)"));
        assert!(!looks_like_not_found(
            "could not resolve host: bucket not found"
        ));
    }

    #[test]
    fn transport_for_dispatches_on_scheme() {
        // Bare path and file:// are directories; unknown schemes bail; http://
        // is rejected until the HTTP transport lands.
        assert!(transport_for("/tmp/x").is_ok());
        assert!(transport_for("file:///tmp/x").is_ok());
        // Unknown schemes bail loudly rather than silently making a junk dir.
        let err = match transport_for("azblob://c/x") {
            Ok(_) => panic!("azblob:// should be rejected"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("not supported"), "got {err}");
    }

    #[test]
    fn cli_aws_list_parses_ls_columns() {
        // `aws s3 ls` prints date/time/size/name columns; only seg-*.json
        // basenames survive the filter.
        let out = "2024-01-01 12:00:00        42 seg-run1-1of2.json\n\
                   2024-01-01 12:00:01        42 seg-run1-2of2.json\n\
                                              PRE nested/\n";
        let t = CliTransport::for_remote_with(
            "s3://bucket/prefix",
            StubRunner::new(&["aws"], move |_p, args, _| {
                assert_eq!(args, &["s3", "ls", "s3://bucket/prefix/segments/"]);
                ok_out(out.as_bytes())
            }),
        )
        .unwrap();
        let mut ids = t.list_segment_ids().unwrap();
        ids.sort();
        assert_eq!(ids, vec!["run1-1of2".to_string(), "run1-2of2".to_string()]);
    }

    #[test]
    fn cli_gcloud_list_parses_full_urls() {
        let out = "gs://bucket/p/segments/seg-a.json\n\
                   gs://bucket/p/segments/seg-b.json\n";
        let t = CliTransport::for_remote_with(
            "gs://bucket/p/",
            StubRunner::new(&["gcloud"], move |p, args, _| {
                assert_eq!(p, "gcloud");
                assert_eq!(args, &["storage", "ls", "gs://bucket/p/segments/"]);
                ok_out(out.as_bytes())
            }),
        )
        .unwrap();
        let mut ids = t.list_segment_ids().unwrap();
        ids.sort();
        assert_eq!(ids, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn cli_list_empty_prefix_is_not_an_error() {
        // aws `s3 ls` of an absent prefix: non-zero exit, empty stderr.
        let t = CliTransport::for_remote_with(
            "s3://bucket/p",
            StubRunner::new(&["aws"], |_, _, _| err_out("")),
        )
        .unwrap();
        assert!(t.list_segment_ids().unwrap().is_empty());
    }

    #[test]
    fn cli_list_real_error_surfaces() {
        let t = CliTransport::for_remote_with(
            "s3://bucket/p",
            StubRunner::new(&["aws"], |_, _, _| {
                err_out("An error occurred (AccessDenied) when calling ListObjects")
            }),
        )
        .unwrap();
        assert!(t.list_segment_ids().is_err());
    }

    #[test]
    fn cli_read_base_missing_is_none() {
        let t = CliTransport::for_remote_with(
            "s3://bucket/p",
            StubRunner::new(&["aws"], |_, _, _| {
                err_out("fatal error: An error occurred (NoSuchKey)")
            }),
        )
        .unwrap();
        assert_eq!(t.read_base().unwrap(), None);
    }

    #[test]
    fn cli_read_base_real_error_surfaces() {
        let t = CliTransport::for_remote_with(
            "s3://bucket/p",
            StubRunner::new(&["aws"], |_, _, _| err_out("Unable to locate credentials")),
        )
        .unwrap();
        assert!(t.read_base().is_err());
    }

    #[test]
    fn cli_write_segment_feeds_stdin_and_builds_url() {
        let t = CliTransport::for_remote_with(
            "s3://bucket/p",
            StubRunner::new(&["aws"], |_, args, stdin| {
                assert_eq!(
                    args,
                    &["s3", "cp", "-", "s3://bucket/p/segments/seg-x.json"]
                );
                assert_eq!(stdin, Some(b"payload".as_ref()));
                ok_out(b"")
            }),
        )
        .unwrap();
        t.write_segment("x", b"payload").unwrap();
    }

    #[test]
    fn cli_gs_prefers_gcloud_then_falls_back_to_gsutil() {
        // gcloud present -> gcloud; only gsutil present -> gsutil verbs (no
        // "storage" subcommand); neither -> a clear error.
        let g = CliTransport::for_remote_with(
            "gs://b/p",
            StubRunner::new(&["gcloud", "gsutil"], |_, _, _| ok_out(b"")),
        )
        .unwrap();
        assert_eq!(g.program(), "gcloud");

        let s = CliTransport::for_remote_with(
            "gs://b/p",
            StubRunner::new(&["gsutil"], |_, args, _| {
                assert_eq!(args, &["cat", "gs://b/p/base.json"]);
                err_out("no url matched")
            }),
        )
        .unwrap();
        assert_eq!(s.program(), "gsutil");
        assert_eq!(s.read_base().unwrap(), None);

        assert!(CliTransport::for_remote_with(
            "gs://b/p",
            StubRunner::new(&[], |_, _, _| ok_out(b""))
        )
        .is_err());
    }

    #[test]
    fn cli_delete_tolerates_missing() {
        let t = CliTransport::for_remote_with(
            "s3://b/p",
            StubRunner::new(&["aws"], |_, args, _| {
                assert_eq!(args, &["s3", "rm", "s3://b/p/segments/seg-x.json"]);
                err_out("delete failed: does not exist")
            }),
        )
        .unwrap();
        assert!(t.delete_segment("x").is_ok());
    }

    // ---- HttpTransport (http:// / https://) --------------------------------

    #[cfg(feature = "http-cache")]
    type HttpResponder = Box<dyn Fn(&str, &str, Option<&[u8]>) -> HttpResp>;
    #[cfg(feature = "http-cache")]
    struct StubHttpClient {
        responder: HttpResponder,
        calls: RefCell<Vec<(String, String)>>,
    }
    #[cfg(feature = "http-cache")]
    impl StubHttpClient {
        fn new(responder: impl Fn(&str, &str, Option<&[u8]>) -> HttpResp + 'static) -> Box<Self> {
            Box::new(StubHttpClient {
                responder: Box::new(responder),
                calls: RefCell::new(Vec::new()),
            })
        }
    }
    #[cfg(feature = "http-cache")]
    impl HttpClient for StubHttpClient {
        fn get(&self, url: &str) -> Result<HttpResp> {
            self.calls.borrow_mut().push(("GET".into(), url.into()));
            Ok((self.responder)("GET", url, None))
        }
        fn put(&self, url: &str, body: &[u8]) -> Result<HttpResp> {
            self.calls.borrow_mut().push(("PUT".into(), url.into()));
            Ok((self.responder)("PUT", url, Some(body)))
        }
        fn delete(&self, url: &str) -> Result<HttpResp> {
            self.calls.borrow_mut().push(("DELETE".into(), url.into()));
            Ok((self.responder)("DELETE", url, None))
        }
    }
    #[cfg(feature = "http-cache")]
    fn resp(status: u16, body: &[u8]) -> HttpResp {
        HttpResp {
            status,
            body: body.to_vec(),
        }
    }

    #[cfg(feature = "http-cache")]
    #[test]
    fn http_list_parses_json_array_and_ignores_non_segments() {
        let t = HttpTransport::new(
            "https://cache.example/team/",
            StubHttpClient::new(|_m, url, _b| {
                assert_eq!(url, "https://cache.example/team/segments/");
                // Mix of bare filenames, a full URL, and a non-segment entry.
                resp(
                    200,
                    br#"["seg-run1-1of2.json","https://cache.example/team/segments/seg-run1-2of2.json","index.html"]"#,
                )
            }),
        );
        let mut ids = t.list_segment_ids().unwrap();
        ids.sort();
        assert_eq!(ids, vec!["run1-1of2".to_string(), "run1-2of2".to_string()]);
    }

    #[cfg(feature = "http-cache")]
    #[test]
    fn http_list_404_is_empty_non_json_errors() {
        let empty =
            HttpTransport::new("https://c/x", StubHttpClient::new(|_, _, _| resp(404, b"")));
        assert!(empty.list_segment_ids().unwrap().is_empty());
        let bad = HttpTransport::new(
            "https://c/x",
            StubHttpClient::new(|_, _, _| resp(200, b"<html>not json</html>")),
        );
        assert!(bad.list_segment_ids().is_err());
    }

    #[cfg(feature = "http-cache")]
    #[test]
    fn http_read_base_404_none_2xx_some_5xx_error() {
        let none = HttpTransport::new("https://c/x", StubHttpClient::new(|_, _, _| resp(404, b"")));
        assert_eq!(none.read_base().unwrap(), None);
        let some = HttpTransport::new(
            "https://c/x",
            StubHttpClient::new(|_m, url, _| {
                assert_eq!(url, "https://c/x/base.json");
                resp(200, b"{}")
            }),
        );
        assert_eq!(some.read_base().unwrap(), Some(b"{}".to_vec()));
        let err = HttpTransport::new(
            "https://c/x",
            StubHttpClient::new(|_, _, _| resp(500, b"boom")),
        );
        assert!(err.read_base().is_err());
    }

    #[cfg(feature = "http-cache")]
    #[test]
    fn http_write_puts_to_segment_url_and_surfaces_failure() {
        let ok = HttpTransport::new(
            "https://c/x",
            StubHttpClient::new(|m, url, body| {
                assert_eq!(m, "PUT");
                assert_eq!(url, "https://c/x/segments/seg-a.json");
                assert_eq!(body, Some(b"payload".as_ref()));
                resp(201, b"")
            }),
        );
        ok.write_segment("a", b"payload").unwrap();
        let fail = HttpTransport::new(
            "https://c/x",
            StubHttpClient::new(|_, _, _| resp(403, b"denied")),
        );
        assert!(fail.write_segment("a", b"x").is_err());
    }

    #[cfg(feature = "http-cache")]
    #[test]
    fn http_delete_tolerates_404() {
        let t = HttpTransport::new(
            "https://c/x",
            StubHttpClient::new(|m, url, _| {
                assert_eq!(m, "DELETE");
                assert_eq!(url, "https://c/x/segments/seg-a.json");
                resp(404, b"")
            }),
        );
        assert!(t.delete_segment("a").is_ok());
    }

    #[cfg(feature = "http-cache")]
    #[test]
    fn http_transport_for_dispatches_and_no_junk_dir() {
        // http(s):// builds an HttpTransport (no network at construction).
        assert!(transport_for("https://cache/x").is_ok());
        assert!(transport_for("http://cache/x").is_ok());
    }

    #[cfg(feature = "http-cache")]
    #[test]
    fn http_real_client_reads_bearer_token_from_env() {
        // The env->Authorization mapping is pure; test it without a server.
        std::env::set_var("RSTEST_CACHE_REMOTE_TOKEN", "sekret");
        let c = RealHttpClient::from_env();
        assert_eq!(c.auth.as_deref(), Some("Bearer sekret"));
        std::env::remove_var("RSTEST_CACHE_REMOTE_TOKEN");
        assert_eq!(RealHttpClient::from_env().auth, None);
    }

    #[test]
    fn compact_remote_keeps_segments_when_base_write_fails() {
        // If the base write fails, segments must NOT be deleted — no data loss.
        let root = tmp_dir("compact-basefail");
        let seed = DirTransport::new(&root);
        push(&seed, &seg("s1", 10, &[("t::a", 1.0)], &[])).unwrap();
        push(&seed, &seg("s2", 20, &[("t::a", 2.0)], &[])).unwrap();
        let t = FailingTransport {
            inner: DirTransport::new(&root),
            fail: FailAt::Base,
        };
        assert!(
            compact_all(&t, &mut Sink::captured().0).is_err(),
            "base write failure must fail compaction"
        );
        let mut ids = t.list_segment_ids().unwrap();
        ids.sort();
        assert_eq!(
            ids,
            vec!["s1".to_string(), "s2".to_string()],
            "segments must survive"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- RealRunner (real subprocess) --------------------------------------

    #[test]
    fn program_extensions_include_bare_name() {
        // Every platform tries the bare name; Windows also tries PATHEXT
        // suffixes so `aws` matches `aws.exe` / `gcloud.cmd`.
        let exts = program_extensions();
        assert!(exts.iter().any(|e| e.is_empty()), "bare name always tried");
        if cfg!(windows) {
            assert!(exts.len() > 1, "windows adds PATHEXT candidates");
        } else {
            assert_eq!(exts, vec![String::new()]);
        }
    }

    #[cfg(unix)]
    #[test]
    fn real_runner_runs_program_feeds_stdin_and_probes_path() {
        let r = RealRunner;
        // program_exists: a shell is always on PATH, a nonsense name is not.
        assert!(r.program_exists("sh"));
        assert!(!r.program_exists("rstest-no-such-binary-xyz"));
        // `cat` echoes stdin to stdout (exercises the stdin-piped branch).
        let out = r.run("cat", &[], Some(b"hello")).unwrap();
        assert!(out.success);
        assert_eq!(out.stdout, b"hello");
        // A missing program is an Err from the spawn itself.
        assert!(r.run("rstest-no-such-binary-xyz", &[], None).is_err());
    }

    // ---- CliTransport construction + gcloud/gsutil verbs -------------------

    #[test]
    fn cli_for_remote_wrapper_and_construction_errors() {
        // for_remote wraps RealRunner; result depends on whether `aws` is on
        // PATH, but either way the wrapper body runs.
        let _ = CliTransport::for_remote("s3://bucket/p");
        // s3:// without the aws CLI => a clear error.
        assert!(CliTransport::for_remote_with(
            "s3://b/p",
            StubRunner::new(&[], |_, _, _| ok_out(b""))
        )
        .is_err());
        // A scheme CliTransport doesn't handle => defensive bail.
        assert!(CliTransport::for_remote_with(
            "ftp://x",
            StubRunner::new(&["aws"], |_, _, _| ok_out(b""))
        )
        .is_err());
    }

    #[test]
    fn cli_gcloud_read_write_delete_verbs() {
        let t = CliTransport::for_remote_with(
            "gs://b/p",
            StubRunner::new(&["gcloud"], |_, args, _| {
                match args.get(1).map(String::as_str) {
                    Some("cat") => ok_out(b"{}"),
                    Some("cp") | Some("rm") => ok_out(b""),
                    _ => err_out("unexpected"),
                }
            }),
        )
        .unwrap();
        // storage cat -> Found -> Some / read_segment bytes.
        assert_eq!(t.read_base().unwrap(), Some(b"{}".to_vec()));
        assert_eq!(t.read_segment("x").unwrap(), Some(b"{}".to_vec()));
        // storage cp - <url> for both base and segment.
        t.write_base(b"{}").unwrap();
        t.write_segment("x", b"data").unwrap();
        // storage rm <url>.
        t.delete_segment("x").unwrap();
    }

    #[test]
    fn cli_read_segment_missing_is_none() {
        // A not-found stderr maps a read to Missing -> Ok(None), so a puller can
        // tell a segment pruned mid-pull from a real transport error.
        let t = CliTransport::for_remote_with(
            "gs://b/p",
            StubRunner::new(&["gcloud"], |_, _, _| err_out("not found")),
        )
        .unwrap();
        assert_eq!(t.read_segment("gone").unwrap(), None);
        // A real (non not-found) failure is still an error.
        let hard = CliTransport::for_remote_with(
            "gs://b/p",
            StubRunner::new(&["gcloud"], |_, _, _| {
                err_out("Unable to locate credentials")
            }),
        )
        .unwrap();
        assert!(hard.read_segment("gone").is_err());
    }

    #[test]
    fn cli_gsutil_list_write_delete_verbs() {
        let t = CliTransport::for_remote_with(
            "gs://b/p",
            StubRunner::new(&["gsutil"], |_, args, _| {
                match args.first().map(String::as_str) {
                    Some("ls") => ok_out(b"gs://b/p/segments/seg-z.json\n"),
                    Some("cp") | Some("rm") => ok_out(b""),
                    _ => err_out("unexpected"),
                }
            }),
        )
        .unwrap();
        assert_eq!(t.program(), "gsutil");
        assert_eq!(t.list_segment_ids().unwrap(), vec!["z".to_string()]);
        t.write_segment("z", b"d").unwrap();
        t.delete_segment("z").unwrap();
    }

    #[test]
    fn cli_write_and_delete_failures_surface() {
        // A real (non not-found) failure on write or delete is an error.
        let t = CliTransport::for_remote_with(
            "s3://b/p",
            StubRunner::new(&["aws"], |_, _, _| {
                err_out("An error occurred (AccessDenied)")
            }),
        )
        .unwrap();
        assert!(t.write_segment("x", b"d").is_err());
        assert!(t.delete_segment("x").is_err());
    }

    // ---- HttpTransport: remaining status branches --------------------------

    #[cfg(feature = "http-cache")]
    #[test]
    fn http_list_non_2xx_surfaces() {
        let t = HttpTransport::new(
            "https://c/x",
            StubHttpClient::new(|_, _, _| resp(500, b"boom")),
        );
        assert!(t.list_segment_ids().is_err());
    }

    #[cfg(feature = "http-cache")]
    #[test]
    fn http_read_segment_2xx_bytes_404_none_5xx_error() {
        let ok = HttpTransport::new(
            "https://c/x",
            StubHttpClient::new(|_m, url, _| {
                assert_eq!(url, "https://c/x/segments/seg-a.json");
                resp(200, b"blob")
            }),
        );
        assert_eq!(ok.read_segment("a").unwrap(), Some(b"blob".to_vec()));
        // 404 -> None (pruned mid-pull), a 5xx -> hard error.
        let miss = HttpTransport::new("https://c/x", StubHttpClient::new(|_, _, _| resp(404, b"")));
        assert_eq!(miss.read_segment("a").unwrap(), None);
        let err = HttpTransport::new("https://c/x", StubHttpClient::new(|_, _, _| resp(500, b"")));
        assert!(err.read_segment("a").is_err());
    }

    #[cfg(feature = "http-cache")]
    #[test]
    fn http_write_base_puts_and_surfaces_failure() {
        let ok = HttpTransport::new(
            "https://c/x",
            StubHttpClient::new(|m, url, _| {
                assert_eq!(m, "PUT");
                assert_eq!(url, "https://c/x/base.json");
                resp(200, b"")
            }),
        );
        ok.write_base(b"{}").unwrap();
        let fail = HttpTransport::new(
            "https://c/x",
            StubHttpClient::new(|_, _, _| resp(500, b"boom")),
        );
        assert!(fail.write_base(b"{}").is_err());
    }

    #[cfg(feature = "http-cache")]
    #[test]
    fn http_delete_non_2xx_non_404_surfaces() {
        let t = HttpTransport::new(
            "https://c/x",
            StubHttpClient::new(|_, _, _| resp(500, b"boom")),
        );
        assert!(t.delete_segment("a").is_err());
    }

    // ---- RealHttpClient over a localhost server ----------------------------

    #[cfg(feature = "http-cache")]
    #[test]
    fn real_http_client_roundtrips_over_localhost() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // Serve 4 requests: GET 200, PUT 200, DELETE 200, GET 404.
        let handle = std::thread::spawn(move || {
            for (i, code) in [
                (0, "200 OK"),
                (1, "200 OK"),
                (2, "200 OK"),
                (3, "404 Not Found"),
            ] {
                let (mut s, _) = listener.accept().unwrap();
                // Fully drain the request (headers + any Content-Length body)
                // before replying. On Windows, dropping the socket with
                // unconsumed inbound bytes triggers an abortive (RST) close,
                // which races the client's write and surfaces as a transport
                // error — the source of this test's flakiness.
                let mut req = Vec::new();
                let mut buf = [0u8; 2048];
                let (head_end, content_len) = loop {
                    let n = s.read(&mut buf).unwrap();
                    if n == 0 {
                        break (req.len(), 0usize);
                    }
                    req.extend_from_slice(&buf[..n]);
                    if let Some(pos) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                        let text = String::from_utf8_lossy(&req[..pos]);
                        let cl = text
                            .lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                k.trim()
                                    .eq_ignore_ascii_case("content-length")
                                    .then(|| v.trim().parse::<usize>().ok())
                                    .flatten()
                            })
                            .unwrap_or(0);
                        break (pos + 4, cl);
                    }
                };
                let mut remaining = content_len.saturating_sub(req.len() - head_end);
                while remaining > 0 {
                    let n = s.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    remaining = remaining.saturating_sub(n);
                }
                let body: &[u8] = if i == 3 { b"" } else { b"ok" };
                let head = format!(
                    "HTTP/1.1 {code}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(body);
                let _ = s.flush();
                // Graceful half-close: signal EOF, let the client finish
                // reading before the socket drops.
                let _ = s.shutdown(std::net::Shutdown::Write);
            }
        });
        // Token set => with_auth attaches Authorization.
        std::env::set_var("RSTEST_CACHE_REMOTE_TOKEN", "tok");
        let c = RealHttpClient::from_env();
        std::env::remove_var("RSTEST_CACHE_REMOTE_TOKEN");
        let base = format!("http://{addr}");
        let g = c.get(&format!("{base}/base.json")).unwrap();
        assert_eq!(g.status, 200);
        assert_eq!(g.body, b"ok");
        let p = c.put(&format!("{base}/seg"), b"data").unwrap();
        assert_eq!(p.status, 200);
        let d = c.delete(&format!("{base}/seg")).unwrap();
        assert_eq!(d.status, 200);
        // A non-2xx status is folded into HttpResp, not an Err.
        let nf = c.get(&format!("{base}/missing")).unwrap();
        assert_eq!(nf.status, 404);
        handle.join().unwrap();
        // A connection failure (no auth => with_auth None branch) is an Err.
        let bad = RealHttpClient::from_env().get("http://127.0.0.1:1/x");
        assert!(bad.is_err());
    }
}
