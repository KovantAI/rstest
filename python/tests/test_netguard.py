"""Unit tests for the outbound-network recorder (`--doctor`).

`check` is driven directly with stand-in sockets, so the real stdlib socket
module is never patched here; the e2e gate covers the installed guard."""

from __future__ import annotations

import socket
from types import SimpleNamespace

import pytest
from rstest_worker._internal.netguard import MAX_PER_TEST, NetGuard
from rstest_worker._internal.stream import StreamPlugin

INET = SimpleNamespace(family=socket.AF_INET)
INET6 = SimpleNamespace(family=socket.AF_INET6)
UNIX = SimpleNamespace(family=getattr(socket, "AF_UNIX", -1))


def guard(allow=()):
    g = NetGuard(list(allow))
    g.start("t.py::test_x")
    return g


def test_loopback_localhost_and_allowlist_pass():
    g = guard(allow=["db.internal", "10.0.0.0/8", "203.0.113.7"])
    g.names["10.1.2.3"] = "postgres"
    for sock, addr in [
        (INET, ("127.0.0.1", 5432)),
        (INET6, ("::1", 80, 0, 0)),
        (INET, ("0.0.0.0", 8000)),
        (INET, ("localhost", 80)),
        (INET, ("db.internal", 5432)),
        (INET, ("10.1.2.3", 5432)),
        (INET, ("203.0.113.7", 443)),
    ]:
        g.check(sock, addr)
    assert g.seen == {}


def test_outside_a_test_and_unix_sockets_are_not_checked():
    g = guard()
    g.check(UNIX, "/tmp/sock")
    g.stop()
    g.check(INET, ("93.184.215.14", 443))
    assert g.seen == {}


def test_record_mode_lists_destinations_without_failing():
    g = guard()
    g.names["93.184.215.14"] = "api.example.com"
    g.check(INET, ("93.184.215.14", 443))
    g.check(INET, ("93.184.215.14", 443))
    g.check(INET, ("198.51.100.1", 80))
    assert g.pop("t.py::test_x") == ["api.example.com:443 (93.184.215.14)", "198.51.100.1:80"]
    assert g.pop("t.py::test_x") == []


def test_from_env(monkeypatch):
    monkeypatch.delenv("RSTEST_NETWORK", raising=False)
    assert NetGuard.from_env() is None
    # The old blocking mode is gone: "block" no longer installs anything.
    monkeypatch.setenv("RSTEST_NETWORK", "block")
    assert NetGuard.from_env() is None
    monkeypatch.setenv("RSTEST_NETWORK", "record")
    monkeypatch.setenv("RSTEST_NETWORK_ALLOW", "redis\n172.16.0.0/12")
    g = NetGuard.from_env()
    assert g is not None
    assert "redis" in g.hosts and len(g.nets) == 1


def test_from_env_ignores_unknown_mode_and_blank_allow(monkeypatch):
    monkeypatch.setenv("RSTEST_NETWORK", "on")
    assert NetGuard.from_env() is None
    monkeypatch.setenv("RSTEST_NETWORK", "record")
    monkeypatch.setenv("RSTEST_NETWORK_ALLOW", "")
    g = NetGuard.from_env()
    assert g is not None
    assert g.hosts == set() and g.nets == []


def test_allowlist_is_case_and_whitespace_insensitive():
    g = guard(allow=[" DB.Internal ", "", "  "])
    assert g.hosts == {"db.internal"}
    g.check(INET, ("DB.INTERNAL", 5432))
    g.names["10.9.9.9"] = "Db.Internal"
    g.check(INET, ("10.9.9.9", 5432))
    assert g.seen == {}


def test_scoped_ipv6_and_short_addresses():
    g = guard()
    g.check(INET6, ("::1%lo0", 80, 0, 0))  # scoped loopback: allowed
    g.check(INET, ("203.0.113.9",))  # malformed: ignored
    g.check(UNIX, ("not-inet", 80))  # tuple address, non-INET family: ignored
    g.check(INET6, ("fe80::1%en0", 80, 0, 0))
    g.check(INET, ("unresolved.example", 80))  # a bare name not allowlisted
    assert g.pop("t.py::test_x") == ["fe80::1%en0:80", "unresolved.example:80"]


def test_destinations_capped_per_test():
    g = guard()
    for port in range(MAX_PER_TEST + 5):
        g.check(INET, ("198.51.100.1", port))
    assert len(g.pop("t.py::test_x")) == MAX_PER_TEST


def test_install_guards_connect_and_names_resolved_hosts(monkeypatch):
    # Stand-ins under the patch, restored by monkeypatch: no real traffic.
    calls = []
    monkeypatch.setattr(
        socket,
        "getaddrinfo",
        lambda host, *a, **k: [(socket.AF_INET, socket.SOCK_STREAM, 6, "", ("93.184.215.14", 443))],
    )
    monkeypatch.setattr(socket.socket, "connect", lambda s, a: calls.append(("connect", a)))
    monkeypatch.setattr(
        socket.socket, "connect_ex", lambda s, a: calls.append(("connect_ex", a)) or 0
    )
    g = guard()
    g.install()
    socket.getaddrinfo(b"bytes.example", 443)  # non-str host: not remembered
    socket.getaddrinfo("api.example.com", 443)
    assert g.names == {"93.184.215.14": "api.example.com"}
    with socket.socket() as s:
        s.connect(("93.184.215.14", 443))
        assert s.connect_ex(("127.0.0.1", 2)) == 0
    # Recorded, never blocked: every call reaches the real connect.
    assert calls == [("connect", ("93.184.215.14", 443)), ("connect_ex", ("127.0.0.1", 2))]
    assert g.pop("t.py::test_x") == ["api.example.com:443 (93.184.215.14)"]


# ── StreamPlugin wiring: the guard spans one test, its record rides teardown ──


class FakeConn:
    def __init__(self):
        self.sent = []

    def send(self, kind, payload):
        self.sent.append((kind, payload))


def _plugin(net):
    p = StreamPlugin(FakeConn())
    p._net = net
    return p


def _report(when):
    return SimpleNamespace(
        nodeid="t.py::test_x",
        when=when,
        outcome="passed",
        duration=0.1,
        longreprtext="",
        location=("t.py", 1, "test_x"),
        failed=False,
        skipped=False,
        sections=[],
        longrepr=None,
    )


def _item():
    return SimpleNamespace(nodeid="t.py::test_x")


def test_stream_scopes_guard_to_the_test_and_ships_destinations_on_teardown():
    p = _plugin(NetGuard([]))
    gen = p.pytest_runtest_protocol(_item(), None)
    next(gen)
    assert p._net.current == "t.py::test_x"
    with pytest.raises(StopIteration):
        gen.send(None)
    assert p._net.current is None
    p._net.seen["t.py::test_x"] = ["198.51.100.1:80"]
    p.pytest_runtest_logreport(_report("call"))
    assert "network" not in p._conn.sent[-1][1]
    p.pytest_runtest_logreport(_report("teardown"))
    assert p._conn.sent[-1][1]["network"] == ["198.51.100.1:80"]
    p.pytest_runtest_logreport(_report("teardown"))  # consumed: nothing twice
    assert "network" not in p._conn.sent[-1][1]


def test_stream_stops_guard_when_the_test_protocol_raises():
    p = _plugin(NetGuard([]))
    gen = p.pytest_runtest_protocol(_item(), None)
    next(gen)
    with pytest.raises(KeyboardInterrupt):
        gen.throw(KeyboardInterrupt())
    assert p._net.current is None


def test_stream_guard_off_without_env(monkeypatch):
    monkeypatch.delenv("RSTEST_NETWORK", raising=False)
    p = StreamPlugin(FakeConn())
    assert p._net is None
    gen = p.pytest_runtest_protocol(_item(), None)
    next(gen)
    with pytest.raises(StopIteration):
        gen.send(None)
    p.pytest_runtest_logreport(_report("teardown"))
    assert "network" not in p._conn.sent[-1][1]
