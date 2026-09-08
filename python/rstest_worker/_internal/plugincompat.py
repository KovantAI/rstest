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


def _is_dist_internal(plugin: Any) -> bool:
    """pytest-cov and xdist implement master-side hooks for their own
    master<->worker handshakes, which rstest already emulates directly
    (workerinput cov keys, covtool combine). Calling their impls inside a
    worker hits controller state that only exists on a real master."""
    mod = getattr(plugin, "__name__", None) or type(plugin).__module__
    return str(mod).split(".", 1)[0] in ("xdist", "pytest_cov")
