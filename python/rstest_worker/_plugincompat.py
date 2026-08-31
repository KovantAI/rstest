"""Neutralization / seeding shims for third-party pytest plugins running
inside a pool worker (which has no xdist master to coordinate them)."""

import zlib


def _randomly_seed(run_uid):
    """One run-level seed for pytest-randomly, derived from the shared run uid
    so every worker agrees (rstest has no master to broadcast one). 32-bit to
    match pytest-randomly's default; the crc32 fallback for a non-hex/empty uid
    is deterministic across processes (unlike salted builtin hash())."""
    try:
        return int(run_uid, 16) & 0xFFFFFFFF
    except (ValueError, TypeError):
        return zlib.crc32(str(run_uid).encode("utf-8")) & 0xFFFFFFFF


def _neutralize_rerunfailures(config):
    """Unregister pytest-rerunfailures so it neither crashes nor double-reruns
    inside a pool worker. Idempotent - safe to call from both cmdline_main and
    configure. See StreamPlugin.pytest_cmdline_main for why timing matters."""
    plugin = config.pluginmanager.get_plugin("rerunfailures")
    if plugin is not None:
        config.pluginmanager.unregister(plugin)


def _is_dist_internal(plugin):
    """pytest-cov and xdist implement master-side hooks for their own
    master<->worker handshakes, which rstest already emulates directly
    (workerinput cov keys, covtool combine). Calling their impls inside a
    worker hits controller state that only exists on a real master."""
    mod = getattr(plugin, "__name__", None) or type(plugin).__module__
    return str(mod).split(".", 1)[0] in ("xdist", "pytest_cov")
