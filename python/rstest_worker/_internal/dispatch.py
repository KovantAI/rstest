"""Orchestrator-driven dispatch plugins layered on StreamPlugin: the pool
worker collects and runs items on command instead of in one collect-then-run
pass. Two models — eager (ItemDispatchPlugin) and lazy (LazyDispatchPlugin)."""

from __future__ import annotations

import os

import pytest

from rstest_worker._internal import messages as m
from rstest_worker._internal.stream import StreamPlugin


class ItemDispatchPlugin(StreamPlugin):
    """xdist remote.py model: collect everything, run items on command.

    The orchestrator feeds item indices; we keep a pending deque and only run
    an item when its successor is known (`nextitem` drives teardown scoping;
    the wrong nextitem changes fixture finalization order). `no_more_items`
    drains the queue, last item gets nextitem=None.
    """

    MIN_PENDING = 2

    def pytest_collection_finish(self, session):
        import hashlib

        ids = [item.nodeid for item in session.items]
        digest = hashlib.sha256("\n".join(ids).encode()).hexdigest()
        payload: m.CollectionDonePayload = {"count": len(ids), "hash": digest}
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
        self._conn.send("lazy_ready", payload)
        return True

    def _collect_file(self, session, path, items_by_id):
        items = session.perform_collect([path], genitems=True)
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
            elif kind in ("end_session", "shutdown"):
                session.testscollected = total
                return True
