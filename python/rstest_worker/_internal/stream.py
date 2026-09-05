"""Translate pytest report hooks into wire events, and emulate the xdist
master-side node hooks each pool worker must play for itself."""

from __future__ import annotations

import logging
import os
import sys
from typing import Any

import pytest

from rstest_worker._internal import messages as m
from rstest_worker._internal.plugincompat import (
    _is_dist_internal,
    _neutralize_rerunfailures,
    _randomly_seed,
)
from rstest_worker._internal.wire import _wire_safe
from rstest_worker._internal.xdistnode import (
    _call_node_impl,
    _XdistGatewayShim,
    _XdistNodeShim,
)

log = logging.getLogger("rstest.worker")


class Timeout(Exception):
    """Raised in the test's own thread when `--timeout` / `@pytest.mark.timeout`
    fires, so pytest reports it as a failure whose traceback points at the line
    the test was stuck on."""


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
        self._res_base: dict[str, tuple[int, int | None]] = {}
        self._res: dict[str, tuple[int, int | None]] = {}
        # Skip the worker's FIRST test: importing a test module can lazily spin
        # up a persistent thread / open a cache fd once, which is not a per-test
        # leak. Measuring from the 2nd test on drops that first-touch noise.
        self._leak_warmed = False
        self._cpu: dict[str, float] = {}  # nodeid -> call-phase process_time delta
        self._fixtures: dict[tuple[str, str], list[Any]] = {}  # (argname, scope) -> [count, secs]
        # (when, category, message, filename, lineno) -> count; aggregated
        # because big suites emit thousands of duplicate warnings.
        self._warnings: dict[tuple[Any, ...], int] = {}
        # xdist master-side hook emulation (pytest_configure_node etc.):
        # the shim node standing in for xdist's WorkerController.
        self._xdist_node: Any = None
        self._node_configured: set[int] = set()  # plugin ids already given configure_node

    @pytest.hookimpl(tryfirst=True)
    def pytest_cmdline_main(self, config):
        # Unregister pytest-rerunfailures BEFORE pytest_configure: under the pool
        # its configure KeyErrors on the never-stashed sock_port, and configure
        # is call_historic (impl list snapshotted, too late to unregister).
        # cmdline_main is the one clean window; rstest owns reruns natively.
        if os.environ.get("RSTEST_WORKER_ID") is not None:
            _neutralize_rerunfailures(config)
        return None  # tryfirst, non-firstresult: let pytest's own impl run

    @pytest.hookimpl(tryfirst=True)
    def pytest_configure(self, config):
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
        config.addinivalue_line(
            "markers",
            "serial: rstest — run exclusively on one worker, after all "
            "parallel workers have finished (for tests unsafe to parallelize)",
        )
        config.addinivalue_line(
            "markers",
            "flaky(reruns=N): rstest — rerun this test up to N times on "
            "failure (per-test override of --reruns)",
        )
        config.addinivalue_line(
            "markers",
            "xdist_group(name): tests in the same group run on the same "
            "worker under --dist loadgroup (xdist-compatible)",
        )
        config.addinivalue_line(
            "markers",
            "timeout(seconds): rstest — fail this test if its call phase runs "
            "longer than N seconds (per-test override of --timeout)",
        )
        # Belt-and-suspenders: rerunfailures is normally neutralized earlier in
        # pytest_cmdline_main (it must be gone before configure, which snapshots
        # the impl list). This only catches a plugin registered after cmdline_main.
        if os.environ.get("RSTEST_WORKER_ID") is not None:
            _neutralize_rerunfailures(config)
        # When part of a pool, announce ourselves the way an xdist worker
        # would: plugins key per-worker resources on `config.workerinput`
        # (pytest-django suffixes test DB names with workerid, others detect
        # "am I running in parallel?"). Research track 2: 5 of the top 50
        # plugins sniff this attribute.
        worker_id = os.environ.get("RSTEST_WORKER_ID")
        if worker_id is not None:
            import socket

            # The most-grepped xdist env vars: plugins (and conftests we
            # cannot edit) read these directly.
            os.environ.setdefault("PYTEST_XDIST_WORKER", worker_id)
            os.environ.setdefault(
                "PYTEST_XDIST_WORKER_COUNT", os.environ.get("RSTEST_WORKER_COUNT", "1")
            )
            run_uid = os.environ.get("RSTEST_RUN_UID", "")
            config.workerinput = {
                "workerid": worker_id,
                "workercount": int(os.environ.get("RSTEST_WORKER_COUNT", "1")),
                # One uid per run, shared by every worker (xdist's
                # testrun_uid contract); the orchestrator provides it.
                "testrun_uid": run_uid,
                # pytest-randomly's master broadcasts one resolved seed; absent,
                # the plugin KeyErrors at -n >= 2. rstest has no master, so we
                # derive one run-level seed from the shared uid (all workers agree).
                "randomly_seed": _randomly_seed(run_uid),
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
            # Disjoint per-worker tmp roots (xdist popen-gwN pattern);
            # user-provided --basetemp wins.
            basetemp = os.environ.get("RSTEST_BASETEMP")
            if basetemp and not config.option.basetemp:
                from pathlib import Path

                # pytest mkdirs option.basetemp with parents=False, so the
                # shared parent must already exist.
                os.makedirs(basetemp, exist_ok=True)
                config.option.basetemp = Path(basetemp) / worker_id
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
        """`@pytest.mark.timeout(N)` wins over the global `--timeout`."""
        marker = item.get_closest_marker("timeout")
        if marker is not None and marker.args:
            return _parse_timeout(marker.args[0])
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
        if secs is None and not self._doctor:
            return (yield)
        import time

        cancel = self._arm_timeout(secs) if secs else None
        t0 = time.process_time() if self._doctor else 0.0
        try:
            return (yield)
        finally:
            if cancel is not None:
                cancel()
            if self._doctor:
                self._cpu[item.nodeid] = time.process_time() - t0

    @pytest.hookimpl(wrapper=True)
    def pytest_fixture_setup(self, fixturedef, request):
        if not self._doctor or fixturedef.argname == "request":
            return (yield)
        import time

        t0 = time.perf_counter()
        try:
            return (yield)
        finally:
            key = (fixturedef.argname, fixturedef.scope)
            entry = self._fixtures.setdefault(key, [0, 0.0])
            entry[0] += 1
            entry[1] += time.perf_counter() - t0

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
                {"name": name, "scope": scope, "count": c, "total": round(t, 4)}
                for (name, scope), (c, t) in self._fixtures.items()
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
        if report.failed and report.sections:
            # Captured stdout/stderr/log; ship only for failures to keep the
            # wire lean.
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
