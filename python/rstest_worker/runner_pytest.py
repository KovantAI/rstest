"""Run tests through the vendored pytest core, streaming reports to the wire.

The vendored `pytest`/`_pytest` (see python/VENDOR.md) provide full test
semantics — fixtures, parametrize, classes, marks, conftest, plugin loading.
rstest owns what happens around the session: scheduling, output, exit codes.
"""

import os
import sys

import pytest

from rstest_worker import _fixturecompat

_fixturecompat.install()


def _wire_safe(value):
    """msgpack-serializable subset of a workerinput value (xdist requires
    execnet-serializable workerinput; this is the same contract)."""
    if isinstance(value, (str, int, float, bool, type(None))):
        return value
    if isinstance(value, (list, tuple)):
        return [_wire_safe(v) for v in value]
    if isinstance(value, dict):
        return {str(k): _wire_safe(v) for k, v in value.items()}
    return None


def _is_dist_internal(plugin):
    """pytest-cov and xdist implement master-side hooks for their own
    master<->worker handshakes — which rstest already emulates directly
    (workerinput cov keys, covtool combine). Calling their impls inside a
    worker hits controller state that only exists on a real master."""
    mod = getattr(plugin, "__name__", None) or type(plugin).__module__
    return str(mod).split(".", 1)[0] in ("xdist", "pytest_cov")


class _XdistGatewayShim:
    def __init__(self, gid):
        self.id = gid


class _XdistNodeShim:
    """Stands in for xdist's WorkerController in master-side hooks.

    Implementations touch node.workerinput (filled per worker),
    node.gateway.id, and occasionally node.config.
    """

    def __init__(self, config, worker_id):
        self.config = config
        self.workerinput = config.workerinput
        self.gateway = _XdistGatewayShim(worker_id)


class StreamPlugin:
    """Translate pytest report hooks into wire events."""

    def __init__(self, conn):
        self._conn = conn
        self._doctor = os.environ.get("RSTEST_DOCTOR") == "1"
        self._cpu = {}  # nodeid -> call-phase process_time delta
        self._fixtures = {}  # (argname, scope) -> [count, total_secs]
        # (when, category, message, filename, lineno) -> count; aggregated
        # because big suites emit thousands of duplicate warnings.
        self._warnings = {}
        # xdist master-side hook emulation (pytest_configure_node etc.):
        # the shim node standing in for xdist's WorkerController.
        self._xdist_node = None
        self._node_configured = set()  # plugin ids already given configure_node

    @pytest.hookimpl(tryfirst=True)
    def pytest_configure(self, config):
        # Neutralize pytest-xdist if the project's ini/addopts pulls it in
        # (-n in addopts is common): its options must PARSE, but its
        # session must not engage — rstest owns parallelism. tryfirst so
        # this lands before xdist's own configure checks.
        #
        # xdist's DSession only registers when `_is_distribution_mode` is true,
        # i.e. dist != "no" AND tx is set — so forcing dist="no" is sufficient
        # to keep xdist inert. We deliberately do NOT null `numprocesses`:
        # third-party plugins gate their parallel-master setup on it (e.g.
        # pytest-retry: `has_plugin("xdist") and getoption("numprocesses")`
        # starts a ReportServer and stashes its port for workers to read). In
        # rstest's "each worker is its own master" model that branch must fire
        # so the worker self-provisions (its own ephemeral ReportServer); a
        # nulled numprocesses sent it down the client branch and KeyError'd on
        # the never-stashed workerinput["server_port"]. We surface the pool
        # width so master-detection sees the truth; xdist stays off via dist.
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
        # pytest-rerunfailures would rerun flaky-marked tests in-process,
        # doubling rstest's orchestrated reruns. Neutralize it inside pool
        # workers (at -n 0 the plugin keeps its native behavior).
        if os.environ.get("RSTEST_WORKER_ID") is not None:
            plugin = config.pluginmanager.get_plugin("rerunfailures")
            if plugin is not None:
                config.pluginmanager.unregister(plugin)
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
            config.workerinput = {
                "workerid": worker_id,
                "workercount": int(os.environ.get("RSTEST_WORKER_COUNT", "1")),
                # One uid per run, shared by every worker (xdist's
                # testrun_uid contract); the orchestrator provides it.
                "testrun_uid": os.environ.get("RSTEST_RUN_UID", ""),
                "mainargv": sys.argv,
                # pytest-cov's worker mode expects these from the xdist
                # master. Workers share our host and cwd (collocated), so
                # pytest-cov writes suffixed .coverage.* data files and the
                # ORCHESTRATOR combines + reports after the run.
                "cov_master_host": socket.gethostname(),
                "cov_master_topdir": os.getcwd(),
                "cov_master_rsync_roots": [],
            }
            # xdist workers expose this channel dict; pytest-cov (and
            # others) write into it. Nothing reads it here — provided so
            # worker-mode plugin code paths don't crash.
            config.workeroutput = {}
            # Disjoint per-worker tmp roots (xdist popen-gwN pattern);
            # user-provided --basetemp wins.
            basetemp = os.environ.get("RSTEST_BASETEMP")
            if basetemp and not config.option.basetemp:
                from pathlib import Path

                # pytest mkdirs option.basetemp with parents=False —
                # the shared parent must already exist.
                os.makedirs(basetemp, exist_ok=True)
                config.option.basetemp = Path(basetemp) / worker_id
            # xdist MASTER-side hook emulation. Real xdist calls
            # pytest_configure_node(node) on the controller before each
            # worker starts, and implementations fill node.workerinput
            # (sqlalchemy: follower_ident + follower DB provisioning).
            # rstest has no master Python process, so each worker plays
            # master for itself: same workerinput dict, same host, same
            # config — the dominant pattern is semantically identical.
            self._xdist_node = _XdistNodeShim(config, worker_id)
            for plugin in config.pluginmanager.get_plugins():
                self._call_configure_node(plugin, lenient=True)

    def _call_configure_node(self, plugin, lenient=False):
        """Direct-call a plugin's pytest_configure_node against our shim.

        Direct (not via config.hook): sqlalchemy registers its XDistHooks
        DURING its own pytest_configure and reads workerinput["follower_ident"]
        on the very next line — only a synchronous call at registration
        time (pytest_plugin_registered fires inside register()) lands in
        that window.

        Two timing patterns coexist:
          - sqlalchemy: configure_node is self-contained (sets a uuid), so the
            registration-time call succeeds immediately.
          - pytest-retry: it registers XdistHook, THEN on the next lines starts
            a ReportServer and stashes its port; its configure_node READS that
            stash. The registration-time call therefore fires too early and
            raises KeyError. `lenient` swallows that and leaves the plugin
            unmarked so the post-configure sweep (sessionstart) retries it once
            the stash is populated.
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
        (lenient) calls left unconfigured — by now every plugin's own
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
                try:
                    impl(node=shim, error=payload.get("error"))
                except Exception:
                    # Cleanup for a dead sibling must never poison THIS
                    # worker's session.
                    pass

    def _call_node_hooks(self, config, name, **kwargs):
        if self._xdist_node is None:
            return
        for plugin in config.pluginmanager.get_plugins():
            if _is_dist_internal(plugin):
                continue
            impl = getattr(plugin, name, None)
            if impl is not None:
                impl(node=self._xdist_node, **kwargs)

    def pytest_sessionstart(self, session):
        # Retry any configure_node hooks deferred during configure (state they
        # read — e.g. pytest-retry's stashed server_port — is now populated).
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
        # Workers must not write shared last-failed/nodeids caches: N
        # workers each know only THEIR failures, and the last writer would
        # win. The orchestrator writes the merged truth after the run.
        # (sessionstart, not configure: cacheprovider creates config.cache
        # during configure and we run tryfirst there.)
        config = session.config
        if os.environ.get("RSTEST_WORKER_ID") is not None and getattr(config, "cache", None):
            real_set = config.cache.set

            def guarded_set(key, value, _real=real_set):
                if key in ("cache/lastfailed", "cache/nodeids", "cache/stepwise"):
                    return None
                return _real(key, value)

            config.cache.set = guarded_set

    @pytest.hookimpl(wrapper=True)
    def pytest_runtest_call(self, item):
        # Doctor: cpu-vs-wall per call phase. wall >> cpu = the test is
        # waiting (sleep / IO / timeout), the #1 suite-content finding in
        # the research profiling (rich 74%, aiohttp 78% of test time).
        if not self._doctor:
            return (yield)
        import time

        t0 = time.process_time()
        try:
            return (yield)
        finally:
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
        # xdist masters call testnodedown as each worker finishes
        # (sqlalchemy drops its follower DB here). Crash caveat: a dead
        # worker never reaches this — real xdist's master still fires it;
        # rstest cannot (no master Python process).
        self._call_node_hooks(session.config, "pytest_testnodedown", error=None)
        if self._warnings:
            self._conn.send(
                "warnings",
                {
                    "entries": [
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
                },
            )
        if self._doctor and self._fixtures:
            self._conn.send(
                "doctor_fixtures",
                {
                    "fixtures": [
                        {"name": name, "scope": scope, "count": c, "total": round(t, 4)}
                        for (name, scope), (c, t) in self._fixtures.items()
                    ]
                },
            )

    def pytest_runtest_logreport(self, report):
        payload = {
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
        if report.failed and report.sections:
            # Captured stdout/stderr/log — pytest shows these under the
            # failure; ship them only for failures to keep the wire lean.
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


class ItemDispatchPlugin(StreamPlugin):
    """xdist remote.py model: collect everything, run items on command.

    The orchestrator feeds item indices; we keep a pending deque and only
    run an item when its successor is known (`nextitem` drives teardown
    scoping — running with the wrong nextitem changes fixture finalization
    order). `no_more_items` drains the queue, last item gets nextitem=None.
    """

    MIN_PENDING = 2

    def pytest_collection_finish(self, session):
        import hashlib

        ids = [item.nodeid for item in session.items]
        digest = hashlib.sha256("\n".join(ids).encode()).hexdigest()
        payload = {"count": len(ids), "hash": digest}
        # Full id list rides the wire from ONE worker only (the orchestrator
        # needs it once, for duration-cache ordering); the rest verify by
        # hash — at pandas scale that's 8x15MB saved on the startup path.
        if os.environ.get("RSTEST_SEND_IDS") == "1":
            payload["ids"] = ids
            # Source location per item (rootdir-relative file + 0-based line),
            # aligned to `ids` — for --collect-only discovery / editor mapping.
            # item.location is (relpath, lineno, domain); lineno may be None.
            payload["locations"] = [
                [item.location[0] or "", item.location[1]] for item in session.items
            ]
            # Every marker name on each item (own + inherited from class/module),
            # aligned to `ids` — for --collect-only discovery completeness. Names
            # only; serial/flaky/groups below stay separate for scheduling.
            payload["marks"] = [
                sorted({m.name for m in item.iter_markers()}) for item in session.items
            ]
            if session.config.cache is not None:
                payload["cache_dir"] = str(session.config.cache._cachedir)
            payload["serial"] = [
                i
                for i, item in enumerate(session.items)
                if item.get_closest_marker("serial") is not None
            ]
            flaky = {}
            groups = {}
            for i, item in enumerate(session.items):
                mark = item.get_closest_marker("flaky")
                if mark is not None:
                    flaky[str(i)] = int(mark.kwargs.get("reruns", 1))
                gmark = item.get_closest_marker("xdist_group")
                if gmark is not None:
                    name = gmark.args[0] if gmark.args else gmark.kwargs.get("name", "default")
                    groups[str(i)] = str(name)
            if flaky:
                payload["flaky"] = flaky
            if groups:
                payload["groups"] = groups
        self._conn.send("collection_done", payload)

    def pytest_runtestloop(self, session):
        from collections import deque

        # Replicate the guard from pytest's own runtestloop (which this
        # hook replaces): collection errors abort the run unless
        # --continue-on-collection-errors. Missing this ran 7k jsonschema
        # tests past an aborting baseline — found by the corpus.
        if session.testsfailed and not session.config.option.continue_on_collection_errors:
            raise session.Interrupted(
                f"{session.testsfailed} error"
                f"{'s' if session.testsfailed != 1 else ''} during collection"
            )
        if session.config.option.collectonly:
            return True
        pending = deque()
        draining = False
        while True:
            while len(pending) >= (1 if draining else self.MIN_PENDING):
                index = pending.popleft()
                item = session.items[index]
                nextitem = session.items[pending[0]] if pending else None
                # Crash attribution: if this process dies mid-protocol, the
                # orchestrator knows exactly which item took it down
                # (research: xdist infers head-of-pending and misattributes).
                self._conn.send("item_start", {"index": index})
                item.config.hook.pytest_runtest_protocol(item=item, nextitem=nextitem)
                self._conn.send("item_done", {"index": index})
                if session.shouldfail or session.shouldstop:
                    # Session-local -x/--maxfail tripped: stop here, report
                    # what never ran, end the session (orchestrator does
                    # the run-global coordination).
                    self._conn.send(
                        "stopped",
                        {"unrun": list(pending), "reason": str(session.shouldfail or session.shouldstop)},
                    )
                    return True
            # Even after draining, keep listening: a failed item from any
            # worker may be rerun HERE (--reruns). Only end_session (every
            # outcome final) or shutdown closes the session.
            msg = self._conn.recv_one()
            if msg is None:
                return True  # orchestrator vanished; finish session cleanly
            kind = msg["kind"]
            if kind == "run_items":
                pending.extend(msg["payload"]["indices"])
            elif kind == "node_down":
                # Crash cleanup on behalf of a dead sibling.
                self.run_foreign_node_down(session.config, msg["payload"])
            elif kind == "no_more_items":
                draining = True
            elif kind in ("end_session", "shutdown"):
                return True


class LazyDispatchPlugin(StreamPlugin):
    """D5 lazy collection: no initial collection pass at all.

    The orchestrator assigns FILES; each file is collected here on demand
    (`Session.perform_collect` supports repeated per-file calls — the
    Session node persists, so session-scope fixtures survive across files
    and module fixtures tear down exactly at file boundaries via the
    cross-file nextitem chain). Item identity on the wire is the NODEID:
    lazy workers share no index space.
    """

    @pytest.hookimpl(tryfirst=True)
    def pytest_collection(self, session):
        # Replace the initial full collection: work arrives as files.
        session.testscollected = 0
        session.items = []
        payload = {}
        if session.config.cache is not None:
            payload["cache_dir"] = str(session.config.cache._cachedir)
        self._conn.send("lazy_ready", payload)
        return True

    def _collect_file(self, session, path, items_by_id):
        items = session.perform_collect([path], genitems=True)
        ids = [it.nodeid for it in items]
        serial = [it.nodeid for it in items if it.get_closest_marker("serial") is not None]
        payload = {"path": path, "ids": ids}
        if serial:
            payload["serial"] = serial
        flaky = {}
        for it in items:
            mark = it.get_closest_marker("flaky")
            if mark is not None:
                flaky[it.nodeid] = int(mark.kwargs.get("reruns", 1))
        if flaky:
            payload["flaky"] = flaky
        for it in items:
            items_by_id[it.nodeid] = it
        # Items are NOT queued here: the orchestrator owns dispatch (it
        # chunks ids back via run_ids — normally to this worker, where the
        # items are cached; to another worker when stealing for balance).
        self._conn.send("file_collected", payload)
        return len(items)

    def pytest_runtestloop(self, session):
        from collections import deque

        if session.config.option.collectonly:
            return True
        pending = deque()  # collected items ready to run
        files = deque()  # assigned files not yet collected
        items_by_id = {}  # nodeid -> item, for reruns by id
        total = 0
        draining = False
        while True:
            # Collect a queued file ASAP: until collected, its ids are
            # invisible to the orchestrator's dispatch queue.
            if files:
                # A collect error in the file surfaces via collectreport
                # (collect_error on the wire); the orchestrator owns the
                # abort decision.
                total += self._collect_file(session, files.popleft(), items_by_id)
                continue
            # The last pending item runs only when its successor is known
            # (nextitem drives fixture teardown scoping) — or on drain.
            if len(pending) >= 2 or (draining and pending):
                item = pending.popleft()
                nextitem = pending[0] if pending else None
                self._conn.send("item_start_id", {"id": item.nodeid})
                item.config.hook.pytest_runtest_protocol(item=item, nextitem=nextitem)
                self._conn.send("item_done_id", {"id": item.nodeid})
                if session.shouldfail or session.shouldstop:
                    self._conn.send(
                        "stopped_ids",
                        {"unrun": [it.nodeid for it in pending]},
                    )
                    session.testscollected = total
                    return True
                continue
            msg = self._conn.recv_one()
            if msg is None:
                session.testscollected = total
                return True
            kind = msg["kind"]
            if kind == "run_files":
                files.extend(msg["payload"]["paths"])
                draining = False
            elif kind == "run_ids":
                for nid in msg["payload"]["ids"]:
                    it = items_by_id.get(nid)
                    if it is None:
                        # Not collected here (steal / crash redistribution /
                        # serial phase): collect the id's whole FILE once —
                        # the rest of a stolen chunk is usually from the
                        # same file, and per-id collection would re-parse
                        # the module for every id.
                        fpath = nid.split("::", 1)[0]
                        n_before = len(items_by_id)
                        fresh = session.perform_collect([fpath], genitems=True)
                        for f in fresh:
                            items_by_id.setdefault(f.nodeid, f)
                        total += len(items_by_id) - n_before
                        it = items_by_id.get(nid)
                    if it is not None:
                        pending.append(it)
                    else:
                        # The id no longer exists in the file (e.g. a
                        # different parametrize evaluation). Report the gap
                        # rather than running silently short.
                        self._conn.send(
                            "collect_error",
                            {
                                "path": nid,
                                "longrepr": "lazy dispatch: nodeid not found on re-collection",
                            },
                        )
                draining = False
            elif kind == "no_more_items":
                draining = True
            elif kind in ("end_session", "shutdown"):
                session.testscollected = total
                return True


def run_session(args: list[str], conn) -> int:
    """Item-dispatch session (pool mode)."""
    return _contained(lambda: pytest.main(list(args), plugins=[ItemDispatchPlugin(conn)]), conn)


def run_lazy_session(args: list[str], conn) -> int:
    """Lazy-collection session (pool mode, --collect lazy)."""
    return _contained(lambda: pytest.main(list(args), plugins=[LazyDispatchPlugin(conn)]), conn)


def run(args: list[str], conn) -> int:
    """One pytest session over `args`. Returns the session exit status.

    The terminal plugin stays REGISTERED: it owns option definitions
    (`verbose`, `-r`, ...) and the TerminalReporter object that plugins
    reach into (pytest-django, sugar, instafail...). Its output is
    harmless — worker stdout is /dev/null by orchestrator decree.

    """
    return _contained(lambda: pytest.main(list(args), plugins=[StreamPlugin(conn)]), conn)


def _contained(session_fn, conn) -> int:
    """Run a session, never letting exceptions kill the worker process.

    pytest.main can be escaped by BaseExceptions: a conftest doing
    module-level `pytest.importorskip(...)` raises Skipped at CONFIG time
    when any session arg lives under that conftest (file-granular dispatch
    hits this; pandas/tests/io/pytables is the canonical case). Real pytest
    crashes with a raw traceback there; we contain it so one poisoned
    session never kills the worker process or the protocol.
    """
    import traceback

    try:
        return int(session_fn())
    except KeyboardInterrupt:
        # pytest's Interrupted (collection errors, --maxfail interrupts)
        # subclasses KeyboardInterrupt; the per-module errors were already
        # reported via collectreport. Exit code 2, like pytest.
        return 2
    except BaseException as exc:
        conn.send(
            "collect_error",
            {
                "path": f"<session: {type(exc).__name__}>",
                "longrepr": traceback.format_exc(),
            },
        )
        return 1  # observed real-pytest exit for config-time Skipped
