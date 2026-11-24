//! `--incremental` and friends: the narrowed-run config, its budget, and the
//! conflicts it warns about.

use std::io::Write;

use anyhow::Result;

use super::cache::parse_duration_secs;
use super::dispatch::{check_order_shuffle, resolve_shard, resolve_shuffle_seed};
use super::lazy::{collect_lazy, DurationCache};
use super::RunConfig;
use crate::cli::Cli;
use crate::reporting::sink::Sink;
use crate::{config, cov_scope, coverage_skip, remote, select};

/// Resolved dispatch selection (`--shuffle`/`--shard`) plus the `--incremental`
/// dispatch-level skip set and the state the post-run carry-forward needs.
pub(super) struct Incremental {
    pub(super) shuffle_seed: Option<u64>,
    pub(super) shard: Option<(usize, usize)>,
    /// `--budget`, in seconds ([`resolve_budget`]).
    pub(super) budget: Option<u64>,
    /// Lazy collection resolved for this run ([`collect_lazy`]), decided once
    /// here and reused by dispatch.
    pub(super) lazy: bool,
    /// `--incremental` is actually in effect this run (all preconditions met).
    pub(super) active: bool,
    pub(super) config: coverage_skip::ConfigState,
    /// Coverage index snapshotted BEFORE the run (drives skip now, carry-forward after).
    pub(super) prev_index: select::CoverageIndex,
    pub(super) baseline: coverage_skip::Baseline,
    pub(super) skip_ids: std::collections::HashSet<String>,
}

/// Coverage scope + config state for an active incremental run. The measured
/// set comes from the args pytest really runs with (ini `addopts` and
/// `PYTEST_ADDOPTS` included: [`cov_scope::effective_pytest_args`]), and the
/// state folds config/conftest content AND — when coverage measures only part
/// of the tree — every unmeasured first-party file, so editing
/// coverage-invisible code busts the skip set instead of leaving a stale
/// false-green. A run without `--cov` reuses the coverage args of the run that
/// wrote the index. Factored out of [`resolve_incremental`] so the wiring is
/// unit-testable without standing up a full `RunConfig`: dropping the scope
/// here (a default [`cov_scope::CovScope`]) is a silent-false-green regression
/// the seam test catches.
///
/// `root` is the project rootdir the cache belongs to and `scope` the
/// invocation dir. Everything recorded (the fingerprint walk, file keys, the
/// measured set) is relative to `root`, so a run from a subdirectory computes
/// the same state as one from the root and both share one baseline.
pub(super) fn incremental_config(
    root: &std::path::Path,
    scope: &std::path::Path,
    args: &[String],
) -> (cov_scope::CovScope, coverage_skip::ConfigState) {
    let effective = cov_scope::effective_pytest_args(scope, args);
    // The index skipping trusts was written by the last COVERAGE run: without
    // --cov now, rebuild that run's scope from its recorded args and start
    // dir, so files it never measured still fold (see
    // ConfigState::index_cov_args).
    let (index_cov_args, index_cov_cwd) = if coverage_skip::coverage_requested(&effective) {
        let here =
            crate::cache::relative_to(scope, root).map(|p| p.to_string_lossy().replace('\\', "/"));
        (Some(cov_scope::coverage_args(&effective)), here)
    } else {
        match coverage_skip::stored_index_cov(scope) {
            Some((a, cwd)) => (Some(a), cwd),
            None => (None, None),
        }
    };
    let cov_cwd = index_cov_cwd
        .as_deref()
        .map_or_else(|| scope.to_path_buf(), |c| root.join(c));
    let cov = index_cov_args
        .as_ref()
        .map(|a| cov_scope::CovScope::resolve_in(root, &cov_cwd, a))
        .unwrap_or_default();
    let mut state = coverage_skip::config_state(root, &cov);
    state.index_cov_args = index_cov_args;
    state.index_cov_cwd = index_cov_cwd;
    (cov, state)
}

/// Resolve `--shuffle`/`--shard` and compute the `--incremental` skip set in one
/// phase. `--incremental` skipping is restricted to the eager parallel pool on
/// `--dist load` with full collection; incompatible modes disable it with a note
/// (an explicit `--changed`/`--since-green` already narrows selection, so
/// dispatch-level skipping on top is excluded — it would record a PARTIAL green
/// baseline). The coverage index / baseline / skip set are loaded only when the
/// feature is actually active, so the default run path pays nothing.
pub(super) fn resolve_incremental(
    cfg: &RunConfig,
    cli: &Cli,
    settings: &config::RstestSettings,
    args: &[String],
    since_green: bool,
    cache: &DurationCache,
    sink: &mut Sink,
) -> Result<Incremental> {
    let RunConfig {
        n,
        passthrough,
        single_worker_reruns,
        ref dist_name,
        ref scope,
        ..
    } = *cfg;
    // --shuffle reorders the orchestrator's dispatch queue, so it needs the
    // full-collection pool. Refusing (not ignoring) matters: a user probing for
    // order dependence must not get a silently ordered run. `is_lazy` is computed
    // once and reused by the incremental gate below.
    let is_lazy = collect_lazy(cli, settings, dist_name, &cfg.python, args, n, cache, sink)?;
    let shuffle_seed = resolve_shuffle_seed(
        cli.shuffle.as_deref(),
        n,
        passthrough,
        single_worker_reruns,
        is_lazy,
        dist_name,
        sink,
    )?;
    // An explicit fail-fast order and --shuffle both claim the dispatch queue;
    // refuse rather than let the shuffle silently discard the requested order.
    // The --watch auto-pick yields to --shuffle without complaint.
    check_order_shuffle(cfg.order, cfg.order_explicit, shuffle_seed.is_some())?;
    // --shard K/N: partition the suite and keep bucket K. Purely an
    // orchestrator-side node-id (or, in lazy mode, file) filter.
    let shard = resolve_shard(
        cli.shard.as_deref(),
        n,
        passthrough,
        single_worker_reruns,
        shuffle_seed.is_some(),
        dist_name,
    )?;
    let budget = resolve_budget(cli.budget.as_deref(), n, passthrough, is_lazy, dist_name)?;
    let active = cli.incremental
        && !since_green
        && cli.changed.is_none()
        && dist_name.as_str() == "load"
        && !passthrough
        && n >= 2
        && shard.is_none()
        && shuffle_seed.is_none()
        && !is_lazy;
    // The config state is only consumed under `active`; computing it
    // unconditionally would walk the whole project tree for conftests.
    let (cov, config) = if active {
        incremental_config(&crate::cache::base_dir(), scope, args)
    } else {
        Default::default()
    };
    warn_incremental_conflicts(
        sink.err(),
        cli.incremental,
        since_green,
        cli.changed.is_some(),
        active,
    );
    // --incremental relies on the coverage index advancing every run; without
    // --cov this run covtool never rewrites it, so a changed test re-runs on
    // every invocation until a coverage run refreshes the index.
    if active && !coverage_skip::coverage_requested(&cov_scope::effective_pytest_args(scope, args))
    {
        sink.warn(
            "rstest: --incremental without --cov: the coverage index won't be \
             refreshed this run, so changed tests keep re-running until a --cov run",
        );
    }
    // A narrowed --cov=<pkg> (or a coverage include/omit) makes first-party
    // source coverage doesn't measure invisible. That gap is closed soundly:
    // config_state folds a hash of every unmeasured first-party .py, so editing
    // one busts the skip set wholesale. Warn only about the coarseness (any such
    // edit re-runs everything); --cov=. restores per-file granularity.
    if cov.is_partial() {
        sink.warn(
            "rstest: --incremental with a scoped --cov (or coverage include/omit): \
             first-party source coverage doesn't measure is folded into the skip \
             fingerprint, so editing any of it re-runs the whole suite; use --cov=. \
             for per-file incrementality",
        );
        // A scope naming no directory/module under the project can't exempt any
        // file from that fold, so every edit re-runs everything: say which.
        for s in cov.unmatched_sources(scope) {
            sink.warn(&format!(
                "rstest: --incremental: --cov scope `{s}` matches no directory or module \
                 under the project, so every first-party .py counts as out of scope and any \
                 edit re-runs the whole suite"
            ));
        }
    }
    // Snapshot the index BEFORE the run: it drives the skip decision now, and
    // post-run it supplies the cached tests' coverage to fold back in (covtool
    // rewrites the index from only the tests that ran).
    let prev_index = if active {
        remote::load_local_cov_index()
    } else {
        select::CoverageIndex::default()
    };
    // The baseline is loaded once and kept: it drives the skip set now, and its
    // recorded def lines restore the cached (not-run) entries' source line after
    // the run (a cached test has no pytest report to supply one).
    let baseline = if active {
        coverage_skip::load(scope, &config.fp)
    } else {
        coverage_skip::Baseline::default()
    };
    let skip_ids = if active {
        coverage_skip::skippable_now(&prev_index, &baseline, &config)
    } else {
        std::collections::HashSet::new()
    };
    Ok(Incremental {
        shuffle_seed,
        shard,
        budget,
        lazy: is_lazy,
        active,
        config,
        prev_index,
        baseline,
        skip_ids,
    })
}

/// Resolve `--budget DURATION` into seconds. It picks from the id list the
/// pool dispatches, so it needs the parallel pool and full collection, and a
/// dist mode that dispatches tests (`--dist each` runs all of them everywhere).
pub(super) fn resolve_budget(
    budget: Option<&str>,
    n: usize,
    passthrough: bool,
    is_lazy: bool,
    dist_name: &str,
) -> Result<Option<u64>> {
    let Some(spec) = budget else { return Ok(None) };
    let secs = parse_duration_secs(spec).map_err(|e| anyhow::anyhow!("--budget: {e}"))?;
    if secs == 0 {
        anyhow::bail!("--budget must be more than 0s");
    }
    if n <= 1 || passthrough {
        anyhow::bail!(
            "--budget needs the parallel pool (-n >= 2, no -s/--pdb/--co); the \
             single-worker path runs the session's own full suite with no dispatch filter"
        );
    }
    if is_lazy {
        anyhow::bail!(
            "--budget is not supported with --collect lazy: it picks from the full \
             collection (use --collect full)"
        );
    }
    if dist_name == "each" {
        anyhow::bail!("--budget is not supported with --dist each (every worker runs every test)");
    }
    Ok(Some(secs))
}

/// Warn (to `w`) when `--incremental` can't run as requested: both incremental
/// modes select on the same run, so `--since-green`/`--changed` take precedence,
/// and dispatch-level skipping needs the parallel full-collection `--dist load`
/// pool. At most one note fires (the first applicable), mirroring the
/// precedence order.
pub(super) fn warn_incremental_conflicts(
    w: &mut dyn Write,
    incremental: bool,
    since_green: bool,
    changed_some: bool,
    incremental_active: bool,
) {
    if incremental && since_green {
        let _ = writeln!(
            w,
            "rstest: --incremental and --since-green are mutually exclusive; \
             --since-green takes precedence this run"
        );
    } else if incremental && changed_some {
        let _ = writeln!(
            w,
            "rstest: --incremental and --changed are mutually exclusive; \
             --changed owns selection this run"
        );
    } else if incremental && !incremental_active {
        let _ = writeln!(
            w,
            "rstest: --incremental needs the parallel pool with full collection and \
             --dist load (not -n 0/1, --dist each/affinity, --collect lazy, --shard, or \
             --shuffle); running everything this time"
        );
    }
}
