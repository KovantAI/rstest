//! Warm-template workers, `--fork-pool=app` (macOS only for now).
//!
//! Workers record which non-test modules they imported; the run stores them in
//! `.rstest_cache/warm/manifest.json`. The next run's fork-pool zygote imports
//! those modules once, under guards (`python/rstest_worker/_internal/warm.py`),
//! before forking, so the workers start with the app already loaded. A module a
//! guard rejects goes on the manifest's deny list and is imported per worker.
//! A warm-template problem never fails a run: the zygote falls back to the
//! plain core (`--fork-pool`) behavior.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::cache;
use crate::reporting::sink::Sink;

pub const FILE: &str = "warm/manifest.json";

/// `--fork-pool[=core|app]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum ForkPool {
    /// Prewarm the vendored pytest core only.
    Core,
    /// Also import the app's modules in the template (internal for now).
    #[value(hide = true)]
    App,
}

/// What a manifest is valid for: another interpreter, rstest build,
/// environment or project makes it stale.
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone, Default)]
pub struct Key {
    pub python: String,
    pub rstest: String,
    pub env_fp: String,
    pub rootdir: String,
}

#[derive(Serialize, Deserialize, Debug, Default, PartialEq)]
pub struct Manifest {
    pub key: Key,
    pub sys_path: Vec<String>,
    pub modules: Vec<String>,
    /// Module -> why it is imported per worker instead of in the template.
    #[serde(default)]
    pub deny: BTreeMap<String, String>,
    /// Set when a worker found coverage tracing (e.g. `--cov` from
    /// `addopts`): template imports would lose their import-time lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<String>,
}

/// The modules every worker of one run imported, merged in first-seen order.
#[derive(Default)]
pub struct Record {
    pub imported: Vec<String>,
    seen: HashSet<String>,
    pub sys_path: Vec<String>,
    pub coverage: bool,
}

impl Record {
    pub fn merge(&mut self, imported: Vec<String>, sys_path: Vec<String>, coverage: bool) {
        for m in imported {
            if self.seen.insert(m.clone()) {
                self.imported.push(m);
            }
        }
        if self.sys_path.is_empty() {
            self.sys_path = sys_path;
        }
        self.coverage |= coverage;
    }
}

/// What the zygote reports back (`RSTEST_WARM_RESULT`).
#[derive(Deserialize, Default)]
struct Outcome {
    #[serde(default)]
    imported: usize,
    #[serde(default)]
    denied: BTreeMap<String, String>,
    hazard: Option<String>,
    error: Option<String>,
    #[serde(default)]
    seconds: f64,
}

/// The worker environment of an `=app` run.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkerWarm {
    /// The manifest the zygote imports; None records only (no usable manifest yet).
    pub manifest: Option<PathBuf>,
    /// Where the zygote writes its [`Outcome`].
    pub result: PathBuf,
}

/// A prepared `=app` run.
pub struct WarmRun {
    pub env: WorkerWarm,
    key: Key,
    path: PathBuf,
    prior: Option<Manifest>,
}

/// The `--doctor` summary.
#[derive(Clone, Debug, PartialEq)]
pub struct Stats {
    pub modules: usize,
    pub denied: usize,
    pub seconds: f64,
}

/// Why this run can't use `=app` (it then runs as `=core`), or None.
pub fn refusal(macos: bool, coverage: bool, prior: Option<&Manifest>) -> Option<String> {
    if !macos {
        return Some("--fork-pool=app is macOS-only for now; using --fork-pool".into());
    }
    if coverage {
        return Some(
            "--fork-pool=app does not support coverage yet (template imports would lose \
             their import-time lines); using --fork-pool"
                .into(),
        );
    }
    prior
        .and_then(|m| m.refused.as_ref())
        .map(|why| format!("--fork-pool=app is off for this project: {why}; using --fork-pool"))
}

/// Coverage requested on the command line or inherited from a pytest-cov
/// parent. `--cov` from `addopts` is caught by the workers instead (see
/// [`Manifest::refused`]).
pub fn coverage_requested(args: &[String]) -> bool {
    args.iter().any(|a| a == "--cov" || a.starts_with("--cov="))
        || std::env::var_os("COV_CORE_SOURCE").is_some()
}

fn load(path: &Path) -> Option<Manifest> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Set up an `=app` run, or None (with a note) when it falls back to `=core`.
/// Only pool runs get here: single-worker and passthrough runs spawn no pool.
pub fn prepare(key: Key, args: &[String], sink: &mut Sink) -> Option<WarmRun> {
    let path = cache::file(FILE);
    let prior = load(&path);
    if let Some(why) = refusal(
        cfg!(target_os = "macos"),
        coverage_requested(args),
        prior.as_ref(),
    ) {
        sink.warn(&format!("rstest: note: {why}"));
        return None;
    }
    let usable = prior.as_ref().is_some_and(|m| m.key == key);
    Some(WarmRun {
        env: WorkerWarm {
            manifest: usable.then(|| path.clone()),
            result: crate::scratch::dir().join("warm-result.json"),
        },
        key,
        path,
        prior: prior.filter(|_| usable),
    })
}

/// After the run: fold the zygote's outcome and the workers' record into the
/// manifest, print a note per newly denied module, and return the counts.
pub fn finish(run: WarmRun, record: Record, sink: &mut Sink) -> Option<Stats> {
    let outcome: Option<Outcome> = std::fs::read(&run.env.result)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    let _ = std::fs::remove_file(&run.env.result);
    let prior = run.prior.unwrap_or_default();
    let mut deny = prior.deny.clone();
    if let Some(o) = &outcome {
        for line in notes(&prior.deny, o) {
            sink.warn(&line);
        }
        deny.extend(o.denied.clone());
    }
    let manifest = Manifest {
        key: run.key,
        sys_path: if record.sys_path.is_empty() {
            prior.sys_path
        } else {
            record.sys_path
        },
        modules: if record.imported.is_empty() {
            prior.modules
        } else {
            record.imported
        },
        deny,
        refused: record
            .coverage
            .then(|| "coverage is active in the workers".to_string()),
    };
    if let Ok(bytes) = serde_json::to_vec(&manifest) {
        let _ = cache::write_atomic(&run.path, &bytes);
    }
    outcome.map(|o| Stats {
        modules: o.imported,
        denied: manifest.deny.len(),
        seconds: o.seconds,
    })
}

/// One note per module a guard newly denied, one summary line for new import
/// errors, and why the template ran cold, if it did.
fn notes(known: &BTreeMap<String, String>, o: &Outcome) -> Vec<String> {
    let mut out = Vec::new();
    let mut errors = 0;
    for (name, why) in o.denied.iter().filter(|(n, _)| !known.contains_key(*n)) {
        if why.starts_with("import error") {
            errors += 1;
        } else {
            out.push(format!(
                "rstest: note: --fork-pool=app: {name} {why}, so it is imported per worker"
            ));
        }
    }
    if errors > 0 {
        out.push(format!(
            "rstest: note: --fork-pool=app: {errors} module(s) failed to import in the \
             template and are imported per worker"
        ));
    }
    if o.hazard.is_some() {
        out.push(
            "rstest: note: --fork-pool=app: the template ran cold this run; the next run \
             skips the module above"
                .into(),
        );
    }
    if let Some(e) = &o.error {
        out.push(format!(
            "rstest: note: --fork-pool=app: template skipped: {e}"
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(env_fp: &str) -> Key {
        Key {
            python: "/py".into(),
            rstest: "0.11.1".into(),
            env_fp: env_fp.into(),
            rootdir: "/proj".into(),
        }
    }

    #[test]
    fn refused_off_macos_under_coverage_and_when_the_manifest_says_so() {
        assert!(refusal(false, false, None).unwrap().contains("macOS-only"));
        assert!(refusal(true, true, None).unwrap().contains("coverage"));
        let m = Manifest {
            refused: Some("coverage is active in the workers".into()),
            ..Manifest::default()
        };
        assert!(refusal(true, false, Some(&m))
            .unwrap()
            .contains("coverage is active"));
        assert_eq!(refusal(true, false, Some(&Manifest::default())), None);
        assert_eq!(refusal(true, false, None), None);
    }

    #[test]
    fn coverage_is_read_from_the_command_line() {
        let a = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(coverage_requested(&a(&["--cov=pkg", "tests"])));
        assert!(coverage_requested(&a(&["--cov"])));
    }

    #[test]
    fn record_merges_in_first_seen_order() {
        let mut r = Record::default();
        let v = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        r.merge(v(&["a", "b"]), v(&["/p1"]), false);
        r.merge(v(&["b", "c", "a"]), v(&["/p2"]), true);
        assert_eq!(r.imported, v(&["a", "b", "c"]));
        assert_eq!(r.sys_path, v(&["/p1"]));
        assert!(r.coverage);
    }

    /// A fresh per-test temp dir, removed on drop.
    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new() -> Self {
            static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let d = std::env::temp_dir().join(format!("rstest-warm-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&d).unwrap();
            Self(d)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn run_in(dir: &Path, prior: Option<Manifest>, key: Key) -> WarmRun {
        let usable = prior.as_ref().is_some_and(|m| m.key == key);
        WarmRun {
            env: WorkerWarm {
                manifest: None,
                result: dir.join("result.json"),
            },
            key,
            path: dir.join("manifest.json"),
            prior: prior.filter(|_| usable),
        }
    }

    #[test]
    fn a_stale_key_drops_the_old_manifest() {
        let dir = TmpDir::new();
        let old = Manifest {
            key: key("old"),
            modules: vec!["stale".into()],
            deny: BTreeMap::from([("x".into(), "why".into())]),
            ..Manifest::default()
        };
        let run = run_in(dir.path(), Some(old), key("new"));
        assert!(run.prior.is_none());
        let (mut sink, _cap) = Sink::captured();
        finish(run, Record::default(), &mut sink);
        let m = load(&dir.path().join("manifest.json")).unwrap();
        assert_eq!(m.key, key("new"));
        assert!(m.modules.is_empty() && m.deny.is_empty());
    }

    #[test]
    fn finish_merges_denies_and_notes_only_new_ones() {
        let dir = TmpDir::new();
        let prior = Manifest {
            key: key("k"),
            modules: vec!["old".into()],
            deny: BTreeMap::from([("known".into(), "opens a socket at import".into())]),
            ..Manifest::default()
        };
        std::fs::write(
            dir.path().join("result.json"),
            r#"{"imported": 7, "seconds": 0.5, "hazard": "envapp: reads PYTEST_XDIST_WORKER at import",
                "error": null, "denied": {"envapp": "reads PYTEST_XDIST_WORKER at import",
                "known": "opens a socket at import", "models": "import error (AppRegistryNotReady: x)"}}"#,
        )
        .unwrap();
        let run = run_in(dir.path(), Some(prior), key("k"));
        let (mut sink, cap) = Sink::captured();
        let mut record = Record::default();
        record.merge(vec!["new".into()], vec!["/p".into()], false);
        let stats = finish(run, record, &mut sink).unwrap();
        assert_eq!(
            stats,
            Stats {
                modules: 7,
                denied: 3,
                seconds: 0.5
            }
        );
        let err = cap.err();
        assert!(err.contains("envapp reads PYTEST_XDIST_WORKER at import"));
        assert!(!err.contains("known"));
        assert!(err.contains("1 module(s) failed to import"));
        assert!(err.contains("ran cold"));
        let m = load(&dir.path().join("manifest.json")).unwrap();
        assert_eq!(m.modules, vec!["new".to_string()]);
        assert_eq!(m.deny.len(), 3);
        assert!(!dir.path().join("result.json").exists());
    }

    #[test]
    fn coverage_in_the_workers_refuses_later_runs() {
        let dir = TmpDir::new();
        let mut record = Record::default();
        record.merge(vec!["a".into()], vec![], true);
        let (mut sink, _cap) = Sink::captured();
        finish(run_in(dir.path(), None, key("k")), record, &mut sink);
        let m = load(&dir.path().join("manifest.json")).unwrap();
        assert!(refusal(true, false, Some(&m)).is_some());
    }
}
