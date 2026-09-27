"""Unit tests for the on-command dispatch plugins: eager (ItemDispatchPlugin)
and lazy (LazyDispatchPlugin)."""

from __future__ import annotations

import hashlib
from types import SimpleNamespace
from typing import Any

import pytest
from rstest_worker._internal.dispatch import ItemDispatchPlugin, LazyDispatchPlugin, _session_roots


class FakeConn:
    """Records send()s and replays queued recv_one() messages."""

    def __init__(self, incoming: list[dict[str, Any]] | None = None) -> None:
        self.sent: list[tuple[str, Any]] = []
        self._incoming = list(incoming or [])

    def send(self, kind: str, payload: Any) -> None:
        self.sent.append((kind, payload))

    def recv_one(self) -> dict[str, Any] | None:
        return self._incoming.pop(0) if self._incoming else None


class FakeItem:
    def __init__(self, nodeid, *, location=None, marks=None, closest=None, config=None):
        self.nodeid = nodeid
        self.location = location or (nodeid.split("::")[0], 1, "domain")
        self._marks = marks or []
        self._closest = closest or {}
        self.config = config

    def iter_markers(self):
        return [SimpleNamespace(name=n) for n in self._marks]

    def get_closest_marker(self, name):
        return self._closest.get(name)


def _digest(ids):
    return hashlib.sha256("\n".join(ids).encode()).hexdigest()


# ── ItemDispatchPlugin.pytest_collection_finish ─────────────────────────────


def test_collection_finish_minimal_payload_without_send_ids(monkeypatch):
    monkeypatch.delenv("RSTEST_SEND_IDS", raising=False)
    conn = FakeConn()
    session = SimpleNamespace(items=[FakeItem("t.py::a"), FakeItem("t.py::b")])
    ItemDispatchPlugin(conn).pytest_collection_finish(session)
    assert conn.sent == [("collection_done", {"count": 2, "hash": _digest(["t.py::a", "t.py::b"])})]


def test_collection_finish_full_payload_with_send_ids(monkeypatch):
    monkeypatch.setenv("RSTEST_SEND_IDS", "1")
    conn = FakeConn()
    flaky_mark = SimpleNamespace(kwargs={"reruns": 3}, args=())
    group_mark = SimpleNamespace(args=("g1",), kwargs={})
    serial_mark = SimpleNamespace(args=(), kwargs={})
    items = [
        FakeItem(
            "t.py::a", location=("t.py", 10, "a"), marks=["serial"], closest={"serial": serial_mark}
        ),
        FakeItem(
            "t.py::b",
            location=("t.py", 20, "b"),
            marks=["flaky"],
            closest={"flaky": flaky_mark, "xdist_group": group_mark},
        ),
    ]
    cache = SimpleNamespace(_cachedir="/tmp/cache")
    session = SimpleNamespace(items=items, config=SimpleNamespace(cache=cache))

    ItemDispatchPlugin(conn).pytest_collection_finish(session)

    kind, payload = conn.sent[0]
    assert kind == "collection_done"
    assert payload["ids"] == ["t.py::a", "t.py::b"]
    assert payload["locations"] == [["t.py", 10], ["t.py", 20]]
    assert payload["marks"] == [["serial"], ["flaky"]]
    assert payload["cache_dir"] == "/tmp/cache"
    assert payload["serial"] == [0]  # item 0 carries the serial marker
    assert payload["flaky"] == {"1": 3}  # item 1, reruns=3
    assert payload["groups"] == {"1": "g1"}


def test_collection_finish_location_none_lineno_and_no_cache(monkeypatch):
    monkeypatch.setenv("RSTEST_SEND_IDS", "1")
    conn = FakeConn()
    item = FakeItem("t.py::a", location=(None, None, "a"))
    session = SimpleNamespace(items=[item], config=SimpleNamespace(cache=None))
    ItemDispatchPlugin(conn).pytest_collection_finish(session)
    payload = conn.sent[0][1]
    assert payload["locations"] == [["", None]]  # None file -> "", lineno passthrough
    assert "cache_dir" not in payload
    assert "flaky" not in payload and "groups" not in payload


def _roots_config(
    rootpath, *, testpaths=(), source="INVOCATION_DIR", pyargs=False, inipath=None, **options
):
    return SimpleNamespace(
        rootpath=rootpath,
        inipath=inipath,
        getini=lambda name: list(testpaths) if name == "testpaths" else None,
        option=SimpleNamespace(pyargs=pyargs, **options),
        args_source=SimpleNamespace(name=source),
        cache=None,
    )


def test_collection_finish_without_a_cacheprovider(monkeypatch):
    # `-p no:cacheprovider` leaves config with no `cache` attribute at all;
    # the id-bearing payload must still ship (no cache_dir), not raise.
    monkeypatch.setenv("RSTEST_SEND_IDS", "1")
    conn = FakeConn()
    session = SimpleNamespace(items=[FakeItem("t.py::a")], config=SimpleNamespace())
    ItemDispatchPlugin(conn).pytest_collection_finish(session)
    kind, payload = conn.sent[0]
    assert kind == "collection_done"
    assert payload["ids"] == ["t.py::a"]
    assert "cache_dir" not in payload


def test_session_roots_absent_without_rootpath():
    # Fake configs (and any config without rootpath) ship no roots.
    assert _session_roots(SimpleNamespace(cache=None)) == {}


def test_session_roots_globs_testpaths_against_the_rootdir(tmp_path):
    (tmp_path / "tests" / "b").mkdir(parents=True)
    (tmp_path / "tests" / "a").mkdir(parents=True)
    (tmp_path / "docs").mkdir()
    cfg = _roots_config(tmp_path, testpaths=["tests/*", "missing"], source="ARGS")
    roots = _session_roots(cfg)
    assert roots["rootdir"] == str(tmp_path)
    assert roots["args_source"] == "args"
    # Globbed from the rootdir (not the cwd), sorted, absolute; no-match
    # entries drop out.
    assert roots["root_args"] == [str(tmp_path / "tests" / "a"), str(tmp_path / "tests" / "b")]


def test_session_roots_escape_glob_characters_in_the_rootdir(tmp_path):
    # A rootdir like `proj[v2]` must be matched literally, or every testpaths
    # glob misses and bisect falls back to collecting the whole rootdir.
    root = tmp_path / "proj[v2]"
    (root / "tests").mkdir(parents=True)
    roots = _session_roots(_roots_config(root, testpaths=["tests"]))
    assert roots["root_args"] == [str(root / "tests")]


def test_session_roots_report_active_order_flags(tmp_path):
    cfg = _roots_config(
        tmp_path, newfirst=True, failedfirst=False, lf=True, stepwise=True, maxfail=1
    )
    assert _session_roots(cfg)["order_flags"] == ["--nf", "--lf", "--sw", "--maxfail"]
    # None active (or the options unknown to this config): the key is absent.
    assert "order_flags" not in _session_roots(_roots_config(tmp_path, maxfail=0))
    assert "order_flags" not in _session_roots(_roots_config(tmp_path))


def test_session_roots_report_the_loaded_config_file(tmp_path):
    ini = tmp_path / "pytest.ini"
    assert _session_roots(_roots_config(tmp_path, inipath=ini))["inifile"] == str(ini)
    # No config file in effect: the key is absent, not null.
    assert "inifile" not in _session_roots(_roots_config(tmp_path))


def test_session_roots_fall_back_to_the_rootdir(tmp_path):
    # No testpaths, or none that match: a no-arg run collects the rootdir.
    assert _session_roots(_roots_config(tmp_path))["root_args"] == [str(tmp_path)]
    cfg = _roots_config(tmp_path, testpaths=["nope/*"])
    assert _session_roots(cfg)["root_args"] == [str(tmp_path)]


def test_session_roots_pyargs_keeps_module_names(tmp_path):
    cfg = _roots_config(tmp_path, testpaths=["pkg.tests"], pyargs=True, source="TESTPATHS")
    roots = _session_roots(cfg)
    assert roots["root_args"] == ["pkg.tests"]
    assert roots["args_source"] == "testpaths"


def test_collection_finish_ships_roots_with_ids(monkeypatch, tmp_path):
    monkeypatch.setenv("RSTEST_SEND_IDS", "1")
    conn = FakeConn()
    session = SimpleNamespace(items=[FakeItem("t.py::a")], config=_roots_config(tmp_path))
    ItemDispatchPlugin(conn).pytest_collection_finish(session)
    payload = conn.sent[0][1]
    assert payload["rootdir"] == str(tmp_path)
    assert payload["args_source"] == "invocation_dir"
    assert payload["root_args"] == [str(tmp_path)]


# ── ItemDispatchPlugin.pytest_runtestloop ───────────────────────────────────


def _eager_session(items, *, collectonly=False, testsfailed=0, continue_on_errors=False):
    hook_calls: list[tuple[str, str | None]] = []

    def protocol(item, nextitem):
        hook_calls.append((item.nodeid, nextitem.nodeid if nextitem else None))

    config = SimpleNamespace(
        hook=SimpleNamespace(pytest_runtest_protocol=protocol),
        option=SimpleNamespace(
            collectonly=collectonly, continue_on_collection_errors=continue_on_errors
        ),
    )
    for it in items:
        it.config = config

    class Interrupted(Exception):
        pass

    session = SimpleNamespace(
        items=items,
        config=config,
        testsfailed=testsfailed,
        shouldfail=False,
        shouldstop=False,
        Interrupted=Interrupted,
    )
    return session, hook_calls


def test_eager_runtestloop_runs_all_items_with_nextitem_scoping():
    items = [FakeItem("a"), FakeItem("b"), FakeItem("c")]
    session, calls = _eager_session(items)
    conn = FakeConn(
        [
            {"kind": "run_items", "payload": {"indices": [0, 1, 2]}},
            {"kind": "no_more_items", "payload": {}},
            {"kind": "end_session", "payload": {}},
        ]
    )
    assert ItemDispatchPlugin(conn).pytest_runtestloop(session) is True
    # nextitem is the successor, None for the last drained item.
    assert calls == [("a", "b"), ("b", "c"), ("c", None)]
    starts = [p["index"] for k, p in conn.sent if k == "item_start"]
    assert starts == [0, 1, 2]


def test_eager_runtestloop_collectonly_returns_early():
    session, calls = _eager_session([FakeItem("a")], collectonly=True)
    conn = FakeConn([{"kind": "run_items", "payload": {"indices": [0]}}])
    assert ItemDispatchPlugin(conn).pytest_runtestloop(session) is True
    assert calls == []  # never ran anything


def test_eager_runtestloop_aborts_on_collection_errors():
    session, _ = _eager_session([FakeItem("a")], testsfailed=2)
    with pytest.raises(session.Interrupted, match="2 errors during collection"):
        ItemDispatchPlugin(FakeConn()).pytest_runtestloop(session)


def test_eager_runtestloop_continue_on_collection_errors():
    session, _ = _eager_session([FakeItem("a")], testsfailed=2, continue_on_errors=True)
    conn = FakeConn([{"kind": "end_session", "payload": {}}])
    # continue flag suppresses the abort -> loop proceeds to end_session.
    assert ItemDispatchPlugin(conn).pytest_runtestloop(session) is True


def test_eager_runtestloop_stops_on_shouldfail():
    items = [FakeItem("a"), FakeItem("b"), FakeItem("c")]
    session, _ = _eager_session(items)
    # Trip -x after the first item runs.
    orig = session.config.hook.pytest_runtest_protocol

    def protocol(item, nextitem):
        orig(item, nextitem)
        session.shouldfail = "maxfail"

    session.config.hook.pytest_runtest_protocol = protocol
    conn = FakeConn([{"kind": "run_items", "payload": {"indices": [0, 1, 2]}}])

    assert ItemDispatchPlugin(conn).pytest_runtestloop(session) is True
    stopped = [p for k, p in conn.sent if k == "stopped"]
    assert stopped == [{"unrun": [1, 2], "reason": "maxfail"}]


def test_eager_runtestloop_orchestrator_vanish_finishes_cleanly():
    items = [FakeItem("a"), FakeItem("b")]
    session, calls = _eager_session(items)
    # run_items for both, then EOF (recv_one -> None) while one is still pending.
    conn = FakeConn([{"kind": "run_items", "payload": {"indices": [0, 1]}}])
    assert ItemDispatchPlugin(conn).pytest_runtestloop(session) is True
    assert calls == [("a", "b")]  # only the head ran before the queue drained


def test_eager_runtestloop_node_down_triggers_foreign_cleanup(monkeypatch):
    session, _ = _eager_session([FakeItem("a")])
    seen: list[Any] = []
    plugin = ItemDispatchPlugin(
        FakeConn(
            [
                {"kind": "node_down", "payload": {"workerinput": {"workerid": "gw1"}}},
                {"kind": "end_session", "payload": {}},
            ]
        )
    )
    monkeypatch.setattr(plugin, "run_foreign_node_down", lambda cfg, payload: seen.append(payload))
    assert plugin.pytest_runtestloop(session) is True
    assert seen == [{"workerinput": {"workerid": "gw1"}}]


# ── LazyDispatchPlugin ──────────────────────────────────────────────────────


def test_lazy_collection_announces_ready_and_short_circuits():
    conn = FakeConn()
    session = SimpleNamespace(
        testscollected=1,
        items=[1],
        config=SimpleNamespace(cache=SimpleNamespace(_cachedir="/c"), rootpath="/r"),
    )
    assert LazyDispatchPlugin(conn).pytest_collection(session) is True
    assert session.testscollected == 0 and session.items == []
    assert conn.sent == [("lazy_ready", {"cache_dir": "/c", "rootdir": "/r"})]


def test_lazy_collection_without_cache():
    conn = FakeConn()
    session = SimpleNamespace(testscollected=0, items=[], config=SimpleNamespace(cache=None))
    LazyDispatchPlugin(conn).pytest_collection(session)
    assert conn.sent == [("lazy_ready", {})]


def test_lazy_collect_file_reports_ids_serial_and_flaky():
    conn = FakeConn()
    flaky_mark = SimpleNamespace(kwargs={"reruns": 2})
    items = [
        FakeItem("t.py::a", closest={"serial": SimpleNamespace()}),
        FakeItem("t.py::b", closest={"flaky": flaky_mark}),
    ]
    session = SimpleNamespace(perform_collect=lambda paths, genitems: items)
    by_id: dict[str, Any] = {}

    n = LazyDispatchPlugin(conn)._collect_file(session, "t.py", by_id)

    assert n == 2
    assert set(by_id) == {"t.py::a", "t.py::b"}
    kind, payload = conn.sent[0]
    assert kind == "file_collected"
    assert payload == {
        "path": "t.py",
        "ids": ["t.py::a", "t.py::b"],
        "serial": ["t.py::a"],
        "flaky": {"t.py::b": 2},
    }


def _lazy_session(perform_collect, *, collectonly=False):
    calls: list[tuple[str, str | None]] = []

    def protocol(item, nextitem):
        calls.append((item.nodeid, nextitem.nodeid if nextitem else None))

    config = SimpleNamespace(
        option=SimpleNamespace(collectonly=collectonly),
        cache=None,
        hook=SimpleNamespace(pytest_runtest_protocol=protocol),
    )

    def collect(paths, genitems):
        items = perform_collect(paths)
        for it in items:
            it.config = config
        return items

    session = SimpleNamespace(
        config=config,
        perform_collect=collect,
        testscollected=0,
        items=[],
        shouldfail=False,
        shouldstop=False,
    )
    return session, calls


def test_lazy_runtestloop_collectonly_returns_early():
    session, _ = _lazy_session(lambda paths: [], collectonly=True)
    assert LazyDispatchPlugin(FakeConn()).pytest_runtestloop(session) is True


def test_lazy_runtestloop_collects_file_then_runs_ids():
    def perform_collect(paths):
        return [FakeItem("t.py::a"), FakeItem("t.py::b")]

    session, calls = _lazy_session(perform_collect)
    conn = FakeConn(
        [
            {"kind": "run_files", "payload": {"paths": ["t.py"]}},
            {"kind": "run_ids", "payload": {"ids": ["t.py::a", "t.py::b"]}},
            {"kind": "no_more_items", "payload": {}},
            {"kind": "end_session", "payload": {}},
        ]
    )
    assert LazyDispatchPlugin(conn).pytest_runtestloop(session) is True
    assert calls == [("t.py::a", "t.py::b"), ("t.py::b", None)]
    assert ("file_collected", {"path": "t.py", "ids": ["t.py::a", "t.py::b"]}) in conn.sent
    assert session.testscollected == 2


def test_lazy_runtestloop_run_ids_recollects_uncollected_file():
    # An id not collected here (steal/redistribution): its whole FILE is
    # collected once on demand.
    collected: list[list[str]] = []

    def perform_collect(paths):
        collected.append(paths)
        return [FakeItem("t.py::a"), FakeItem("t.py::b")]

    session, calls = _lazy_session(perform_collect)
    conn = FakeConn(
        [
            {"kind": "run_ids", "payload": {"ids": ["t.py::a"]}},  # never collected before
            {"kind": "no_more_items", "payload": {}},
            {"kind": "end_session", "payload": {}},
        ]
    )
    assert LazyDispatchPlugin(conn).pytest_runtestloop(session) is True
    assert collected == [["t.py"]]  # collected the id's file
    assert calls == [("t.py::a", None)]


def test_lazy_runtestloop_run_ids_missing_after_recollect_reports_gap():
    # The file re-collects but the exact nodeid isn't produced (different
    # parametrization): report a collect_error rather than silently skipping.
    session, calls = _lazy_session(lambda paths: [FakeItem("t.py::other")])
    conn = FakeConn(
        [
            {"kind": "run_ids", "payload": {"ids": ["t.py::gone"]}},
            {"kind": "end_session", "payload": {}},
        ]
    )
    assert LazyDispatchPlugin(conn).pytest_runtestloop(session) is True
    errors = [p for k, p in conn.sent if k == "collect_error"]
    assert errors and errors[0]["path"] == "t.py::gone"
    assert calls == []


def test_lazy_runtestloop_stops_on_shouldfail():
    items = [FakeItem("t.py::a"), FakeItem("t.py::b")]
    session, _ = _lazy_session(lambda paths: items)
    orig = session.config.hook.pytest_runtest_protocol

    def protocol(item, nextitem):
        orig(item, nextitem)
        session.shouldfail = True

    session.config.hook.pytest_runtest_protocol = protocol
    conn = FakeConn(
        [
            {"kind": "run_files", "payload": {"paths": ["t.py"]}},
            {"kind": "run_ids", "payload": {"ids": ["t.py::a", "t.py::b"]}},
        ]
    )
    assert LazyDispatchPlugin(conn).pytest_runtestloop(session) is True
    stopped = [p for k, p in conn.sent if k == "stopped_ids"]
    assert stopped == [{"unrun": ["t.py::b"]}]


def test_lazy_runtestloop_orchestrator_vanish_finishes_cleanly():
    session, _ = _lazy_session(lambda paths: [])
    assert LazyDispatchPlugin(FakeConn([])).pytest_runtestloop(session) is True
    assert session.testscollected == 0
