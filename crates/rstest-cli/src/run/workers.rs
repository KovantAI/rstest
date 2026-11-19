//! Worker-count resolution: `-n` parsing, `-n auto` sizing by CPU, file count
//! and cached test time, and the selection those sizings read.

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::scheduling::durations;
use crate::{collect, config};

pub(super) fn parse_numprocesses(value: &str, args: &[String]) -> Result<usize> {
    if value == "auto" {
        return Ok(auto_workers(args));
    }
    value.parse().with_context(|| {
        format!(
            "invalid worker count {value:?} for -n/--numprocesses or [tool.rstest] \
             numprocesses (expected {})",
            config::NUMPROCESSES_EXPECTED
        )
    })
}

/// `auto` = logical cores, capped by what the selected tests can use (worker
/// startup costs real time). Sized from the session args' selection (paths and
/// nodeids, the invocation dir or `testpaths` when none), with config
/// discovered from the args' common ancestor like pytest. The parallelizable
/// units are the tests the duration cache times inside the selection, plus one
/// per selected test file or nodeid it has no timing for: `--dist load` splits
/// within a file, so a timed file counts its tests, not 1. When the whole
/// selection is timed (or nothing is selected), the cached time caps it too:
/// a few-second suite needs ~2 workers.
///
/// A walk with no Python file at all (an empty folder) means one worker,
/// unless `args` name a path the walk didn't cover.
///
/// Memory caps it last: when the recorded peak worker RSS times the count
/// would not fit the memory limit, fewer workers (said on stderr, where the
/// sizing notes go).
pub(super) fn auto_workers(args: &[String]) -> usize {
    let n = auto_workers_by_cpu(args);
    match crate::scheduling::memory::cap(n) {
        Some(cap) => {
            eprintln!("{}", cap.note());
            cap.workers
        }
        None => n,
    }
}

pub(super) fn auto_workers_by_cpu(args: &[String]) -> usize {
    let cores = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(4);
    let Ok(cwd) = std::env::current_dir() else {
        let cache = durations::load();
        return cap_workers_by_time(cores, cache.values().sum(), cache.len()).max(1);
    };
    let cwd = config::normalize(&cwd);
    let rootdir = crate::cache::rootdir()
        .map(PathBuf::from)
        .unwrap_or_else(|| config::rootdir(&cwd, args));
    let mut sel = Selection::from_args(&cwd, args);
    let explicit = !sel.is_empty();
    // No args below the rootdir: pytest collects the invocation dir (it reads
    // `testpaths` only when run from the rootdir itself).
    if !explicit && cwd != rootdir && cwd.starts_with(&rootdir) {
        sel.paths.push(cwd.clone());
    }
    // Worker-sizing has no run Sink; a malformed-config note here is a dup of
    // the one the real discover emits through the Sink, so send it to stderr.
    let start = config::common_ancestor(&cwd, &config::selected_paths(&cwd, args));
    let project = config::discover(&start, &mut std::io::stderr());
    // Explicit paths walk as given; no selection walks `testpaths` / rootdir;
    // nodeids alone need no walk.
    let files = if sel.nodeids.is_empty() || !sel.paths.is_empty() {
        match collect::collect_test_files(&sel.paths, &project) {
            Ok(files) => files,
            Err(_) => return cores,
        }
    } else {
        Vec::new()
    };
    if files.is_empty()
        && sel.nodeids.is_empty()
        && !names_existing_path(args)
        && collect::has_python_files(&sel.paths, &project).is_ok_and(|found| !found)
    {
        return 1;
    }
    let cache = durations::load();
    let t = sel.timed(&files, &cache, &rootdir);
    let mut n = match t.units() {
        0 => cores,
        units => cores.min(units),
    };
    if t.cold == 0 && t.tests > 0 {
        n = cap_workers_by_time(n, t.secs, t.tests);
    } else if !explicit && !cache.is_empty() {
        // A partly timed whole suite: the cached time is the best estimate.
        n = cap_workers_by_time(n, cache.values().sum(), cache.len());
    }
    n.max(1)
}

/// What the session args select, for `-n auto` sizing: existing paths (files
/// or dirs) and nodeids (`file.py::test...`) whose file exists, absolute.
#[derive(Default)]
pub(super) struct Selection {
    pub(super) paths: Vec<PathBuf>,
    /// (absolute file, the `::`-suffix as written).
    pub(super) nodeids: Vec<(PathBuf, String)>,
}

/// The duration cache folded over a selection (see [`Selection::timed`]).
#[derive(Debug, Default, PartialEq)]
pub(super) struct Timed {
    /// Cached tests inside the selection.
    pub(super) tests: usize,
    /// Their summed cached seconds.
    pub(super) secs: f64,
    /// Selected test files and nodeids with no cached test.
    pub(super) cold: usize,
}

impl Timed {
    pub(super) fn units(&self) -> usize {
        self.tests + self.cold
    }
}

impl Selection {
    pub(super) fn from_args(cwd: &std::path::Path, args: &[String]) -> Self {
        let mut sel = Self::default();
        for (a, pos) in args.iter().zip(crate::cli::positional_mask(args)) {
            if !pos || a.starts_with('@') {
                continue;
            }
            match a.split_once("::") {
                Some((file, rest)) => {
                    let file = config::normalize(&cwd.join(file));
                    if file.is_file() {
                        sel.nodeids.push((file, rest.to_string()));
                    }
                }
                None => {
                    let path = config::normalize(&cwd.join(a));
                    if path.exists() {
                        sel.paths.push(path);
                    }
                }
            }
        }
        sel
    }

    pub(super) fn is_empty(&self) -> bool {
        self.paths.is_empty() && self.nodeids.is_empty()
    }

    /// Fold the duration cache (nodeids relative to `rootdir`) over the
    /// selection: `files` (the walked test files) and the nodeids.
    pub(super) fn timed(
        &self,
        files: &[PathBuf],
        cache: &std::collections::HashMap<String, f64>,
        rootdir: &std::path::Path,
    ) -> Timed {
        use std::collections::HashMap;
        let canon = |p: &std::path::Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        let mut file_hit: HashMap<PathBuf, bool> =
            files.iter().map(|f| (canon(f), false)).collect();
        let nodeids: Vec<(PathBuf, String)> = self
            .nodeids
            .iter()
            .map(|(file, rest)| (canon(file), rest.clone()))
            .collect();
        let mut id_hit = vec![false; nodeids.len()];
        let mut resolved: HashMap<&str, PathBuf> = HashMap::new();
        let mut t = Timed::default();
        for (id, secs) in cache {
            let (file, rest) = id.split_once("::").unwrap_or((id.as_str(), ""));
            let abs = resolved
                .entry(file)
                .or_insert_with(|| canon(&rootdir.join(file)));
            let mut hit = false;
            if let Some(h) = file_hit.get_mut(abs.as_path()) {
                *h = true;
                hit = true;
            }
            for (i, (f, want)) in nodeids.iter().enumerate() {
                let matches = f == abs
                    && (rest == want
                        || rest
                            .strip_prefix(want.as_str())
                            .is_some_and(|r| r.starts_with('[') || r.starts_with("::")));
                if matches {
                    id_hit[i] = true;
                    hit = true;
                }
            }
            if hit {
                t.tests += 1;
                t.secs += secs;
            }
        }
        t.cold =
            file_hit.values().filter(|h| !**h).count() + id_hit.iter().filter(|h| !**h).count();
        t
    }
}

/// Worker count for a parallel-safety check (`audit`, `migrate-check`): like
/// `auto`, but never capped by the duration cache and never below 2. A check
/// pass that resolves to one worker runs serially, so parallel-only failures
/// can't show and the suite would be called parallel-safe untested.
pub(crate) fn check_workers() -> usize {
    let cores = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(4);
    let mut n = cores;
    if let Ok(cwd) = std::env::current_dir() {
        let project = config::discover(&cwd, &mut std::io::stderr());
        if let Ok(files) = collect::collect_test_files(&[], &project) {
            n = cap_workers_by_files(n, files.len());
        }
    }
    n.max(2)
}

/// Whether the args select something on disk: a path, or a nodeid
/// (`file.py::test`) whose file exists. `--pyargs` counts too: its positionals
/// are import paths the walk never sees, so an empty walk proves nothing.
/// Over-matching only keeps the empty-walk shortcut off, which is the safe
/// direction.
pub(super) fn names_existing_path(args: &[String]) -> bool {
    args.iter().any(|a| a == "--pyargs")
        || !crate::cli::path_args(args).is_empty()
        || args.iter().any(|a| {
            a.split_once("::")
                .is_some_and(|(file, _)| std::path::Path::new(file).exists())
        })
}

/// Cap the worker count by test-file count: never more workers than files. An
/// empty walk (count 0) leaves `n` unchanged — the other signals still apply.
pub(super) fn cap_workers_by_files(n: usize, file_count: usize) -> usize {
    if file_count > 0 {
        n.min(file_count)
    } else {
        n
    }
}

/// Per-test framework cost the duration cache doesn't see (it times the test,
/// not the hooks and reporting around it): 10,000 empty tests record 0.0s each
/// but take 1.83s under serial pytest on an M4 Max.
pub(super) const PER_TEST_OVERHEAD_SECS: f64 = 0.0002;
/// Work one worker must have to pay off its startup. Measured 2026-11-10 on an
/// M4 Max: each extra worker adds ~7ms of pool startup, so 0.25s keeps a
/// sub-second suite on 1 worker while 10,000 trivial tests get 8.
pub(super) const SECS_PER_WORKER: f64 = 0.25;

/// Cap by suite time from the duration cache plus per-test overhead for its
/// `tests` tests. Never drops below 1 worker.
pub(super) fn cap_workers_by_time(n: usize, total_secs: f64, tests: usize) -> usize {
    let work = total_secs + tests as f64 * PER_TEST_OVERHEAD_SECS;
    let by_time = (work / SECS_PER_WORKER).ceil() as usize;
    n.min(by_time.max(1))
}
