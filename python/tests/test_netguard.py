"""Unit tests for the outbound-network guard (`--block-network` / doctor).

`check` is driven directly with stand-in sockets, so the real stdlib socket
module is never patched here; the e2e gate covers the installed guard."""

from __future__ import annotations

import socket
from pathlib import Path
from types import SimpleNamespace

import pytest
from rstest_worker._internal.netguard import NetGuard, NetworkBlocked

HERE = Path(__file__).resolve().parent

INET = SimpleNamespace(family=socket.AF_INET)
INET6 = SimpleNamespace(family=socket.AF_INET6)
UNIX = SimpleNamespace(family=getattr(socket, "AF_UNIX", -1))


def guard(block=True, allow=(), marked=False):
    g = NetGuard(block, list(allow), HERE)
    item = SimpleNamespace(
        nodeid="t.py::test_x",
        get_closest_marker=lambda name: object() if marked and name == "network" else None,
    )
    g.start(item)
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
    g = guard(block=False)
    g.names["93.184.215.14"] = "api.example.com"
    g.check(INET, ("93.184.215.14", 443))
    g.check(INET, ("93.184.215.14", 443))
    g.check(INET, ("198.51.100.1", 80))
    assert g.pop("t.py::test_x") == ["api.example.com:443 (93.184.215.14)", "198.51.100.1:80"]
    assert g.pop("t.py::test_x") == []


def test_block_mode_raises_with_destination_and_call_site():
    g = guard()
    g.names["93.184.215.14"] = "api.example.com"
    with pytest.raises(NetworkBlocked) as excinfo:
        g.check(INET, ("93.184.215.14", 443))
    msg = str(excinfo.value)
    assert "connection to api.example.com:443 (93.184.215.14)" in msg
    # The call site is this file (the rootdir here), not netguard.py.
    assert "from test_netguard.py:" in msg
    assert "@pytest.mark.network" in msg
    # Not an OSError: an `except OSError` retry loop can't swallow it.
    assert not isinstance(excinfo.value, OSError)
    # Still recorded for the doctor.
    assert g.pop("t.py::test_x") == ["api.example.com:443 (93.184.215.14)"]


def test_marked_test_is_exempt_but_still_recorded():
    g = guard(marked=True)
    g.check(INET, ("93.184.215.14", 443))
    assert g.pop("t.py::test_x") == ["93.184.215.14:443"]


def test_from_env(monkeypatch):
    monkeypatch.delenv("RSTEST_NETWORK", raising=False)
    # Off: the config isn't even read (a bare stand-in has no rootpath).
    assert NetGuard.from_env(object()) is None
    monkeypatch.setenv("RSTEST_NETWORK", "record")
    monkeypatch.setenv("RSTEST_NETWORK_ALLOW", "redis\n172.16.0.0/12")
    g = NetGuard.from_env(SimpleNamespace(rootpath=HERE))
    assert g is not None and not g.block
    assert "redis" in g.hosts and len(g.nets) == 1
