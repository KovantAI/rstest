"""Neutralization / seeding shims for third-party pytest plugins running
inside a pool worker (which has no xdist master to coordinate them)."""

from __future__ import annotations

import zlib
from typing import Any


def _randomly_seed(run_uid: Any) -> int:
    """One run-level seed for pytest-randomly, derived from the shared run uid
    so every worker agrees (rstest has no master to broadcast one). 32-bit to
    match pytest-randomly's default; the crc32 fallback for a non-hex/empty uid
    is deterministic across processes (unlike salted builtin hash())."""
    try:
        return int(run_uid, 16) & 0xFFFFFFFF
    except (ValueError, TypeError):
        return zlib.crc32(str(run_uid).encode("utf-8")) & 0xFFFFFFFF


def _random_order_seed(config: Any, run_uid: Any) -> str:
    """The `workerinput["random_order_seed"]` an xdist master would broadcast for
    pytest-random-order.

    Its `pytest_configure` reads that key *unconditionally* whenever
    `workerinput` exists (even with reordering disabled — the default), so a pool
    worker KeyErrors at collection unless we seed it (same dead-master-path class
    as pytest-randomly's `randomly_seed` and pytest-retry's `server_port`).

    All workers must agree on the value: rstest full-collect hash-checks that
    every worker collected the identical order, and a per-worker seed would
    reshuffle differently and trip that check. So when the user did not pin a
    seed (the option is still the plugin's per-process `"default:<rand>"`), derive
    one shared value from the run uid — keeping the `"default:"` prefix so the
    plugin's own `is_enabled` stays False and order is untouched unless the user
    actually asked for `--random-order[-bucket|-seed]`. An explicitly pinned seed
    (no `"default:"` prefix) is honored verbatim."""
    try:
        opt = config.getoption("random_order_seed")
    except Exception:
        opt = None
    if isinstance(opt, str) and not opt.startswith("default:"):
        return opt
    return "default:" + str(_randomly_seed(run_uid))


def _neutralize_rerunfailures(config: Any) -> None:
    """Unregister pytest-rerunfailures so it neither crashes nor double-reruns
    inside a pool worker. Idempotent - safe to call from both cmdline_main and
    configure. See StreamPlugin.pytest_cmdline_main for why timing matters."""
    plugin = config.pluginmanager.get_plugin("rerunfailures")
    if plugin is not None:
        config.pluginmanager.unregister(plugin)


# pytest-retry's pytest11 entrypoint is named "pytest-retry" (hyphen); some
# environments also expose the underscore form. Match either.
_RETRY_PLUGIN_NAMES = ("pytest-retry", "pytest_retry")


def _get_pytest_retry(config: Any) -> Any:
    for name in _RETRY_PLUGIN_NAMES:
        plugin = config.pluginmanager.get_plugin(name)
        if plugin is not None:
            return plugin
    return None


def _neutralize_pytest_retry(config: Any) -> None:
    """Unregister pytest-retry inside a pool worker (fallback for when its
    report channel cannot be seeded). Retries are then rstest's job. Best-effort:
    pytest-retry's pytest_configure is call_historic, so unregistering after the
    snapshot has no effect - this only helps when called before its configure."""
    plugin = _get_pytest_retry(config)
    if plugin is not None:
        config.pluginmanager.unregister(plugin)


def _seed_pytest_retry(config: Any) -> None:
    """Give pytest-retry the `workerinput["server_port"]` an xdist master would.

    pytest-retry gates its ReportServer on `has_plugin("xdist") and
    getoption("numprocesses")`; rstest is not xdist and need not have it
    installed, so in a pool worker that condition is false and the plugin falls
    through to its worker branch - `ClientReporter(workerinput["server_port"])` -
    reading a key no master ever set (KeyError, aborting collection). Stand up
    pytest-retry's own ReportServer inside this worker and hand it the port, so
    each worker serves itself (the "every worker plays master" model, like
    `_randomly_seed` for pytest-randomly's key). Retries execute regardless - the
    retry protocol is independent of the reporter - this only keeps the report
    channel from crashing. No-op if pytest-retry is absent, already seeded, or
    its server API drifts (then fall back to neutralizing it)."""
    if _get_pytest_retry(config) is None:
        return
    # If real pytest-xdist is installed, rstest keeps `numprocesses` visible
    # (see StreamPlugin._neutralize_xdist) so pytest-retry takes its own master
    # branch and self-provisions - seeding here would just orphan a second
    # server. Only step in when that branch cannot run.
    if config.pluginmanager.has_plugin("xdist"):
        return
    workerinput = getattr(config, "workerinput", None)
    if not isinstance(workerinput, dict) or "server_port" in workerinput:
        return
    try:
        from pytest_retry.server import ReportServer  # ty: ignore[unresolved-import]

        server = ReportServer()
        workerinput["server_port"] = server.initialize_server()
        # Keep the server alive for the whole session: its __del__ closes the
        # listening socket, so a dropped reference would refuse the client.
        config._rstest_retry_server = server
    except Exception:
        _neutralize_pytest_retry(config)


# pytest-mypy's pytest11 entrypoint is named "mypy"; match the module too in
# case an environment exposes it under the package name.
_MYPY_PLUGIN_NAMES = ("mypy", "pytest_mypy")


def _get_pytest_mypy(config: Any) -> Any:
    for name in _MYPY_PLUGIN_NAMES:
        plugin = config.pluginmanager.get_plugin(name)
        if plugin is not None:
            return plugin
    return None


def _neutralize_pytest_mypy(config: Any) -> None:
    """Unregister pytest-mypy inside a pool worker (fallback for when its
    results-cache path cannot be seeded). Its mypy items are then simply not
    collected under the pool - run mypy checks at `-n 0`. Best-effort no-op if
    the plugin is absent."""
    plugin = _get_pytest_mypy(config)
    if plugin is not None:
        config.pluginmanager.unregister(plugin)


def _seed_pytest_mypy(config: Any) -> None:
    """Give pytest-mypy the `workerinput["mypy_config_stash_serialized"]` an
    xdist master would broadcast.

    pytest-mypy splits into a controller (a node *without* `workerinput`) that
    allocates the mypy results-cache path, and a worker branch that reads that
    path back from `workerinput["mypy_config_stash_serialized"]`. Under rstest
    every process has `workerinput`, so no process is the controller and the
    worker branch reads a key nobody set - `KeyError` aborting `pytest_configure`
    (same dead-master-path class as pytest-randomly's `randomly_seed` and
    pytest-retry's `server_port`).

    mypy itself is run lazily by the first `MypyFileItem` via
    `MypyResults.from_session` (cache-on-miss, `FileLock`-guarded) - that path
    does not need the controller plugin. So each worker plays master for itself:
    hand it its own unique results-cache path and its own items run mypy on this
    worker's subset (correct isolation; the controller only ever *displayed* the
    summary). We reserve the name the way pytest-mypy's own controller does
    (`NamedTemporaryFile(delete=True)` - the plugin recreates the file on first
    write). Idempotent; no-op if pytest-mypy is absent, the key is already set,
    or real xdist is installed (then rstest keeps `numprocesses` visible and
    pytest-mypy provisions its own controller path). Falls back to neutralizing
    the plugin if the reservation fails."""
    if _get_pytest_mypy(config) is None:
        return
    # With real pytest-xdist installed rstest keeps `numprocesses` visible (see
    # StreamPlugin._neutralize_xdist), so pytest-mypy takes its own controller
    # branch and self-provisions - seeding here would just orphan a second path.
    if config.pluginmanager.has_plugin("xdist"):
        return
    workerinput = getattr(config, "workerinput", None)
    if not isinstance(workerinput, dict) or "mypy_config_stash_serialized" in workerinput:
        return
    try:
        import tempfile

        # Reserve a unique per-worker path; delete=True because we only need the
        # name - pytest-mypy opens/writes it itself (cache-on-miss), exactly as
        # its controller's `NamedTemporaryFile(delete=True)` block does.
        with tempfile.NamedTemporaryFile(prefix="rstest-mypy-", delete=True) as tmp_f:
            path = tmp_f.name
        workerinput["mypy_config_stash_serialized"] = path
    except Exception:
        _neutralize_pytest_mypy(config)


def _is_dist_internal(plugin: Any) -> bool:
    """pytest-cov and xdist implement master-side hooks for their own
    master<->worker handshakes, which rstest already emulates directly
    (workerinput cov keys, covtool combine). Calling their impls inside a
    worker hits controller state that only exists on a real master."""
    mod = getattr(plugin, "__name__", None) or type(plugin).__module__
    return str(mod).split(".", 1)[0] in ("xdist", "pytest_cov")
