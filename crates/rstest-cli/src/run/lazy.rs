//! Lazy-collection decisions: when `--collect auto` picks lazy, the roots a
//! lazy run walks, and the cached durations those checks read.

use std::path::{Path, PathBuf};

use anyhow::Result;

use super::dispatch::resolve_order;
use super::names_a_selection;
use super::workers::PER_TEST_OVERHEAD_SECS;
use crate::cli::Cli;
use crate::reporting::sink::Sink;
use crate::scheduling::{durations, pool};
use crate::{collect, config};

/// Auto-lazy floor: below this many known tests, full collection's locality
/// (contiguous module dispatch, one warm collection per worker) beats paying
/// the on-demand per-file collection overhead, so a small suite stays eager.
pub(super) const AUTO_LAZY_MIN_TESTS: usize = 2000;
/// Auto-lazy work floor on `tests * workers`. Lazy's win is dropping the
/// (workers - 1) redundant full collections, so it scales with both suite
/// size and worker count; 2000 tests need >= 8 workers, 4000 need >= 4.
pub(super) const AUTO_LAZY_MIN_WORK: usize = 16_000;

/// A nodeid positional (`tests/a.py::t`, or `pkg.mod::t` under `--pyargs`):
/// a selection the lazy file walk can't see. Option values don't count:
/// `-W default::X` and `--deselect a.py::t` hold `::` but select no test.
pub(super) fn selects_nodeids(args: &[String]) -> bool {
    args.iter()
        .zip(crate::cli::positional_mask(args))
        .any(|(a, pos)| pos && a.contains("::"))
}

/// pytest's `search_pypath` (no namespace packages), run in the worker
/// interpreter: one line per name, the module file or package directory, or
/// empty when it doesn't resolve.
pub(super) const PYARGS_PROBE: &str = r#"
import importlib.util, os, sys
for name in sys.argv[1:]:
    try:
        spec = importlib.util.find_spec(name)
    except (AttributeError, ImportError, ValueError):
        spec = None
    path = ""
    if spec is not None:
        if not spec.submodule_search_locations:
            path = spec.origin or ""
        elif spec.origin:
            path = os.path.dirname(spec.origin)
    print(path)
"#;

/// Where the lazy file walk starts: the positional paths, or under `--pyargs`
/// each positional resolved as pytest does (a dotted name to its module or
/// package, else the arg as a path). `None` when a `--pyargs` arg resolves to
/// nothing pytest could collect: lazy can't reproduce that run.
pub(super) fn lazy_roots(python: &Path, args: &[String]) -> Option<Vec<PathBuf>> {
    let positional = || {
        args.iter()
            .zip(crate::cli::positional_mask(args))
            .filter(|(a, pos)| *pos && !a.starts_with('@'))
            .map(|(a, _)| a.clone())
    };
    if !args.iter().any(|a| a == "--pyargs") {
        return Some(
            crate::cli::path_args(args)
                .into_iter()
                .filter(|a| !a.starts_with('@'))
                .map(PathBuf::from)
                .collect(),
        );
    }
    let names: Vec<String> = positional().collect();
    if names.is_empty() {
        return Some(Vec::new());
    }
    // Resolved once per process: the lazy decision and the dispatch walk
    // both ask. ponytail: sys.path is the bare interpreter's plus the cwd,
    // so an ini `pythonpath` entry pytest adds is not seen; such a name
    // stays unresolved and the run keeps full collection.
    type Memo = std::collections::HashMap<(PathBuf, Vec<String>), Option<Vec<PathBuf>>>;
    static MEMO: std::sync::OnceLock<std::sync::Mutex<Memo>> = std::sync::OnceLock::new();
    let key = (python.to_path_buf(), names.clone());
    let memo = MEMO.get_or_init(Default::default);
    if let Some(hit) = memo.lock().ok().and_then(|m| m.get(&key).cloned()) {
        return hit;
    }
    let resolved = (|| {
        let mut cmd = std::process::Command::new(python);
        cmd.args(["-c", PYARGS_PROBE])
            .args(&names)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        crate::scheduling::worker::scrub_secrets(&mut cmd);
        let out = cmd.output().ok().filter(|o| o.status.success())?;
        let lines: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect();
        (lines.len() == names.len()).then_some(())?;
        names
            .iter()
            .zip(lines)
            .map(|(name, found)| {
                // Unresolved: pytest falls back to the arg as a path.
                let p = PathBuf::from(if found.is_empty() { name } else { &found });
                p.exists().then_some(p)
            })
            .collect::<Option<Vec<_>>>()
    })();
    if let Ok(mut m) = memo.lock() {
        m.insert(key, resolved.clone());
    }
    resolved
}

/// Auto-default heuristic (only reached when neither `--collect` nor
/// `[tool.rstest] collect` was set): pick lazy for a big-enough suite on a
/// file-affine dist, so a large parallel run stops re-collecting the whole
/// suite once per worker. Pure over its inputs (`tests` = cached test count)
/// so tests can drive it without a warm cache. Conservative on purpose: a cold
/// cache (`tests == 0`), any file-affinity-breaking selection, or a narrowed
/// path selection keeps full collection.
pub(super) fn auto_lazy(dist_name: &str, args: &[String], n: usize, tests: usize) -> bool {
    n >= 2
        && matches!(dist_name, "load" | "loadfile")
        // nodeid / --pyargs selection can't ride the file walk (same reason
        // explicit lazy falls back), so never auto-pick lazy for those.
        && !selects_nodeids(args)
        // `tests` counts the whole cached suite, not the selection. A path
        // selection (explicit, or --changed/--since-green narrowing) can be a
        // handful of files, and auto-lazy's strict file affinity would cap
        // parallelism at the file count (one big file = one worker).
        && !names_a_selection(args)
        && tests >= AUTO_LAZY_MIN_TESTS
        && tests.saturating_mul(n) >= AUTO_LAZY_MIN_WORK
}

/// Resolve the collection strategy (CLI > [tool.rstest] > auto) and validate
/// lazy-mode constraints. When neither the flag nor the setting is present,
/// [`auto_lazy`] decides from suite size and worker count.
#[allow(clippy::too_many_arguments)]
pub(super) fn collect_lazy(
    cli: &Cli,
    settings: &config::RstestSettings,
    dist_name: &str,
    python: &Path,
    args: &[String],
    n: usize,
    cache: &DurationCache,
    sink: &mut Sink,
) -> Result<bool> {
    let explicit = cli.collect.clone().or_else(|| settings.collect.clone());
    match explicit.as_deref() {
        Some("full") => Ok(false),
        Some("lazy") => {
            if !matches!(dist_name, "load" | "loadfile") {
                anyhow::bail!(
                    "--collect lazy is file-affine and cannot honor --dist {dist_name} \
                     (only load/loadfile: loadscope/loadgroup need a global id list and \
                     each runs the full suite on every worker; use --collect full)"
                );
            }
            // Single-test selection by nodeid wants exact-item dispatch;
            // a --pyargs name that doesn't resolve leaves the file walk
            // nothing to start from. Both fall back to full collection.
            if selects_nodeids(args) {
                sink.warn(
                    "rstest: nodeid arguments given; --collect lazy falls back \
                     to full collection",
                );
                return Ok(false);
            }
            if lazy_roots(python, args).is_none() {
                sink.warn(
                    "rstest: a --pyargs argument did not resolve to a module or \
                     package path; --collect lazy falls back to full collection",
                );
                return Ok(false);
            }
            Ok(true)
        }
        Some(other) => anyhow::bail!("unknown --collect mode: {other} (use full|lazy)"),
        // Auto: no warn on the file-affinity-breaking fallbacks (the user did
        // not ask for lazy); the banner is printed once at the dispatch site.
        // Features that require (or force) full collection veto auto-lazy so
        // the auto-pick never silently disables --incremental or turns
        // --shuffle (which errors under lazy) into a hard failure. An explicit
        // --collect lazy still conflicts with these loudly downstream.
        //
        // --shard is vetoed too: auto-lazy would switch it to file-granular
        // partitioning with no collection hash (so `shard-verify` finds no shard
        // metadata), and shard jobs whose caches differ (one cold) would pick
        // different modes and partition the suite inconsistently. A fail-fast
        // order (explicit or the --watch auto-pick) is ignored under lazy, so it
        // keeps full collection rather than silently losing its ordering.
        None if cli.shuffle.is_some()
            || cli.incremental
            || cli.shard.is_some()
            || cli.budget.is_some()
            || matches!(resolve_order(cli, settings), Ok(pool::Order::FailFast)) =>
        {
            Ok(false)
        }
        // Cheap gates first: the duration-cache load hashes every cached test
        // source, so skip it when the count cannot matter (serial run, affinity-
        // breaking dist); `auto_lazy` re-checks these for its own callers.
        None if n < 2 || !matches!(dist_name, "load" | "loadfile") => Ok(false),
        None => Ok(auto_lazy(dist_name, args, n, cache.get().len()) && {
            // Past the size gate, check the lazy file walk against what the
            // last eager collection timed. Rare enough (big warm suites only)
            // that the extra walk is noise; its config warnings are already
            // reported by the dispatch-time discover, so drop them here.
            let cwd = std::env::current_dir()?;
            let project = config::discover(&cwd, &mut std::io::sink());
            let env_addopts = std::env::var("PYTEST_ADDOPTS").unwrap_or_default();
            let opts = args
                .iter()
                .chain(&project.addopts)
                .map(String::as_str)
                .chain(env_addopts.split_whitespace());
            if requests_doctests(opts) {
                return Ok(false);
            }
            // A walk error (a glob or missing `testpaths` entry, an unreadable
            // dir) or an unresolved --pyargs name only means auto can't vouch
            // for lazy: stay full, never fail a run that didn't ask for lazy.
            // Under --pyargs the walk starts at the resolved packages; the
            // cache then vouches that they are the whole suite it timed.
            let Some(roots) = lazy_roots(python, args) else {
                return Ok(false);
            };
            let Ok(files) = collect::collect_test_files(&roots, &project) else {
                return Ok(false);
            };
            let rel: Vec<String> = files
                .iter()
                .map(|f| {
                    f.strip_prefix(&project.rootdir)
                        .unwrap_or(f)
                        .to_string_lossy()
                        .replace('\\', "/")
                })
                .collect();
            lazy_layout_fits(cache.get(), &rel, n, durations::load_collect())
                && !lazy_measured_slower(&durations::load_mode_walls(), n)
        }),
    }
}

/// Whether pytest will collect doctests (`--doctest-modules`, `--doctest-glob`)
/// from argv, ini `addopts` or `PYTEST_ADDOPTS`. The lazy walk never hands those
/// items to a worker, and a cache written by an earlier lazy run would not list
/// them either, so [`lazy_layout_fits`] alone can't catch doctests enabled later.
pub(super) fn requests_doctests<'a>(mut opts: impl Iterator<Item = &'a str>) -> bool {
    opts.any(|o| o == "--doctest-modules" || o.starts_with("--doctest-glob"))
}

/// Whether the lazy file walk (`files`, rootdir-relative) reproduces the suite
/// the duration cache timed, and spreads well enough for strict file affinity.
/// Only auto-lazy asks: an explicit `--collect lazy` is the user's call.
///
/// Lazy dispatches only `python_files`-matching `.py` files, so it drops items
/// eager collection finds elsewhere (`--doctest-modules` in non-test modules,
/// `--doctest-glob`, plugin-collected non-`.py` files): a cached file the walk
/// lacks means lazy would lose tests. The other direction is fine: a walked
/// file with no cached timing is new, all-skipped or test-free, and one pytest
/// would never recurse into (`norecursedirs`, `collect_ignore`, `--ignore`) is
/// reported empty by the lazy worker, which replays pytest's ignore checks.
///
/// Judged as if lazy never steals (stealing needs an explicit `--dist load`,
/// and this check runs either way), so each file runs whole on one worker: a
/// file whose cached time exceeds an even per-worker share would be the long
/// pole the eager pool avoids by spreading its tests (this also covers
/// `files < n`). Lazy pays that excess but saves each worker most of the
/// whole-suite collection eager repeats per worker: `collect` (cached seconds
/// one worker spends collecting, `durations::COLLECT_FILE`) times
/// `(1 - 1/n)`. The excess may use that saving, never less than
/// [`LAZY_LONG_POLE_SLACK_SECS`] (the whole budget while nothing is recorded).
pub(super) fn lazy_layout_fits(
    cache: &std::collections::HashMap<String, f64>,
    files: &[String],
    n: usize,
    collect: Option<f64>,
) -> bool {
    // A file's weight counts the per-test overhead its cached call times
    // miss: packaging's tests/test_version.py holds 84% of 61k sub-millisecond
    // tests, ~0.4s of call time but ~10s on the one worker lazy pins it to.
    let mut per_file: std::collections::HashMap<&str, f64> = std::collections::HashMap::new();
    for (id, secs) in cache {
        *per_file.entry(crate::text::nodeid_file(id)).or_insert(0.0) +=
            secs + PER_TEST_OVERHEAD_SECS;
    }
    let walked: std::collections::HashSet<&str> = files.iter().map(String::as_str).collect();
    if !per_file.keys().all(|f| walked.contains(f)) {
        return false;
    }
    let total: f64 = per_file.values().sum();
    let longest = per_file.values().copied().fold(0.0, f64::max);
    let saved = collect.unwrap_or(0.0) * (1.0 - 1.0 / n as f64);
    longest - total / n as f64 <= saved.max(LAZY_LONG_POLE_SLACK_SECS)
}

/// How much slower than full collection a recorded lazy run may be before
/// auto-lazy stops picking lazy: run-to-run noise on a busy machine is a few
/// percent.
pub(super) const LAZY_SLOWER_MARGIN: f64 = 1.05;

/// Whether the recorded runs say lazy collection loses here: the last lazy and
/// the last full run both used `n` workers on about the same suite (within 5%
/// of each other's test count, so a narrowed run never decides), and lazy took
/// more than [`LAZY_SLOWER_MARGIN`] times as long. The layout checks above can't
/// see every cost: agent-platform-backend passes them all yet ran 52s lazy
/// against 47s full (its files share one-time setup that full collection
/// pays once per worker). An aged-out record (`RSTEST_WALL_TTL_DAYS`) lets
/// auto try lazy again.
pub(super) fn lazy_measured_slower(walls: &durations::ModeWalls, n: usize) -> bool {
    let (Some(full), Some(lazy)) = (walls.full, walls.lazy) else {
        return false;
    };
    let similar = |a: usize, b: usize| a.abs_diff(b) as f64 <= 0.05 * a.max(b) as f64;
    full.workers == n
        && lazy.workers == n
        && similar(full.tests, lazy.tests)
        && lazy.secs > full.secs * LAZY_SLOWER_MARGIN
}

/// Wall time a lazy long-pole file may add over an even per-worker share
/// before auto-lazy stays eager. An absolute floor, not a ratio: a fast suite's
/// millisecond per-file timings are noise, and a ratio on them would flip the
/// mode from run to run while the pole costs nothing either way.
pub(super) const LAZY_LONG_POLE_SLACK_SECS: f64 = 1.0;

/// The duration cache, loaded on first use and shared for the rest of the run.
/// `durations::load` hashes every cached test source, so the auto-lazy check,
/// the lazy banner, `--shard` file partitioning, and lazy file ordering must
/// not each pay for their own load.
#[derive(Default)]
pub(super) struct DurationCache(
    pub(super) std::cell::OnceCell<std::collections::HashMap<String, f64>>,
);

impl DurationCache {
    pub(super) fn get(&self) -> &std::collections::HashMap<String, f64> {
        self.0.get_or_init(durations::load)
    }
}

/// Steal (split files across workers) only on an EXPLICIT `--dist load`: lazy
/// collection defaults to strict file affinity, since stealing exposes the
/// cross-file / in-file order dependence that affinity hides. A `load` from
/// either the CLI or `[tool.rstest] dist` enables it (by design, a CLI
/// `loadfile` does not override a config `load`).
pub(super) fn lazy_should_steal(cli_dist: Option<&str>, settings_dist: Option<&str>) -> bool {
    cli_dist == Some("load") || settings_dist == Some("load")
}
