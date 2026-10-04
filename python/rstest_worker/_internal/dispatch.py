"""Orchestrator-driven dispatch plugins layered on StreamPlugin: the pool
worker collects and runs items on command instead of in one collect-then-run
pass. Two models — eager (ItemDispatchPlugin) and lazy (LazyDispatchPlugin)."""

from __future__ import annotations

import os

import pytest

from rstest_worker._internal import messages as m
from rstest_worker._internal.stream import StreamPlugin

# (option dest, CLI spelling) of the options that reorder or cut short a session
# based on the cache: --nf / --ff / --lf (cacheprovider) and stepwise. `-x` /
# `--maxfail` ride along as "--maxfail".
_ORDER_FLAGS = (
    ("newfirst", "--nf"),
    ("failedfirst", "--ff"),
    ("lf", "--lf"),
    ("stepwise", "--sw"),
    ("stepwise_skip", "--sw-skip"),
)


def _maxfail(config) -> int:
    """pytest's resolved -x / --maxfail (argv, ini `addopts` and
    PYTEST_ADDOPTS alike; 0 = no limit). The orchestrator only parses argv,
    so it adopts this to coordinate the stop across every worker."""
    return int(getattr(getattr(config, "option", None), "maxfail", 0) or 0)


def _ignored_by_recursion(session, path: str) -> bool:
    """Whether pytest's directory recursion from the session's initial args
    would skip `path`, a file the orchestrator assigned by explicit path.

    `perform_collect([path])` makes the file an initial path, which bypasses
    `pytest_ignore_collect`: `norecursedirs`, `collect_ignore(_glob)` and
    `--ignore(-glob)` would all be lost, so a `build/` copy of the tests would
    run. Replay the checks `Dir.collect` makes on the way down, from the
    initial arg that contains the file, with the same hook proxies (a
    directory's entries are checked with that directory's conftests) and the
    same exemption for initial paths and their parents. A file outside every
    initial arg, or a session without the needed config, is not ignored.
    """
    config = getattr(session, "config", None)
    invocation = getattr(getattr(config, "invocation_params", None), "dir", None)
    args = getattr(config, "args", None)
    if invocation is None or not args:
        return False
    initial = [(invocation / a.split("::", 1)[0]).absolute() for a in args]
    target = (invocation / path).absolute()
    roots = [r for r in initial if r == target or r in target.parents]
    if not roots:
        return False
    root = max(roots, key=lambda r: len(r.parts))
    chain = [p for p in reversed(target.parents) if root in p.parents] + [target]
    for p in chain:
        # isinitpath: a file only as itself, a directory also as a parent.
        if any(i == p or (p != target and p in i.parents) for i in initial):
            continue
        ihook = session.gethookproxy(p.parent)
        if ihook.pytest_ignore_collect(collection_path=p, config=config):
            return True
    return False


# _DRAINED_NEXTITEM: an item that runs with nothing queued behind it gets the
# Session as its `nextitem`, not pytest's "last item" None. The orchestrator
# may still send a drained worker more work (the serial phase, reruns), and
# None would tear the whole session down, so every later item would set up
# its session fixtures again. The Session keeps session-scoped fixtures alive;
# everything narrower is torn down, since the successor is unknown.
# `_teardown_session` finishes the job when the session ends.


def _teardown_session(item) -> None:
    """Tear down what a drained `item` left set up (_DRAINED_NEXTITEM), as
    its `nextitem=None` teardown would have. A failure is reported as an
    error on `item`, as pytest attributes a session fixture's teardown error
    to the last test. Not a second runtest_teardown: plugins' per-item
    teardown hooks (logging, capture) already ran for this item."""
    from _pytest.runner import CallInfo, get_reraise_exceptions

    call = CallInfo.from_call(
        lambda: item.session._setupstate.teardown_exact(None),
        when="teardown",
        reraise=get_reraise_exceptions(item.config),
    )
    if call.excinfo is not None:
        report = item.ihook.pytest_runtest_makereport(item=item, call=call)
        item.ihook.pytest_runtest_logreport(report=report)


def _session_roots(config) -> m.SessionRootsPayload:
    """pytest's own view of where this session is rooted, for `rstest bisect`:
    the rootdir nodeids are relative to, where the initial args came from, and
    the roots a no-arg run *from the rootdir* would collect (the ini
    `testpaths`, globbed against the rootdir the way pytest does, else the
    rootdir itself), and the config file in effect, so child runs can pin both.
    Empty when the config carries no rootpath."""
    import glob

    rootpath = getattr(config, "rootpath", None)
    if rootpath is None:
        return {}
    root = str(rootpath)
    testpaths = list(config.getini("testpaths"))
    if not getattr(config.option, "pyargs", False):
        testpaths = [
            p
            for t in testpaths
            for p in sorted(glob.iglob(os.path.join(glob.escape(root), t), recursive=True))
        ]
    roots: m.SessionRootsPayload = {
        "rootdir": root,
        "args_source": config.args_source.name.lower(),
        "root_args": testpaths or [root],
    }
    inipath = getattr(config, "inipath", None)
    if inipath is not None:
        roots["inifile"] = str(inipath)
    # The conftest cutoff pytest actually used (a user's --confcutdir, from the
    # command line or addopts, else pytest's default), made absolute against
    # the invocation dir.
    ns = getattr(config, "known_args_namespace", None)
    confcutdir = getattr(ns, "confcutdir", None)
    if confcutdir:
        base = getattr(getattr(config, "invocation_params", None), "dir", None)
        roots["confcutdir"] = str(base / confcutdir) if base is not None else str(confcutdir)
    # Cache-driven reordering/filtering in effect (from the command line, ini
    # `addopts` or PYTEST_ADDOPTS alike): bisect's "victim runs last" needs to
    # know, since some of these can't be switched off from the command line.
    option = getattr(config, "option", None)
    flags = [flag for dest, flag in _ORDER_FLAGS if getattr(option, dest, False)]
    # -x / --maxfail cut a session short at the first unrelated failure.
    maxfail = _maxfail(config)
    if maxfail:
        flags.append("--maxfail")
        roots["maxfail"] = maxfail
    if flags:
        roots["order_flags"] = flags
    return roots


class ItemDispatchPlugin(StreamPlugin):
    """xdist remote.py model: collect everything, run items on command.

    The orchestrator feeds item indices; we keep a pending deque and only run
    an item when its successor is known (`nextitem` drives teardown scoping;
    the wrong nextitem changes fixture finalization order). `no_more_items`
    drains the queue; a drained item gets the Session (_DRAINED_NEXTITEM).
    """

    MIN_PENDING = 2

    def pytest_collection_finish(self, session):
        import hashlib

        ids = [item.nodeid for item in session.items]
        digest = hashlib.sha256("\n".join(ids).encode()).hexdigest()
        payload: m.CollectionDonePayload = {"count": len(ids), "hash": digest}
        if self._deselected:
            payload["deselected"] = self._deselected
        # Full id list rides the wire from ONE worker only (orchestrator needs
        # it once, for duration-cache ordering); the rest verify by hash - at
        # pandas scale that's 8x15MB saved on the startup path.
        if os.environ.get("RSTEST_SEND_IDS") == "1":
            payload["ids"] = ids
            # Source location per item (rootdir-relative file + 0-based line),
            # aligned to `ids`, for --collect-only discovery / editor mapping.
            # item.location is (relpath, lineno, domain); lineno may be None.
            payload["locations"] = [
                [item.location[0] or "", item.location[1]] for item in session.items
            ]
            # Every marker name on each item (own + inherited from class/module),
            # aligned to `ids`, for --collect-only completeness. Names only;
            # serial/flaky/groups below stay separate for scheduling.
            payload["marks"] = [
                sorted({m.name for m in item.iter_markers()}) for item in session.items
            ]
            # No `cache` attribute at all with `-p no:cacheprovider`.
            cache = getattr(session.config, "cache", None)
            if cache is not None:
                payload["cache_dir"] = str(cache._cachedir)
            payload.update(_session_roots(session.config))
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

        # Replicate the guard from pytest's own runtestloop (which this hook
        # replaces): collection errors abort the run unless
        # --continue-on-collection-errors (else 7k jsonschema tests ran past it).
        if session.testsfailed and not session.config.option.continue_on_collection_errors:
            raise session.Interrupted(
                f"{session.testsfailed} error"
                f"{'s' if session.testsfailed != 1 else ''} during collection"
            )
        if session.config.option.collectonly:
            return True
        pending = deque()
        draining = False
        held = None  # last item run with the session as its nextitem
        while True:
            while len(pending) >= (1 if draining else self.MIN_PENDING):
                index = pending.popleft()
                item = session.items[index]
                # Drained (nothing queued behind it): see _DRAINED_NEXTITEM.
                nextitem = session.items[pending[0]] if pending else session
                held = None if pending else item
                # Crash attribution: if this process dies mid-protocol, the
                # orchestrator knows exactly which item took it down
                # (research: xdist infers head-of-pending and misattributes).
                # `timeout` sizes the orchestrator's hang watchdog per test.
                self._conn.send(
                    "item_start", {"index": index, "timeout": self._effective_timeout(item)}
                )
                item.config.hook.pytest_runtest_protocol(item=item, nextitem=nextitem)
                self._conn.send("item_done", {"index": index})
                if session.shouldfail or session.shouldstop:
                    # Session-local -x/--maxfail tripped: stop here, report
                    # what never ran, end the session (orchestrator does
                    # the run-global coordination).
                    self._conn.send(
                        "stopped",
                        {
                            "unrun": list(pending),
                            "reason": str(session.shouldfail or session.shouldstop),
                        },
                    )
                    return True
            # Even after draining, keep listening: a failed item from any
            # worker may be rerun HERE (--reruns). Only end_session (every
            # outcome final) or shutdown closes the session.
            msg = self._conn.recv_one()
            kind = None if msg is None else msg["kind"]
            if kind in (None, "end_session", "shutdown"):
                # None: the orchestrator vanished; finish the session cleanly.
                if held is not None:
                    _teardown_session(held)
                return True
            if kind == "run_items":
                pending.extend(msg["payload"]["indices"])
            elif kind == "node_down":
                # Crash cleanup on behalf of a dead sibling.
                self.run_foreign_node_down(session.config, msg["payload"])
            elif kind == "no_more_items":
                draining = True


class LazyDispatchPlugin(StreamPlugin):
    """D5 lazy collection: no initial collection pass at all.

    The orchestrator assigns FILES, each collected on demand via repeated
    `Session.perform_collect` calls on one persistent Session, so session-scope
    fixtures survive across files and module fixtures tear down at file
    boundaries. Item identity on the wire is the NODEID (no shared index space).
    """

    @pytest.hookimpl(tryfirst=True)
    def pytest_collection(self, session):
        # Replace the initial full collection: work arrives as files.
        session.testscollected = 0
        session.items = []
        payload = {}
        if session.config.cache is not None:
            payload["cache_dir"] = str(session.config.cache._cachedir)
        rootpath = getattr(session.config, "rootpath", None)
        if rootpath is not None:
            payload["rootdir"] = str(rootpath)
        maxfail = _maxfail(session.config)
        if maxfail:
            payload["maxfail"] = maxfail
        self._conn.send("lazy_ready", payload)
        return True

    def _collect_file(self, session, path, items_by_id):
        # Eager recursion would never reach an ignored file: report it empty.
        deselected_before = self._deselected
        items = (
            []
            if _ignored_by_recursion(session, path)
            else session.perform_collect([path], genitems=True)
        )
        ids = [it.nodeid for it in items]
        serial = [it.nodeid for it in items if it.get_closest_marker("serial") is not None]
        payload: m.FileCollectedPayload = {"path": path, "ids": ids}
        if serial:
            payload["serial"] = serial
        flaky = {}
        for it in items:
            mark = it.get_closest_marker("flaky")
            if mark is not None:
                flaky[it.nodeid] = int(mark.kwargs.get("reruns", 1))
        if flaky:
            payload["flaky"] = flaky
        if self._deselected > deselected_before:
            payload["deselected"] = self._deselected - deselected_before
        for it in items:
            items_by_id[it.nodeid] = it
        # Items are NOT queued here: the orchestrator owns dispatch, chunking
        # ids back via run_ids (normally to this worker, where they're cached;
        # to another worker when stealing for balance).
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
        held = None  # last item run with the session as its nextitem
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
            # (nextitem drives fixture teardown scoping) or on drain.
            if len(pending) >= 2 or (draining and pending):
                item = pending.popleft()
                # Drained (nothing queued behind it): see _DRAINED_NEXTITEM.
                nextitem = pending[0] if pending else session
                held = None if pending else item
                self._conn.send(
                    "item_start_id", {"id": item.nodeid, "timeout": self._effective_timeout(item)}
                )
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
            kind = None if msg is None else msg["kind"]
            if kind in (None, "end_session", "shutdown"):
                if held is not None:
                    _teardown_session(held)
                session.testscollected = total
                return True
            if kind == "run_files":
                files.extend(msg["payload"]["paths"])
                draining = False
            elif kind == "run_ids":
                for nid in msg["payload"]["ids"]:
                    it = items_by_id.get(nid)
                    if it is None:
                        # Not collected here (steal / crash redistribution /
                        # serial phase): collect the id's whole FILE once, since
                        # a stolen chunk is usually from one file and per-id
                        # collection would re-parse the module for every id.
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
