"""Neutralization / seeding shims for third-party pytest plugins running
inside a pool worker (which has no xdist master to coordinate them)."""

from __future__ import annotations

import types
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


# ── Dead-master-path detector (--warn-on-dead-master-path) ──────────────────
#
# A whole class of plugins branches on "am I the xdist master?" Under the pool
# every worker carries `config.workerinput`, and once a suite adopts rstest,
# pytest-xdist is usually uninstalled - so those branches resolve wrong two
# ways that share one root cause:
#   * SILENT NO-OP  - master-only behavior gated on `not hasattr(config,
#     "workerinput")` fires nowhere (pytest-html writes no report; no crash).
#   * CRASH PRECURSOR - master-hook *registration* gated on
#     `has_plugin("xdist")` never runs (xdist absent), but the worker branch
#     reads the unprovisioned `workerinput[key]` -> KeyError at collection.
# The negative event ("a branch did not run") is unobservable at runtime, so
# we detect the *condition* statically from each plugin's own code objects,
# keyed on the durable `workerinput` token (set regardless of xdist presence)
# rather than on registered xdist hooks (which vanish once xdist is dropped).
# Detection only - advisory. See warn-dead-master-path-plan.md.

# Plugins that read `workerinput` for behavior rstest FULLY supports; never
# warn on these. (xdist / pytest_cov are also covered by _is_dist_internal.)
_MASTER_PATH_VETTED = frozenset(
    {
        "xdist",
        "pytest_cov",
        "pytest_randomly",
        "pytest_retry",
        "pytest_django",
        "pytest_asyncio",
        "pytest_aiohttp",
        "hypothesis",
        "pytest_mock",
    }
)

# Reading any of these = the plugin is xdist-worker-aware (its master path is
# the counterpart that goes dead under the pool).
_WORKERINPUT_TOKENS = frozenset({"workerinput", "slaveinput", "PYTEST_XDIST_WORKER"})
# Presence of these = master registration is gated on xdist being installed;
# combined with a workerinput read, that is the CRASH-precursor shape.
_XDIST_GATE_NAMES = frozenset({"has_plugin", "hasplugin"})


def _code_tokens(code: types.CodeType) -> set[str]:
    """All names and string constants referenced by a code object, recursing
    into nested code objects (closures, comprehensions, lambdas) so a token
    inside an inner function is still found."""
    tokens: set[str] = set(code.co_names)
    for const in code.co_consts:
        if isinstance(const, str):
            tokens.add(const)
        elif isinstance(const, types.CodeType):
            tokens |= _code_tokens(const)
    return tokens


def _iter_hookimpls(plugin: Any):
    """Yield (hook_name, code_object) for each `pytest_*` callable the plugin
    exposes (module functions or instance methods; both carry `__code__`)."""
    for name in dir(plugin):
        if not name.startswith("pytest_"):
            continue
        try:
            fn = getattr(plugin, name)
        except Exception:
            continue  # property/descriptor side effects - skip defensively
        code = getattr(fn, "__code__", None)
        if isinstance(code, types.CodeType):
            yield name, code


def _is_xdist_gated(tokens: set[str]) -> bool:
    """True if `tokens` show master registration gated on xdist: a
    `has_plugin(...)`/`hasplugin(...)` call naming "xdist", or a read of the
    xdist-only `numprocesses` option."""
    if "numprocesses" in tokens:
        return True
    return bool(_XDIST_GATE_NAMES & tokens) and "xdist" in tokens


def classify_master_path(plugin: Any) -> tuple[str, list[str]] | None:
    """Classify a plugin's master path as ``"crash"``, ``"silent"``, or
    ``None`` (not xdist-worker-aware / not our concern).

    Returns ``(cls, hooks)`` where ``hooks`` are the `pytest_*` hooks whose
    code carried a relevant token. A `workerinput` read is the necessary
    condition for either class; the xdist-registration gate distinguishes the
    crash precursor from the silent no-op.
    """
    signal_hooks: list[str] = []
    all_tokens: set[str] = set()
    for hook, code in _iter_hookimpls(plugin):
        tokens = _code_tokens(code)
        if tokens & _WORKERINPUT_TOKENS or _is_xdist_gated(tokens):
            signal_hooks.append(hook)
            all_tokens |= tokens
    if not (all_tokens & _WORKERINPUT_TOKENS):
        return None  # no worker-branch read -> nothing goes dead under the pool
    cls = "crash" if _is_xdist_gated(all_tokens) else "silent"
    return cls, sorted(signal_hooks)


def _plugin_root(plugin: Any) -> str:
    mod = getattr(plugin, "__name__", None) or type(plugin).__module__
    return str(mod).split(".", 1)[0]


def _plugin_name(plugin: Any) -> str:
    return str(getattr(plugin, "__name__", None) or type(plugin).__name__)


def _is_master_path_vetted(plugin: Any) -> bool:
    return _is_dist_internal(plugin) or _plugin_root(plugin) in _MASTER_PATH_VETTED


def scan_dead_master_paths(plugins: Any) -> list[dict[str, Any]]:
    """Scan an iterable of registered plugins and return one finding per
    non-vetted plugin whose master path goes dead under the pool. Each finding:
    ``{"plugin", "root", "cls", "hooks"}``. Pure/static - no plugin code runs.
    """
    findings: list[dict[str, Any]] = []
    for plugin in plugins:
        if _is_master_path_vetted(plugin):
            continue
        result = classify_master_path(plugin)
        if result is None:
            continue
        cls, hooks = result
        findings.append(
            {
                "plugin": _plugin_name(plugin),
                "root": _plugin_root(plugin),
                "cls": cls,
                "hooks": hooks,
            }
        )
    return findings
