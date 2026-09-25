"""Unit tests for StreamPlugin seams not covered by the leakcheck/timeout
suites: cmdline_main, xdist node-hook plumbing, warning aggregation,
sessionfinish emission, the doctor fixture timer, and report payloads."""

from __future__ import annotations

import contextlib
from types import SimpleNamespace
from typing import Any

import pytest
from rstest_worker._internal import stream
from rstest_worker._internal.stream import StreamPlugin


class FakeConn:
    def __init__(self) -> None:
        self.sent: list[tuple[str, Any]] = []

    def send(self, kind: str, payload: Any) -> None:
        self.sent.append((kind, payload))


def _plugin() -> StreamPlugin:
    return StreamPlugin(FakeConn())


def mk_report(
    when,
    outcome,
    *,
    nodeid="t.py::a",
    failed=False,
    skipped=False,
    longreprtext="",
    longrepr=None,
    sections=None,
    location=("t.py", 1, "a"),
):
    return SimpleNamespace(
        nodeid=nodeid,
        when=when,
        outcome=outcome,
        duration=0.0,
        longreprtext=longreprtext,
        longrepr=longrepr,
        failed=failed,
        skipped=skipped,
        sections=sections or [],
        location=location,
    )


# ── pytest_cmdline_main: rerunfailures neutralization ───────────────────────


def test_cmdline_main_neutralizes_rerunfailures_in_worker(monkeypatch):
    seen: list[Any] = []
    monkeypatch.setattr(stream, "_neutralize_rerunfailures", lambda c: seen.append(c))
    monkeypatch.setenv("RSTEST_WORKER_ID", "gw0")
    config = object()
    assert _plugin().pytest_cmdline_main(config) is None
    assert seen == [config]


def test_cmdline_main_noop_outside_worker(monkeypatch):
    seen: list[Any] = []
    monkeypatch.setattr(stream, "_neutralize_rerunfailures", lambda c: seen.append(c))
    monkeypatch.delenv("RSTEST_WORKER_ID", raising=False)
    _plugin().pytest_cmdline_main(object())
    assert seen == []


# ── worker_id / testrun_uid native fixtures ─────────────────────────────────


def _fixture_fn(name):
    """Underlying function behind a @pytest.fixture-decorated method."""
    return getattr(StreamPlugin, name).__wrapped__


def test_worker_id_returns_workerid_from_workerinput():
    p = _plugin()
    request = SimpleNamespace(config=SimpleNamespace(workerinput={"workerid": "gw3"}))
    assert _fixture_fn("worker_id")(p, request) == "gw3"


def test_worker_id_falls_back_to_master_without_workerinput():
    p = _plugin()
    request = SimpleNamespace(config=SimpleNamespace())  # no workerinput attr
    assert _fixture_fn("worker_id")(p, request) == "master"


def test_worker_id_is_master_in_one_worker_pool():
    # --reruns below -n 2 runs a one-worker pool that still builds workerinput.
    p = _plugin()
    request = SimpleNamespace(
        config=SimpleNamespace(workerinput={"workerid": "gw0", "workercount": 1})
    )
    assert _fixture_fn("worker_id")(p, request) == "master"


def test_testrun_uid_returns_uid_from_workerinput():
    p = _plugin()
    request = SimpleNamespace(config=SimpleNamespace(workerinput={"testrun_uid": "abc123"}))
    assert _fixture_fn("testrun_uid")(p, request) == "abc123"


def test_testrun_uid_is_fresh_in_one_worker_pool():
    p = _plugin()
    request = SimpleNamespace(
        config=SimpleNamespace(workerinput={"testrun_uid": "abc123", "workercount": 1})
    )
    uid = _fixture_fn("testrun_uid")(p, request)
    assert uid != "abc123" and len(uid) == 32


def test_testrun_uid_generates_fresh_hex_without_workerinput():
    p = _plugin()
    request = SimpleNamespace(config=SimpleNamespace())  # no workerinput attr
    uid = _fixture_fn("testrun_uid")(p, request)
    assert len(uid) == 32 and int(uid, 16) >= 0  # valid uuid4 hex
    # fresh each call
    assert uid != _fixture_fn("testrun_uid")(p, request)


# ── _call_configure_node / plugin_registered ───────────────────────────────


def test_call_configure_node_invokes_once_and_marks(monkeypatch):
    monkeypatch.setattr(stream, "_is_dist_internal", lambda pl: False)
    p = _plugin()
    p._xdist_node = SimpleNamespace()  # truthy shim
    seen: list[Any] = []
    plugin = SimpleNamespace(pytest_configure_node=lambda node: seen.append(node))

    p._call_configure_node(plugin)
    assert seen == [p._xdist_node]
    assert id(plugin) in p._node_configured

    p._call_configure_node(plugin)  # already configured -> skipped
    assert len(seen) == 1


def test_call_configure_node_skips_without_shim():
    p = _plugin()  # _xdist_node is None
    plugin = SimpleNamespace(pytest_configure_node=lambda node: pytest.fail("called"))
    p._call_configure_node(plugin)  # returns early, no call


def test_call_configure_node_skips_plugin_without_hook(monkeypatch):
    monkeypatch.setattr(stream, "_is_dist_internal", lambda pl: False)
    p = _plugin()
    p._xdist_node = SimpleNamespace()
    p._call_configure_node(SimpleNamespace())  # no pytest_configure_node attr


def test_call_configure_node_lenient_swallows_error(monkeypatch):
    monkeypatch.setattr(stream, "_is_dist_internal", lambda pl: False)
    p = _plugin()
    p._xdist_node = SimpleNamespace()

    def boom(node):
        raise RuntimeError("state not ready")

    plugin = SimpleNamespace(pytest_configure_node=boom)
    p._call_configure_node(plugin, lenient=True)  # swallowed
    assert id(plugin) not in p._node_configured
    with pytest.raises(RuntimeError):  # strict re-raises
        p._call_configure_node(plugin)


def test_plugin_registered_defers_to_configure_node(monkeypatch):
    p = _plugin()
    seen: list[Any] = []
    monkeypatch.setattr(
        p, "_call_configure_node", lambda pl, lenient=False: seen.append((pl, lenient))
    )
    plugin = object()
    p.pytest_plugin_registered(plugin, manager=None)
    assert seen == [(plugin, True)]


# ── _call_node_hooks / run_foreign_node_down ───────────────────────────────


def test_call_node_hooks_noop_without_shim():
    p = _plugin()  # _xdist_node None
    config = SimpleNamespace(pluginmanager=SimpleNamespace(get_plugins=lambda: [1 / 0]))
    p._call_node_hooks(config, "pytest_testnodeready")  # returns before touching plugins


def test_call_node_hooks_invokes_local_hook(monkeypatch):
    monkeypatch.setattr(stream, "_is_dist_internal", lambda pl: False)
    p = _plugin()
    p._xdist_node = SimpleNamespace(workerid="gw0")
    seen: list[Any] = []
    plugin = SimpleNamespace(pytest_testnodeready=lambda node: seen.append(node))
    config = SimpleNamespace(pluginmanager=SimpleNamespace(get_plugins=lambda: [plugin]))
    p._call_node_hooks(config, "pytest_testnodeready")
    assert seen == [p._xdist_node]


def test_run_foreign_node_down_calls_testnodedown(monkeypatch):
    monkeypatch.setattr(stream, "_is_dist_internal", lambda pl: False)
    p = _plugin()
    seen: list[Any] = []
    plugin = SimpleNamespace(
        pytest_testnodedown=lambda node, error=None: seen.append((node.workerinput, error))
    )
    config = SimpleNamespace(pluginmanager=SimpleNamespace(get_plugins=lambda: [plugin]))
    p.run_foreign_node_down(config, {"workerinput": {"workerid": "gw3"}, "error": "boom"})
    assert seen == [({"workerid": "gw3"}, "boom")]


def test_run_foreign_node_down_swallows_hook_error(monkeypatch):
    monkeypatch.setattr(stream, "_is_dist_internal", lambda pl: False)
    p = _plugin()

    def boom(node, error=None):
        raise RuntimeError("dead sibling hook blew up")

    plugin = SimpleNamespace(pytest_testnodedown=boom)
    config = SimpleNamespace(pluginmanager=SimpleNamespace(get_plugins=lambda: [plugin]))
    # Must not propagate (cleanup for a dead sibling can't poison this worker).
    p.run_foreign_node_down(config, {"workerinput": {}, "error": None})


def test_run_foreign_node_down_skips_dist_internal(monkeypatch):
    monkeypatch.setattr(stream, "_is_dist_internal", lambda pl: True)
    p = _plugin()
    plugin = SimpleNamespace(pytest_testnodedown=lambda node, error=None: pytest.fail("called"))
    config = SimpleNamespace(pluginmanager=SimpleNamespace(get_plugins=lambda: [plugin]))
    p.run_foreign_node_down(config, {"workerinput": {}})  # skipped, no call


def test_sweep_configure_node_retries_all_plugins(monkeypatch):
    p = _plugin()
    plugin = object()
    p._xdist_node = SimpleNamespace(
        config=SimpleNamespace(pluginmanager=SimpleNamespace(get_plugins=lambda: [plugin]))
    )
    seen: list[Any] = []
    monkeypatch.setattr(p, "_call_configure_node", lambda pl: seen.append(pl))
    p._sweep_configure_node()
    assert seen == [plugin]  # strict retry of each plugin


def test_sweep_configure_node_noop_without_shim():
    p = _plugin()  # _xdist_node None
    p._sweep_configure_node()  # returns early, no crash


# ── pytest_sessionstart: node_input snapshot for crash cleanup ─────────────


def test_sessionstart_ships_node_input_snapshot(monkeypatch):
    monkeypatch.delenv("RSTEST_WORKER_ID", raising=False)
    p = _plugin()
    monkeypatch.setattr(p, "_sweep_configure_node", lambda: None)
    monkeypatch.setattr(p, "_call_node_hooks", lambda *a, **k: None)
    p._xdist_node = SimpleNamespace(workerinput={"workerid": "gw0", "workercount": 4})
    session = SimpleNamespace(config=SimpleNamespace(cache=None))
    p.pytest_sessionstart(session)
    node_inputs = [pl for k, pl in p._conn.sent if k == "node_input"]
    assert node_inputs == [{"workerinput": {"workerid": "gw0", "workercount": 4}}]


# ── _build_workerinput: dead-master-path seed coverage (no-xdist) ───────────


class _WiConfig:
    """Minimal config for _build_workerinput: settable workerinput/workeroutput
    plus getoption (consumed by _random_order_seed)."""

    def __init__(self):
        self.workerinput = None
        self.workeroutput = None

    def getoption(self, name):
        return None


# Every `workerinput[<key>]` a master-gated third-party plugin reads *by direct
# subscript* on its worker branch — the dead-master-path crash class. With
# pytest-xdist uninstalled (the adopted state) no master ever stages these, so a
# pool worker KeyErrors at collection unless _build_workerinput seeds it. Audited
# against the installed gate-venv plugins (2026-09-17); grouped by who provisions
# the key. If a plugin adds a new key, extend the audit AND the seed together —
# this guard fails first so the gap can't ship silently.
_BUILD_SEEDED_KEYS = frozenset(
    {
        "randomly_seed",  # pytest-randomly
        "random_order_seed",  # pytest-random-order
        "workerid",  # pytest-cov + generic "am I a worker?" sniffers
        "workercount",  # pytest-cov, xdist-compat sniffers
        "mainargv",  # xdist-compat prog-name reconstruction
        "testrun_uid",  # shared run id (xdist testrun_uid contract)
        "cov_master_host",  # pytest-cov worker mode
        "cov_master_topdir",  # pytest-cov worker mode
    }
)
# Crash-class keys NOT seeded here because another mechanism owns them — listed
# so the audit is exhaustive, not to assert on:
#   server_port      -> _seed_pytest_retry (stands up pytest-retry's server)
#   follower_ident   -> _XdistNodeShim configure_node emulation (sqlalchemy)
#   sock_port,       -> pytest-rerunfailures, unregistered wholesale
#   statusdb_token       (_neutralize_rerunfailures)
#   testrunuid       -> read only by xdist itself; absent when xdist uninstalled


def test_build_workerinput_seeds_every_direct_seed_crash_key(monkeypatch):
    monkeypatch.setenv("RSTEST_WORKER_COUNT", "4")
    monkeypatch.setenv("RSTEST_RUN_UID", "abc123")
    config = _WiConfig()
    StreamPlugin._build_workerinput(config, "gw0")
    assert config.workerinput is not None
    missing = _BUILD_SEEDED_KEYS - set(config.workerinput)
    assert not missing, f"unseeded dead-master-path keys (no-xdist KeyError risk): {missing}"
    # workeroutput must exist too — plugins (pytest-cov) write into it.
    assert config.workeroutput == {}


# ── pytest_warning_recorded: aggregation ───────────────────────────────────


def test_warning_recorded_aggregates_duplicates():
    p = _plugin()
    wm = SimpleNamespace(
        message="deprecated thing",
        category=SimpleNamespace(__name__="DeprecationWarning"),
        filename="f.py",
        lineno=5,
    )
    p.pytest_warning_recorded(wm, "runtest", "t.py::a", ("f.py", 5, ""))
    p.pytest_warning_recorded(wm, "runtest", "t.py::a", ("f.py", 5, ""))
    assert list(p._warnings.values()) == [2]  # same key -> counted twice
    (key,) = p._warnings
    assert key == ("runtest", "DeprecationWarning", "deprecated thing", "f.py", 5)


def test_warning_recorded_uses_message_type_for_nonstring():
    p = _plugin()

    class MyWarning(Warning):
        pass

    wm = SimpleNamespace(
        message=MyWarning("x"),
        category=SimpleNamespace(__name__="ignored"),
        filename="g.py",
        lineno=1,
    )
    p.pytest_warning_recorded(wm, "collect", "t.py", ("g.py", 1, ""))
    (key,) = p._warnings
    assert key[1] == "MyWarning"  # type(message).__name__, not category


# ── pytest_sessionfinish: warnings + doctor fixtures emission ───────────────


def test_sessionfinish_emits_warnings():
    p = _plugin()  # _xdist_node None -> node hooks are a no-op
    p._warnings = {("runtest", "UserWarning", "msg", "f.py", 3): 4}
    p.pytest_sessionfinish(session=SimpleNamespace(config=SimpleNamespace()), exitstatus=0)
    kinds = {k for k, _ in p._conn.sent}
    assert "warnings" in kinds
    entries = next(pl for k, pl in p._conn.sent if k == "warnings")["entries"]
    assert entries == [
        {
            "when": "runtest",
            "category": "UserWarning",
            "message": "msg",
            "filename": "f.py",
            "lineno": 3,
            "count": 4,
        }
    ]


def test_sessionfinish_emits_doctor_fixtures(monkeypatch):
    monkeypatch.setenv("RSTEST_DOCTOR", "1")
    p = _plugin()  # _doctor read from env at init
    # [count, secs, all_constant, first_fingerprint]; session scope is never a
    # promotion candidate, so `constant` in the payload is False regardless.
    p._fixtures = {("db", "session"): [3, 1.23456, True, "fp"]}
    p.pytest_sessionfinish(session=SimpleNamespace(config=SimpleNamespace()), exitstatus=0)
    fixtures = next(pl for k, pl in p._conn.sent if k == "doctor_fixtures")["fixtures"]
    assert fixtures == [
        {
            "name": "db",
            "scope": "session",
            "count": 3,
            "total": 1.2346,
            "constant": False,
            "repeated": False,
            "redundant": 0.0,
            "fingerprint": None,
        }
    ]


def test_sessionfinish_flags_constant_function_fixture(monkeypatch):
    monkeypatch.setenv("RSTEST_DOCTOR", "1")
    p = _plugin()
    # function scope, ran twice, value-constant => promotion candidate.
    p._fixtures = {
        ("cfg", "function"): [4, 2.0, True, "fp"],
        ("varies", "function"): [2, 0.5, False, "fp"],  # value changed => not
        # One call is "no evidence against": reported constant so it can't veto
        # the cross-worker merge, but not `repeated`, so it is no evidence for.
        ("once", "function"): [1, 0.5, True, "fp"],
    }
    p.pytest_sessionfinish(session=SimpleNamespace(config=SimpleNamespace()), exitstatus=0)
    fixtures = next(pl for k, pl in p._conn.sent if k == "doctor_fixtures")["fixtures"]
    got = {
        f["name"]: (f["constant"], f["repeated"], f["redundant"], f["fingerprint"])
        for f in fixtures
    }
    # cfg: 4 calls, mean 0.5s => 3 redundant setups = 1.5s. The fingerprint
    # ships only for constant fixtures, for the CLI's cross-worker check.
    assert got == {
        "cfg": (True, True, 1.5, "fp"),
        "varies": (False, False, 0.0, None),
        "once": (True, False, 0.0, "fp"),
    }


def test_sessionfinish_quiet_when_nothing_to_report():
    p = _plugin()
    p.pytest_sessionfinish(session=SimpleNamespace(config=SimpleNamespace()), exitstatus=0)
    assert p._conn.sent == []  # no warnings, doctor off -> nothing sent


# ── _fixture_fingerprint: scope-promotion value comparison ─────────────────


@pytest.mark.parametrize(
    "value",
    ["prod", b"k", 7, 1.5, True, 2j, ("a", 1, None), frozenset({1, 2}), ((1,), frozenset())],
)
def test_fixture_fingerprint_accepts_immutable_builtins(value):
    fp = stream._fixture_fingerprint(value)
    assert fp is not None
    assert fp == stream._fixture_fingerprint(value)


def test_fixture_fingerprint_distinguishes_values_and_types():
    fp = stream._fixture_fingerprint
    assert fp(("a", 1)) != fp(("a", 2))
    # Equal-comparing values of different types are different values.
    assert len({fp(1), fp(1.0), fp(True)}) == 3


def test_fixture_fingerprint_frozenset_ignores_iteration_order():
    a = frozenset(["x", "y", "z"])
    b = frozenset(["z", "y", "x"])
    assert stream._fixture_fingerprint(a) == stream._fixture_fingerprint(b)


class _Settings:
    def __repr__(self) -> str:
        return "Settings(mode='prod')"


class _Str(str):
    pass


@pytest.mark.parametrize(
    "value",
    [
        None,  # side-effect-only fixtures return None every call
        [],  # fresh mutable containers look identical but must not be shared
        {},
        {"k": 1},
        set(),
        bytearray(b"x"),
        _Settings(),  # user objects: repr proves nothing and runs user code
        _Str("prod"),  # subclasses may override behaviour
        ("a", []),  # immutable shell around a mutable
        object(),
    ],
)
def test_fixture_fingerprint_rejects_everything_else(value):
    assert stream._fixture_fingerprint(value) is None


def test_fixture_fingerprint_never_calls_user_repr():
    class Loud:
        def __repr__(self) -> str:
            pytest.fail("repr must not run")

    from unittest import mock

    assert stream._fixture_fingerprint(Loud()) is None
    assert stream._fixture_fingerprint(("a", Loud())) is None
    assert stream._fixture_fingerprint(mock.MagicMock()) is None


def test_fixture_fingerprint_caps_walked_items(monkeypatch):
    monkeypatch.setattr(stream, "_FP_MAX_ITEMS", 3)
    assert stream._fixture_fingerprint((1, 2, 3)) is not None
    assert stream._fixture_fingerprint((1, 2, 3, 4)) is None
    assert stream._fixture_fingerprint(((1, 2), (3, 4))) is None  # nested items count


def test_fixture_fingerprint_survives_deep_nesting():
    v: tuple = ()
    for _ in range(5000):
        v = (v,)
    assert stream._fixture_fingerprint(v) is None


# ── pytest_fixture_setup: doctor timing wrapper ────────────────────────────


def test_fixture_setup_records_under_doctor(monkeypatch):
    monkeypatch.setenv("RSTEST_DOCTOR", "1")
    p = _plugin()
    fd = SimpleNamespace(argname="db", scope="function")
    gen = p.pytest_fixture_setup(fd, request=None)
    assert next(gen) is None  # wrapper yields to the real setup
    with pytest.raises(StopIteration):
        gen.send("result")
    count, total, _const, _fp = p._fixtures[("db", "function")]
    assert count == 1 and total >= 0.0


def test_fixture_setup_tracks_constant_value(monkeypatch):
    monkeypatch.setenv("RSTEST_DOCTOR", "1")
    p = _plugin()
    # Two setups returning the SAME value (via cached_result) keep all_constant.
    for _ in range(2):
        fd = SimpleNamespace(argname="cfg", scope="function", cached_result=(42, 0, None))
        gen = p.pytest_fixture_setup(fd, request=None)
        next(gen)
        with pytest.raises(StopIteration):
            gen.send(42)
    count, _total, const, _fp = p._fixtures[("cfg", "function")]
    assert count == 2 and const is True

    # A differing value on the second call clears the flag.
    for val in (1, 2):
        fd = SimpleNamespace(argname="rnd", scope="function", cached_result=(val, 0, None))
        gen = p.pytest_fixture_setup(fd, request=None)
        next(gen)
        with pytest.raises(StopIteration):
            gen.send(val)
    assert p._fixtures[("rnd", "function")][2] is False


def test_fixture_setup_yield_fixture_is_not_constant(monkeypatch):
    monkeypatch.setenv("RSTEST_DOCTOR", "1")
    p = _plugin()

    def gen_fixture():
        yield 42

    async def agen_fixture():
        yield 42

    # Same value every call, but a yield fixture's teardown may do per-test
    # work, so neither sync nor async generator fixtures are candidates.
    for name, func in (("sync", gen_fixture), ("async", agen_fixture)):
        for _ in range(2):
            fd = SimpleNamespace(
                argname=name, scope="function", func=func, cached_result=(42, 0, None)
            )
            gen = p.pytest_fixture_setup(fd, request=None)
            next(gen)
            with pytest.raises(StopIteration):
                gen.send(42)
        assert p._fixtures[(name, "function")][2] is False, name


def test_fixture_setup_parametrize_pseudo_fixture_is_not_constant(monkeypatch):
    from _pytest.python import get_direct_param_fixture_func

    monkeypatch.setenv("RSTEST_DOCTOR", "1")
    p = _plugin()
    # `@pytest.mark.parametrize("backend", ["sqlite"])` is served by a synthetic
    # function-scoped fixturedef; same value every call, nothing to promote.
    for _ in range(2):
        fd = SimpleNamespace(
            argname="backend",
            scope="function",
            func=get_direct_param_fixture_func,
            cached_result=("sqlite", 0, None),
        )
        gen = p.pytest_fixture_setup(fd, request=None)
        next(gen)
        with pytest.raises(StopIteration):
            gen.send("sqlite")
    assert p._fixtures[("backend", "function")][2] is False


def _run_setup(p, fd, request=None, register_finalizer=False):
    gen = p.pytest_fixture_setup(fd, request=request)
    next(gen)
    if register_finalizer:
        fd._finalizers.append(lambda: None)  # what request.addfinalizer does
    with pytest.raises(StopIteration):
        gen.send(fd.cached_result[0])


def test_fixture_setup_addfinalizer_fixture_is_not_constant(monkeypatch):
    monkeypatch.setenv("RSTEST_DOCTOR", "1")
    p = _plugin()
    # `request.addfinalizer(rollback); return "ready"`: same value, but the
    # per-test teardown would become once-per-worker if promoted.
    for _ in range(2):
        fd = SimpleNamespace(
            argname="txn", scope="function", cached_result=("ready", 0, None), _finalizers=[]
        )
        _run_setup(p, fd, register_finalizer=True)
    assert p._fixtures[("txn", "function")][2] is False


def test_fixture_setup_without_new_finalizer_stays_constant(monkeypatch):
    monkeypatch.setenv("RSTEST_DOCTOR", "1")
    p = _plugin()
    for _ in range(2):
        # pytest pre-registers its post-finalizer before the hook runs.
        fd = SimpleNamespace(
            argname="cfg", scope="function", cached_result=("x", 0, None), _finalizers=[object()]
        )
        _run_setup(p, fd)
    assert p._fixtures[("cfg", "function")][2] is True


@pytest.mark.parametrize(
    ("dep_scope", "expected"),
    [("session", True), ("module", False), ("function", False), (None, False)],
)
def test_fixture_setup_requires_session_scoped_dependencies(monkeypatch, dep_scope, expected):
    monkeypatch.setenv("RSTEST_DOCTOR", "1")
    p = _plugin()
    # `def env(monkeypatch): ...; return "prod"` would raise ScopeMismatch if
    # promoted; only session-scoped inputs (plus `request`) allow it.
    active = {} if dep_scope is None else {"dep": SimpleNamespace(scope=dep_scope)}
    request = SimpleNamespace(_fixture_defs=active)
    for _ in range(2):
        fd = SimpleNamespace(
            argname="env",
            scope="function",
            argnames=("request", "dep"),
            cached_result=("prod", 0, None),
        )
        _run_setup(p, fd, request=request)
    assert p._fixtures[("env", "function")][2] is expected


def test_fixture_setup_dynamic_narrower_dependency_taints_parent(monkeypatch):
    monkeypatch.setenv("RSTEST_DOCTOR", "1")
    p = _plugin()
    # `def cfg_name(request): request.getfixturevalue("tmp_path"); return "c.ini"`:
    # the nested setup runs while cfg_name's hook frame is open.
    for _ in range(2):
        outer = SimpleNamespace(
            argname="cfg_name", scope="function", cached_result=("c.ini", 0, None)
        )
        inner = SimpleNamespace(argname="tmp_path", scope="function", cached_result=("/t", 0, None))
        g_outer = p.pytest_fixture_setup(outer, request=None)
        next(g_outer)
        _run_setup(p, inner)
        with pytest.raises(StopIteration):
            g_outer.send("c.ini")
    assert p._fixtures[("cfg_name", "function")][2] is False
    assert p._setup_stack == []


def test_fixture_setup_dynamic_session_dependency_keeps_parent(monkeypatch):
    monkeypatch.setenv("RSTEST_DOCTOR", "1")
    p = _plugin()
    for _ in range(2):
        outer = SimpleNamespace(
            argname="url", scope="function", cached_result=("http://x", 0, None)
        )
        inner = SimpleNamespace(argname="server", scope="session", cached_result=("x", 0, None))
        g_outer = p.pytest_fixture_setup(outer, request=None)
        next(g_outer)
        _run_setup(p, inner)
        with pytest.raises(StopIteration):
            g_outer.send("http://x")
    assert p._fixtures[("url", "function")][2] is True


def test_fixture_setup_failed_setup_is_not_constant(monkeypatch):
    monkeypatch.setenv("RSTEST_DOCTOR", "1")
    p = _plugin()
    # pytest caches (None, key, exc_info) for a setup that raised or skipped;
    # an always-failing fixture must not look value-constant.
    for _ in range(2):
        exc = pytest.skip.Exception("no gpu")
        fd = SimpleNamespace(argname="gpu", scope="function", cached_result=(None, 0, (exc, None)))
        gen = p.pytest_fixture_setup(fd, request=None)
        next(gen)
        with pytest.raises(StopIteration):
            gen.send(None)
    assert p._fixtures[("gpu", "function")][2] is False


def test_fixture_setup_passthrough_without_doctor():
    p = _plugin()  # doctor off
    fd = SimpleNamespace(argname="db", scope="function")
    gen = p.pytest_fixture_setup(fd, request=None)
    next(gen)
    with pytest.raises(StopIteration):
        gen.send("result")
    assert p._fixtures == {}  # nothing timed


def test_fixture_setup_ignores_request_fixture(monkeypatch):
    monkeypatch.setenv("RSTEST_DOCTOR", "1")
    p = _plugin()
    fd = SimpleNamespace(argname="request", scope="function")
    gen = p.pytest_fixture_setup(fd, request=None)
    next(gen)
    with pytest.raises(StopIteration):
        gen.send("result")
    assert p._fixtures == {}  # the built-in request fixture is not measured


# ── pytest_runtest_logreport: payload branches ─────────────────────────────


def test_logreport_basic_report_payload():
    p = _plugin()
    p.pytest_runtest_logreport(mk_report("call", "passed"))
    kind, payload = p._conn.sent[0]
    assert kind == "report"
    assert payload["nodeid"] == "t.py::a"
    assert payload["when"] == "call"
    assert payload["outcome"] == "passed"
    assert payload["lineno"] == 1  # from location[1]


def test_logreport_attaches_cpu_on_call(monkeypatch):
    monkeypatch.setenv("RSTEST_DOCTOR", "1")
    p = _plugin()
    p._cpu["t.py::a"] = 0.123456
    p.pytest_runtest_logreport(mk_report("call", "passed"))
    payload = p._conn.sent[0][1]
    assert payload["cpu"] == 0.1235  # rounded to 4 places
    assert "t.py::a" not in p._cpu  # popped


def _drive_runtest_call(p, item):
    # Drive the wrapper=True generator: advance to the yield, then resume so its
    # `finally` (cpu recording) runs.
    gen = p.pytest_runtest_call(item)
    next(gen)
    with contextlib.suppress(StopIteration):
        gen.send(None)


def test_runtest_call_measures_cpu_under_stream_output(monkeypatch):
    # A live JSON consumer opens cpu measurement even without --doctor.
    monkeypatch.delenv("RSTEST_DOCTOR", raising=False)
    monkeypatch.setenv("RSTEST_STREAM_OUTPUT", "1")
    p = _plugin()
    assert p._measure_cpu
    monkeypatch.setattr(p, "_effective_timeout", lambda it: None)
    _drive_runtest_call(p, SimpleNamespace(nodeid="t.py::a"))
    assert "t.py::a" in p._cpu


def test_runtest_call_skips_cpu_by_default(monkeypatch):
    # Plain run (no doctor, no stream): no cpu measured, so --report-json stays
    # byte-comparable to the pytest baseline.
    monkeypatch.delenv("RSTEST_DOCTOR", raising=False)
    monkeypatch.delenv("RSTEST_STREAM_OUTPUT", raising=False)
    p = _plugin()
    assert not p._measure_cpu
    monkeypatch.setattr(p, "_effective_timeout", lambda it: None)
    _drive_runtest_call(p, SimpleNamespace(nodeid="t.py::a"))
    assert "t.py::a" not in p._cpu


def test_logreport_ships_sections_only_on_failure():
    p = _plugin()
    big = "x" * 30000
    p.pytest_runtest_logreport(
        mk_report("call", "failed", failed=True, sections=[("Captured stdout", big)])
    )
    payload = p._conn.sent[0][1]
    assert payload["sections"] == [["Captured stdout", big[-20000:]]]  # tail-truncated


def test_logreport_omits_passing_sections_without_stream_output(monkeypatch):
    # Default (no live JSON consumer): a passing test's captured output is NOT
    # shipped, keeping the wire lean.
    monkeypatch.delenv("RSTEST_STREAM_OUTPUT", raising=False)
    p = _plugin()
    p.pytest_runtest_logreport(
        mk_report("call", "passed", sections=[("Captured stdout call", "hi\n")])
    )
    assert "sections" not in p._conn.sent[0][1]


def test_logreport_ships_passing_sections_under_stream_output(monkeypatch):
    # With RSTEST_STREAM_OUTPUT=1 (--output json / --stream-json), a passing
    # test's captured output rides along so editors can show it.
    monkeypatch.setenv("RSTEST_STREAM_OUTPUT", "1")
    p = _plugin()
    p.pytest_runtest_logreport(
        mk_report("call", "passed", sections=[("Captured stdout call", "hi\n")])
    )
    assert p._conn.sent[0][1]["sections"] == [["Captured stdout call", "hi\n"]]


def test_logreport_omits_sections_when_empty():
    p = _plugin()
    p.pytest_runtest_logreport(mk_report("call", "passed", sections=[]))
    assert "sections" not in p._conn.sent[0][1]


def test_logreport_extracts_skip_reason():
    p = _plugin()
    p.pytest_runtest_logreport(
        mk_report("setup", "skipped", skipped=True, longrepr=("f.py", 3, "needs network"))
    )
    payload = p._conn.sent[0][1]
    assert payload["skip_reason"] == "needs network"


def test_logreport_omits_lineno_when_location_lineno_none():
    p = _plugin()
    p.pytest_runtest_logreport(mk_report("call", "passed", location=("t.py", None, "a")))
    assert "lineno" not in p._conn.sent[0][1]


# ── _init_xdist_node: per-plugin configure_node sweep ──────────────────────


def test_init_xdist_node_configures_each_plugin(monkeypatch):
    p = _plugin()
    seen: list[Any] = []
    monkeypatch.setattr(p, "_call_configure_node", lambda pl, lenient=False: seen.append(pl))
    a, b = object(), object()
    config = SimpleNamespace(
        workerinput={},
        pluginmanager=SimpleNamespace(get_plugins=lambda: [a, b]),
    )
    p._init_xdist_node(config, "gw0")
    assert seen == [a, b]


# ── _call_node_hooks: dist-internal plugins are skipped ────────────────────


def test_call_node_hooks_skips_dist_internal(monkeypatch):
    monkeypatch.setattr(stream, "_is_dist_internal", lambda pl: True)
    p = _plugin()
    p._xdist_node = SimpleNamespace(workerid="gw0")
    plugin = SimpleNamespace(pytest_testnodeready=lambda node: pytest.fail("called"))
    config = SimpleNamespace(pluginmanager=SimpleNamespace(get_plugins=lambda: [plugin]))
    p._call_node_hooks(config, "pytest_testnodeready")  # skipped, no call


# ── pytest_collectreport / pytest_internalerror: collect wire messages ─────


def test_collectreport_ships_error_on_failure():
    p = _plugin()
    p.pytest_collectreport(
        SimpleNamespace(failed=True, skipped=False, nodeid="t.py", longreprtext="boom")
    )
    assert p._conn.sent == [("collect_error", {"path": "t.py", "longrepr": "boom"})]


def test_collectreport_ships_skip_on_skipped():
    p = _plugin()
    p.pytest_collectreport(
        SimpleNamespace(failed=False, skipped=True, nodeid="t.py", longreprtext="")
    )
    assert p._conn.sent == [("collect_skip", {"path": "t.py"})]


def test_internalerror_ships_collect_error():
    p = _plugin()
    p.pytest_internalerror("kaboom")
    assert p._conn.sent == [("collect_error", {"path": "<internalerror>", "longrepr": "kaboom"})]
