"""Translate pytest report hooks into wire events, and emulate the xdist
master-side node hooks each pool worker must play for itself."""

from __future__ import annotations

import hashlib
import inspect
import logging
import os
import sys
from typing import Any

import pytest
from _pytest.fixtures import is_visibility_more_specific

from rstest_worker._internal import messages as m
from rstest_worker._internal.plugincompat import (
    _is_dist_internal,
    _neutralize_rerunfailures,
    _random_order_seed,
    _randomly_seed,
    _seed_pytest_mypy,
    _seed_pytest_retry,
    _warn_pytest_pins,
)
from rstest_worker._internal.wire import _wire_safe
from rstest_worker._internal.xdistnode import (
    _call_node_impl,
    _XdistGatewayShim,
    _XdistNodeShim,
)

log = logging.getLogger("rstest.worker")

# Identity fixtures StreamPlugin defines that pytest-xdist also defines; ours
# must win the override chain (see pytest_sessionstart_identity_fixtures).
_IDENTITY_FIXTURES = ("worker_id", "testrun_uid")

# Sentinel for "no value fingerprinted yet" in the scope-promotion tracker.
_UNSET = object()

# Scope-promotion fingerprinting only accepts immutable builtin values:
# exact scalar types, and tuples/frozensets of them. Anything else (a fresh
# `[]`, a mock, a DataFrame, a lazy QuerySet, a user object) can't be proven
# safe to share by looking at it, and calling its repr runs user code.
_FP_SCALARS = frozenset({str, bytes, int, float, bool, complex, type(None)})
# Total items walked per value (cost cap).
_FP_MAX_ITEMS = 10_000


def _fp_encode(value: Any, budget: list[int]) -> str | None:
    t = type(value)
    if t in _FP_SCALARS:
        # Type-tagged so 1, 1.0 and True stay distinct; builtin reprs are
        # deterministic and run no user code.
        return f"{t.__name__}:{value!r}"
    if t is tuple or t is frozenset:
        budget[0] -= len(value)
        if budget[0] < 0:
            return None
        parts = []
        for item in value:
            enc = _fp_encode(item, budget)
            if enc is None:
                return None
            parts.append(enc)
        if t is frozenset:
            parts.sort()  # iteration order is not part of the value
        return f"{t.__name__}({','.join(parts)})"
    return None


def _fixture_fingerprint(value: Any) -> str | None:
    """A stable-across-calls fingerprint of a fixture's produced value, for
    the scope-promotion advisor, or None when the value can't be proven safe
    to share. Only immutable builtin values qualify (see ``_FP_SCALARS``);
    everything else, and ``None`` itself, returns None. ``None`` is excluded
    because side-effect-only fixtures (reset a global, truncate tables)
    return it every call. None is conservative: the fixture is then never
    flagged constant."""
    if value is None:
        return None
    try:
        enc = _fp_encode(value, [_FP_MAX_ITEMS])
    except (RecursionError, ValueError):
        # ValueError: int repr past sys.get_int_max_str_digits() (3.11+).
        return None
    if enc is None:
        return None
    return hashlib.blake2b(enc.encode("utf-8", "surrogatepass"), digest_size=16).hexdigest()


def _promotable_setup(fixturedef: Any, request: Any, finalizers_before: int | None) -> bool:
    """Whether this successful function-scoped setup could run once per
    session instead: no per-test teardown and no narrower-scoped inputs."""
    # cached_result is (value, cache_key, exc_info). A failed or skipped
    # setup stores (None, key, exc_info): no real value to judge.
    cached = getattr(fixturedef, "cached_result", None)
    if cached is None or cached[2] is not None:
        return False
    func = getattr(fixturedef, "func", None)
    # `@pytest.mark.parametrize` args are served by a synthetic
    # function-scoped fixturedef; there is no fixture to promote.
    if getattr(func, "__name__", "") == "get_direct_param_fixture_func":
        return False
    # Per-test teardown (yield, or request.addfinalizer) does work a constant
    # value can't vouch for. pytest queues both on fixturedef._finalizers
    # during setup; fall back to the yield check if that's unavailable.
    if inspect.isgeneratorfunction(func) or inspect.isasyncgenfunction(func):
        return False
    finalizers = getattr(fixturedef, "_finalizers", None)
    if (
        finalizers_before is not None
        and finalizers is not None
        and len(finalizers) != finalizers_before
    ):
        return False
    # Promoting to session scope requires session-scoped inputs; anything
    # narrower (monkeypatch, tmp_path, a per-test DB) would raise
    # ScopeMismatch, and its per-test effects are the point.
    active = getattr(request, "_fixture_defs", None)
    for name in getattr(fixturedef, "argnames", ()):
        if name == "request":
            continue
        dep = active.get(name) if isinstance(active, dict) else None
        if dep is None or dep.scope != "session":
            return False
    return True


def _spy_dynamic_fetches(request: Any, frame: list[bool]) -> bool:
    """Shadow ``request.getfixturevalue`` so a fixture body fetching a
    narrower-scoped fixture taints ``frame``. The nested-setup check in
    ``pytest_fixture_setup`` misses a fetch pytest serves from its cache (the
    test already requested ``tmp_path``), since no setup hook fires. Returns
    whether the spy was installed (undo with ``del request.getfixturevalue``)."""
    orig = getattr(request, "getfixturevalue", None)
    if not callable(orig):
        return False

    def getfixturevalue(argname: str) -> Any:
        value = orig(argname)
        if argname != "request":
            active = getattr(request, "_fixture_defs", None)
            dep = active.get(argname) if isinstance(active, dict) else None
            if dep is None or dep.scope != "session":
                frame[0] = True
        return value

    try:
        request.getfixturevalue = getfixturevalue
    except AttributeError:
        return False
    return True


class Timeout(BaseException):
    """Raised in the test's own thread when `--timeout` / `@pytest.mark.timeout`
    fires, so pytest reports it as a failure whose traceback points at the line
    the test was stuck on.

    Derives from `BaseException`, not `Exception`, so a test's own broad
    `except Exception` (common in retry loops) can't swallow the deadline —
    matching pytest-timeout, whose `pytest.fail` raises a `BaseException`.
    pytest's call-phase protocol still reports it as a failure with traceback."""


def _parse_timeout(raw: str | float | None) -> float | None:
    """Positive float seconds, or None (disabled / unparseable / non-positive)."""
    if raw is None:
        return None
    try:
        v = float(raw)
    except (TypeError, ValueError):
        return None
    return v if v > 0 else None


def _count_threads() -> int:
    """Live Python thread count (portable). Native C-extension threads that
    bypass the `threading` module are not counted."""
    import threading

    return threading.active_count()


def _count_fds() -> int | None:
    """Open file-descriptor count, or None where it can't be read. `/proc/self/fd`
    on Linux, `/dev/fd` on macOS/BSD; other platforms disable fd tracking."""
    for d in ("/proc/self/fd", "/dev/fd"):
        try:
            return len(os.listdir(d))
        except OSError:
            continue
    return None


class StreamPlugin:
    """Translate pytest report hooks into wire events."""

    def __init__(self, conn: Any) -> None:
        self._conn = conn
        self._doctor = os.environ.get("RSTEST_DOCTOR") == "1"
        # Per-test timeout (--timeout): interrupt the call phase in-process at
        # the deadline. @pytest.mark.timeout(N) overrides per test.
        self._timeout = _parse_timeout(os.environ.get("RSTEST_TIMEOUT"))
        # Resource-leak check (--doctor or --fail-on-leak): snapshot threads/fds
        # before setup and after teardown, ship the net delta on the teardown
        # report.
        self._leakcheck = os.environ.get("RSTEST_LEAKCHECK") == "1"
        # A live JSON consumer (--output json / --stream-json) is attached, so
        # ship captured stdout/stderr/log sections on every report, not only
        # failures (editors show per-passing-test output).
        self._stream_output = os.environ.get("RSTEST_STREAM_OUTPUT") == "1"
        # Measure call-phase CPU (process_time) when doctor asks OR a live JSON
        # consumer is attached — the latter lets editors flag wait-bound tests
        # (wall ≫ cpu) inline without a separate --doctor run. Kept off by
        # default so plain --report-json stays byte-comparable to the pytest
        # baseline (`rstest try`), whose recorder emits no cpu.
        self._measure_cpu = self._doctor or self._stream_output
        self._res_base: dict[str, tuple[int, int | None]] = {}
        self._res: dict[str, tuple[int, int | None]] = {}
        # Skip the worker's FIRST test: importing a test module can lazily spin
        # up a persistent thread / open a cache fd once, which is not a per-test
        # leak. Measuring from the 2nd test on drops that first-touch noise.
        self._leak_warmed = False
        self._cpu: dict[str, float] = {}  # nodeid -> call-phase process_time delta
        # (argname, scope) -> [count, secs, all_constant, first_fingerprint].
        # all_constant/first_fingerprint track the scope-promotion advisor:
        # only function-scoped fixtures whose value fingerprints identically on
        # every call stay `all_constant`.
        self._fixtures: dict[tuple[str, str], list[Any]] = {}
        # Doctor: one frame per fixture setup in progress, innermost last. A
        # frame is [tainted]; a narrower-than-session setup starting inside it
        # (request.getfixturevalue in the body) taints every enclosing frame.
        self._setup_stack: list[list[bool]] = []
        # (when, category, message, filename, lineno) -> count; aggregated
        # because big suites emit thousands of duplicate warnings.
        self._warnings: dict[tuple[Any, ...], int] = {}
        # xdist master-side hook emulation (pytest_configure_node etc.):
        # the shim node standing in for xdist's WorkerController.
        self._xdist_node: Any = None
        self._node_configured: set[int] = set()  # plugin ids already given configure_node

    # Native worker-identity fixtures. pytest-xdist ships `worker_id` /
    # `testrun_uid` fixtures; a suite migrating off xdist that removes it from
    # its config would otherwise lose them (fixture-not-found) even though
    # rstest still populates `workerinput`. We provide them ourselves, with
    # semantics byte-identical to xdist's, so `def test(worker_id)` resolves
    # with or without pytest-xdist installed. When xdist IS installed it also
    # defines these. In a real pool (>= 2 workers) both definitions return the
    # same values, but xdist's report a worker identity whenever `workerinput`
    # exists, which is wrong for the `--reruns` one-worker pool. So ours are
    # moved to the end of the override chain (see
    # `pytest_sessionstart_identity_fixtures`) and win over xdist's.
    @staticmethod
    def _pool_workerinput(config):
        """`config.workerinput` when this worker is one of >= 2 in a pool, else
        None. `--reruns` below `-n 2` runs a one-worker pool that still builds
        workerinput; it is single-worker mode all the same, so the identity
        fixtures must not report a worker identity for it."""
        workerinput = getattr(config, "workerinput", None)
        if workerinput is None or workerinput.get("workercount", 2) < 2:
            return None
        return workerinput

    @pytest.fixture(scope="session")
    def worker_id(self, request):
        """The worker this test runs on: `gw0`, `gw1`, ...; `"master"` below
        `-n 2` (single-worker mode, no worker identity)."""
        workerinput = self._pool_workerinput(request.config)
        if workerinput is not None:
            return workerinput["workerid"]
        return "master"

    @pytest.fixture(scope="session")
    def testrun_uid(self, request):
        """A uid shared by every worker in one run (xdist's `testrun_uid`
        contract). Below `-n 2` there is no run-level uid, so a fresh one is
        generated per session, matching xdist's standalone behavior."""
        workerinput = self._pool_workerinput(request.config)
        if workerinput is not None:
            return workerinput["testrun_uid"]
        import uuid

        return uuid.uuid4().hex

    @pytest.hookimpl(specname="pytest_sessionstart", trylast=True)
    def pytest_sessionstart_identity_fixtures(self, session):
        """Make our `worker_id` / `testrun_uid` win over pytest-xdist's.

        Both are global plugin fixtures, so the last one registered wins, and
        rstest's plugin is registered (via `pytest.main(plugins=...)`) before
        setuptools plugins such as xdist. trylast: the FixtureManager is built
        by pytest's own sessionstart, which must run first. Re-inserting with
        pytest's visibility ordering keeps a conftest or test module override
        of these names winning over ours, as before."""
        fm = getattr(session, "_fixturemanager", None)
        if fm is None:
            return
        for name in _IDENTITY_FIXTURES:
            defs = fm._arg2fixturedefs.get(name)
            if not defs:
                continue
            ours = [fd for fd in defs if getattr(fd.func, "__self__", None) is self]
            for fd in ours:
                defs.remove(fd)
                for i, existing in enumerate(defs):
                    if is_visibility_more_specific(existing, fd):
                        defs.insert(i, fd)
                        break
                else:
                    defs.append(fd)

    @pytest.hookimpl(tryfirst=True)
    def pytest_cmdline_main(self, config):
        # Unregister pytest-rerunfailures BEFORE pytest_configure: under the pool
        # its configure KeyErrors on the never-stashed sock_port, and configure
        # is call_historic (impl list snapshotted, too late to unregister).
        # cmdline_main is the one clean window; rstest owns reruns natively.
        if os.environ.get("RSTEST_WORKER_ID") is not None:
            _neutralize_rerunfailures(config)
        return None  # tryfirst, non-firstresult: let pytest's own impl run

    @pytest.hookimpl(wrapper=True)
    def pytest_load_initial_conftests(self, early_config, parser, args):
        # pytest-cov builds its plugin here, before workerinput exists, so every
        # pool worker first starts a throwaway Central controller that erases the
        # shared plain `.coverage`. N workers deleting one file at once is benign
        # on POSIX but races on Windows (a delete-pending file raises WinError 5
        # and the worker dies at collection). Only gw0 clears the stale file;
        # the rest start as if --cov-append. Workers only write suffixed files,
        # so gw0's erase never touches this run's data.
        ns = early_config.known_args_namespace
        worker_id = os.environ.get("RSTEST_WORKER_ID")
        skip_erase = (
            worker_id not in (None, "gw0")
            and getattr(ns, "cov_source", None)
            and getattr(ns, "cov_append", None) is False
        )
        if skip_erase:
            ns.cov_append = True
        try:
            return (yield)
        finally:
            if skip_erase:
                ns.cov_append = False

    @pytest.hookimpl(tryfirst=True)
    def pytest_configure(self, config):
        self._neutralize_xdist(config)
        self._register_markers(config)
        _warn_pytest_pins(config)
        worker_id = os.environ.get("RSTEST_WORKER_ID")
        if worker_id is None:
            return  # standalone run: nothing pool-specific to set up
        # Belt-and-suspenders: rerunfailures is normally neutralized earlier in
        # pytest_cmdline_main (it must be gone before configure, which snapshots
        # the impl list). This only catches a plugin registered after cmdline_main.
        _neutralize_rerunfailures(config)
        self._build_workerinput(config, worker_id)
        # workerinput now exists; seed pytest-retry's server_port before its own
        # (non-tryfirst) pytest_configure reads it and KeyErrors.
        _seed_pytest_retry(config)
        # Same dead-master-path: pytest-mypy's worker branch reads
        # workerinput["mypy_config_stash_serialized"]. This tryfirst configure
        # runs before the plugin's own, so the key is present when it reads it.
        _seed_pytest_mypy(config)
        self._set_basetemp(config, worker_id)
        self._init_xdist_node(config, worker_id)

    @staticmethod
    def _neutralize_xdist(config):
        # Neutralize pytest-xdist if ini/addopts pulls it in: its options must
        # PARSE but not engage (rstest owns parallelism). dist="no" keeps xdist
        # inert; numprocesses stays set so plugins that gate parallel-master
        # setup on it (pytest-retry) self-provision instead of KeyErroring.
        opt = config.option
        if hasattr(opt, "dist"):
            opt.dist = "no"
        if hasattr(opt, "numprocesses"):
            wc = os.environ.get("RSTEST_WORKER_COUNT")
            opt.numprocesses = int(wc) if (wc and os.environ.get("RSTEST_WORKER_ID")) else None

    @staticmethod
    def _register_markers(config):
        config.addinivalue_line(
            "markers",
            "serial: rstest - run exclusively on one worker, after all "
            "parallel workers have finished (for tests unsafe to parallelize)",
        )
        config.addinivalue_line(
            "markers",
            "flaky(reruns=N): rstest - rerun this test up to N times on "
            "failure (per-test override of --reruns)",
        )
        config.addinivalue_line(
            "markers",
            "xdist_group(name): tests in the same group run on the same "
            "worker under --dist loadgroup (xdist-compatible)",
        )
        config.addinivalue_line(
            "markers",
            "timeout(seconds): rstest - fail this test if its call phase runs "
            "longer than N seconds (per-test override of --timeout)",
        )

    @staticmethod
    def _build_workerinput(config, worker_id):
        # When part of a pool, announce ourselves the way an xdist worker
        # would: plugins key per-worker resources on `config.workerinput`
        # (pytest-django suffixes test DB names with workerid, others detect
        # "am I running in parallel?"). Research track 2: 5 of the top 50
        # plugins sniff this attribute.
        import socket

        # The most-grepped xdist env vars: plugins (and conftests we
        # cannot edit) read these directly. Assigned, not setdefault: a value
        # inherited from the caller's environment would give every worker the
        # same id and collide on per-worker resources.
        os.environ["PYTEST_XDIST_WORKER"] = worker_id
        os.environ["PYTEST_XDIST_WORKER_COUNT"] = os.environ.get("RSTEST_WORKER_COUNT", "1")
        run_uid = os.environ.get("RSTEST_RUN_UID", "")
        os.environ["PYTEST_XDIST_TESTRUNUID"] = run_uid
        config.workerinput = {
            "workerid": worker_id,
            "workercount": int(os.environ.get("RSTEST_WORKER_COUNT", "1")),
            # One uid per run, shared by every worker (xdist's
            # testrun_uid contract); the orchestrator provides it. xdist's own
            # worker and `testrun_uid` fixture read the `testrunuid` key;
            # `testrun_uid` is kept for anything already reading that spelling.
            "testrunuid": run_uid,
            "testrun_uid": run_uid,
            # pytest-randomly's master broadcasts one resolved seed; absent,
            # the plugin KeyErrors at -n >= 2. rstest has no master, so we
            # derive one run-level seed from the shared uid (all workers agree).
            "randomly_seed": _randomly_seed(run_uid),
            # pytest-random-order reads this key unconditionally when workerinput
            # exists (even with reordering off, its default); absent, it KeyErrors
            # at collection. Shared value so all workers agree (full-collect hash).
            "random_order_seed": _random_order_seed(config, run_uid),
            "mainargv": sys.argv,
            # pytest-cov's worker mode expects these from the xdist master.
            # Workers are collocated (same host/cwd), so they write suffixed
            # .coverage.* files and the ORCHESTRATOR combines after the run.
            "cov_master_host": socket.gethostname(),
            "cov_master_topdir": os.getcwd(),
            "cov_master_rsync_roots": [],
        }
        # xdist workers expose this channel dict; pytest-cov and others write
        # into it. Nothing reads it here - provided so plugin paths don't crash.
        config.workeroutput = {}

    @staticmethod
    def _set_basetemp(config, worker_id):
        # Disjoint per-worker tmp roots (xdist popen-gwN pattern);
        # user-provided --basetemp wins.
        basetemp = os.environ.get("RSTEST_BASETEMP")
        if basetemp and not config.option.basetemp:
            from pathlib import Path

            # pytest mkdirs option.basetemp with parents=False, so the
            # shared parent must already exist.
            os.makedirs(basetemp, exist_ok=True)
            config.option.basetemp = Path(basetemp) / worker_id

    def _init_xdist_node(self, config, worker_id):
        # xdist MASTER-side hook emulation: real xdist calls
        # pytest_configure_node(node) before each worker, filling
        # node.workerinput. rstest has no master, so each worker plays its own.
        self._xdist_node = _XdistNodeShim(config, worker_id)
        for plugin in config.pluginmanager.get_plugins():
            self._call_configure_node(plugin, lenient=True)

    def _call_configure_node(self, plugin, lenient=False):
        """Direct-call a plugin's pytest_configure_node against our shim.

        Direct (not via config.hook) so it lands in the registration window:
        sqlalchemy reads workerinput["follower_ident"] on the line after it
        registers XDistHooks, which only a synchronous call reaches.

        pytest-retry instead stashes a ReportServer port AFTER registering, so
        its configure_node KeyErrors if called then; `lenient` swallows that and
        leaves it unmarked for the sessionstart sweep to retry once populated.
        """
        if self._xdist_node is None or _is_dist_internal(plugin):
            return
        impl = getattr(plugin, "pytest_configure_node", None)
        if impl is None or id(plugin) in self._node_configured:
            return
        try:
            impl(self._xdist_node)
        except Exception:
            if lenient:
                return  # state not ready yet; retried strictly at sessionstart
            raise
        self._node_configured.add(id(plugin))

    def _sweep_configure_node(self):
        """Strict retry of any configure_node hooks the registration-time
        (lenient) calls left unconfigured; by now every plugin's own
        pytest_configure has run, so the state they read is populated."""
        if self._xdist_node is None:
            return
        for plugin in self._xdist_node.config.pluginmanager.get_plugins():
            self._call_configure_node(plugin)

    def pytest_plugin_registered(self, plugin, manager):
        # Late registrations (the sqlalchemy mid-configure pattern). Lenient:
        # a hook that reads not-yet-set state (pytest-retry) is retried later.
        self._call_configure_node(plugin, lenient=True)

    def run_foreign_node_down(self, config, payload):
        """pytest_testnodedown for a CRASHED sibling: shim built from the
        dead worker's workerinput snapshot, not ours."""
        winput = payload.get("workerinput") or {}
        shim = _XdistNodeShim.__new__(_XdistNodeShim)
        shim.config = config
        shim.workerinput = winput
        shim.gateway = _XdistGatewayShim(winput.get("workerid", "gw?"))
        for plugin in config.pluginmanager.get_plugins():
            if _is_dist_internal(plugin):
                continue
            impl = getattr(plugin, "pytest_testnodedown", None)
            if impl is not None:
                # Cleanup for a dead sibling must never poison THIS worker's
                # session, so a misbehaving plugin hook is swallowed here — but
                # logged (exc_info) so a real bug leaves a trace instead of
                # vanishing silently.
                try:
                    _call_node_impl(impl, shim, error=payload.get("error"))
                except Exception:
                    log.warning(
                        "dead-sibling pytest_testnodedown hook failed for %r",
                        getattr(plugin, "__class__", type(plugin)).__name__,
                        exc_info=True,
                    )

    def _call_node_hooks(self, config, name, **kwargs):
        if self._xdist_node is None:
            return
        for plugin in config.pluginmanager.get_plugins():
            if _is_dist_internal(plugin):
                continue
            impl = getattr(plugin, name, None)
            if impl is not None:
                # Loud: this is THIS worker's own local node hook, so a genuine
                # bug should fail the session, not vanish (unlike the DEAD
                # sibling's hook that run_foreign_node_down swallows).
                _call_node_impl(impl, self._xdist_node, **kwargs)

    def pytest_sessionstart(self, session):
        # Retry any configure_node hooks deferred during configure (state they
        # read, e.g. pytest-retry's stashed server_port, is now populated).
        self._sweep_configure_node()
        self._call_node_hooks(session.config, "pytest_testnodeready")
        if self._xdist_node is not None:
            # Snapshot for crash cleanup: if this process dies, the
            # orchestrator hands the dict to a surviving worker so
            # pytest_testnodedown still fires with OUR idents.
            self._conn.send(
                "node_input",
                {"workerinput": _wire_safe(self._xdist_node.workerinput)},
            )
        # Workers must not write shared last-failed/nodeids caches: each knows
        # only ITS failures and the last writer would win. The orchestrator
        # writes merged truth. (sessionstart, not configure: cache exists by now.)
        config = session.config
        if os.environ.get("RSTEST_WORKER_ID") is not None and getattr(config, "cache", None):
            real_set = config.cache.set

            def guarded_set(key, value, _real=real_set):
                if key in ("cache/lastfailed", "cache/nodeids", "cache/stepwise"):
                    return None
                return _real(key, value)

            config.cache.set = guarded_set

    def _effective_timeout(self, item) -> float | None:
        """`@pytest.mark.timeout(N)` wins over the global `--timeout`. Accepts
        the positional `timeout(N)` and keyword `timeout(timeout=N)` forms
        (pytest-timeout-compatible)."""
        marker = item.get_closest_marker("timeout")
        if marker is not None:
            if marker.args:
                return _parse_timeout(marker.args[0])
            kwargs = getattr(marker, "kwargs", {})
            if "timeout" in kwargs:
                return _parse_timeout(kwargs["timeout"])
        return self._timeout

    @staticmethod
    def _arm_timeout(secs: float):
        """Interrupt the CURRENT (main) thread after `secs` via SIGALRM, so a
        stuck test fails with a traceback at the line it blocked on. Returns a
        cancel callback, or None where it can't run (no SIGALRM, or the test
        isn't on the main thread) — the orchestrator watchdog is the backstop
        there, and for C-extension calls that never return to the interpreter."""
        import signal
        import threading

        if (
            not hasattr(signal, "SIGALRM")
            or threading.current_thread() is not threading.main_thread()
        ):
            return None

        def _fire(signum, frame):
            raise Timeout(f"test exceeded --timeout ({secs:g}s)")

        old = signal.signal(signal.SIGALRM, _fire)
        signal.setitimer(signal.ITIMER_REAL, secs)

        def cancel():
            signal.setitimer(signal.ITIMER_REAL, 0)
            signal.signal(signal.SIGALRM, old)

        return cancel

    @pytest.hookimpl(wrapper=True)
    def pytest_runtest_setup(self, item):
        # Leak check: baseline thread/fd counts BEFORE any setup fixture runs.
        if self._leakcheck:
            self._res_base[item.nodeid] = (_count_threads(), _count_fds())
        return (yield)

    @pytest.hookimpl(wrapper=True)
    def pytest_runtest_teardown(self, item, nextitem):
        # Leak check: net delta AFTER teardown (a test that opens+closes is 0;
        # one that never releases shows a positive delta). Stashed for the
        # teardown report to carry.
        try:
            return (yield)
        finally:
            if self._leakcheck and item.nodeid in self._res_base:
                bt, bf = self._res_base.pop(item.nodeid)
                if not self._leak_warmed:
                    # First test: warm-up, don't attribute first-touch to it.
                    self._leak_warmed = True
                else:
                    at, af = _count_threads(), _count_fds()
                    fd_delta = (af - bf) if (af is not None and bf is not None) else None
                    self._res[item.nodeid] = (at - bt, fd_delta)

    @pytest.hookimpl(wrapper=True)
    def pytest_runtest_call(self, item):
        # Layers two per-call-phase concerns: the --timeout interrupt (outer)
        # and doctor's cpu-vs-wall measurement (inner). wall >> cpu = the test
        # is waiting (sleep / IO), the #1 suite-content finding in the research
        # profiling (rich 74%, aiohttp 78% of test time).
        secs = self._effective_timeout(item)
        if secs is None and not self._measure_cpu:
            return (yield)
        import time

        cancel = self._arm_timeout(secs) if secs else None
        t0 = time.process_time() if self._measure_cpu else 0.0
        try:
            return (yield)
        finally:
            if cancel is not None:
                cancel()
            if self._measure_cpu:
                self._cpu[item.nodeid] = time.process_time() - t0

    @pytest.hookimpl(wrapper=True)
    def pytest_fixture_setup(self, fixturedef, request):
        if not self._doctor or fixturedef.argname == "request":
            return (yield)
        import time

        fins = getattr(fixturedef, "_finalizers", None)
        fins_before = len(fins) if fins is not None else None
        # Declared argnames are set up before this hook runs, so anything that
        # starts while a frame is open was fetched dynamically from its body.
        if fixturedef.scope != "session":
            for frame in self._setup_stack:
                frame[0] = True
        frame = [False]
        self._setup_stack.append(frame)
        spied = fixturedef.scope == "function" and _spy_dynamic_fetches(request, frame)
        t0 = time.perf_counter()
        try:
            return (yield)
        finally:
            if spied:
                del request.getfixturevalue
            self._setup_stack.pop()
            key = (fixturedef.argname, fixturedef.scope)
            entry = self._fixtures.setdefault(key, [0, 0.0, True, _UNSET])
            entry[0] += 1
            entry[1] += time.perf_counter() - t0
            # Value-identity tracking only matters for function-scoped fixtures
            # (the only promotion candidates); wider scopes are already shared.
            if fixturedef.scope != "function":
                entry[2] = False
            elif entry[2]:
                ok = not frame[0] and _promotable_setup(fixturedef, request, fins_before)
                fp = _fixture_fingerprint(fixturedef.cached_result[0]) if ok else None
                if fp is None:
                    entry[2] = False
                elif entry[3] is _UNSET:
                    entry[3] = fp
                elif entry[3] != fp:
                    entry[2] = False

    def pytest_warning_recorded(self, warning_message, when, nodeid, location):
        m = warning_message
        key = (
            when,
            type(m.message).__name__ if not isinstance(m.message, str) else m.category.__name__,
            str(m.message)[:400],
            m.filename,
            m.lineno,
        )
        self._warnings[key] = self._warnings.get(key, 0) + 1

    def pytest_sessionfinish(self, session, exitstatus):
        # xdist masters call testnodedown as each worker finishes (sqlalchemy
        # drops its follower DB here). Caveat: a crashed worker never reaches
        # this, and rstest has no master to fire it in its place.
        self._call_node_hooks(session.config, "pytest_testnodedown", error=None)
        if self._warnings:
            entries: list[m.WarningEntry] = [
                {
                    "when": when,
                    "category": cat,
                    "message": msg,
                    "filename": fname,
                    "lineno": lineno,
                    "count": count,
                }
                for (when, cat, msg, fname, lineno), count in self._warnings.items()
            ]
            self._conn.send("warnings", {"entries": entries})
        if self._doctor and self._fixtures:
            fixtures: list[m.FixtureStat] = [
                {
                    "name": name,
                    "scope": scope,
                    "count": c,
                    "total": round(t, 4),
                    # Function-scoped and the value never varied across the
                    # calls this worker saw. Not gated on c >= 2: a worker that
                    # ran it once has no evidence against, and must not veto
                    # the merge; `repeated` carries the evidence instead.
                    # An _UNSET fingerprint means tracking never completed
                    # (setup interrupted): no evidence, and not serializable.
                    "constant": (cand := bool(scope == "function" and const and fp is not _UNSET)),
                    # This session compared at least two values.
                    "repeated": cand and c >= 2,
                    # Setup seconds session scope would have skipped in this
                    # session: every call after the first.
                    "redundant": (c - 1) * t / c if cand and c >= 2 else 0.0,
                    # The value itself, so the CLI can veto a fixture whose
                    # value differs between workers (e.g. derived from
                    # request.module under --dist loadfile). Deterministic
                    # across processes: a digest of builtin values only.
                    "fingerprint": fp if cand else None,
                }
                for (name, scope), (c, t, const, fp) in self._fixtures.items()
            ]
            self._conn.send("doctor_fixtures", {"fixtures": fixtures})

    def pytest_runtest_logreport(self, report):
        payload: m.ReportPayload = {
            "nodeid": report.nodeid,
            "when": report.when,
            "outcome": report.outcome,
            "duration": report.duration,
            "longrepr": report.longreprtext or None,
            "wasxfail": hasattr(report, "wasxfail"),
        }
        # report.location is (relpath, lineno, domain); lineno is 0-based and
        # may be None. Ship it for editor mapping (file derives from nodeid).
        location = getattr(report, "location", None)
        if location is not None and location[1] is not None:
            payload["lineno"] = location[1]
        if report.when == "call" and report.nodeid in self._cpu:
            payload["cpu"] = round(self._cpu.pop(report.nodeid), 4)
        if report.when == "teardown" and report.nodeid in self._res:
            dt, df = self._res.pop(report.nodeid)
            if dt:
                payload["thread_delta"] = dt
            if df:
                payload["fd_delta"] = df
        if report.sections and (report.failed or self._stream_output):
            # Captured stdout/stderr/log. Shipped for failures always, and for
            # every outcome when a live JSON consumer asked for it
            # (RSTEST_STREAM_OUTPUT); otherwise omitted to keep the wire lean.
            payload["sections"] = [[name, content[-20000:]] for name, content in report.sections]
        if report.skipped and isinstance(report.longrepr, tuple):
            payload["skip_reason"] = str(report.longrepr[2])[:200]
        self._conn.send("report", payload)

    def pytest_collectreport(self, report):
        if report.failed:
            self._conn.send(
                "collect_error",
                {"path": report.nodeid, "longrepr": report.longreprtext},
            )
        elif report.skipped:
            self._conn.send("collect_skip", {"path": report.nodeid})

    def pytest_internalerror(self, excrepr):
        self._conn.send(
            "collect_error",
            {"path": "<internalerror>", "longrepr": str(excrepr)},
        )
