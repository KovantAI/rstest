"""Unit tests for the outbound-network recorder (`--doctor`).

`check` is driven directly with stand-in sockets, so the real stdlib socket
module is never patched here; the e2e gate covers the installed guard."""

from __future__ import annotations

import socket
from types import SimpleNamespace

from rstest_worker._internal.netguard import NetGuard

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
