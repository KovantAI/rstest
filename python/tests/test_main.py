"""Unit tests for the worker entrypoint's command loop and dispatch."""

import sys
import types

import pytest
from rstest_worker import __main__ as worker_main
from rstest_worker._internal import runner_pytest


class _FakeConn:
    """Records sends and replays a scripted command stream."""

    def __init__(self, commands):
        self._commands = commands
        self.sent = []

    def commands(self):
        return iter(self._commands)

    def send(self, kind, payload):
        self.sent.append((kind, payload))


def test_serve_dispatches_run_tests(monkeypatch):
    monkeypatch.setattr(runner_pytest, "run", lambda args, conn: 7)
    conn = _FakeConn([{"kind": "run_tests", "payload": {"args": ["-x"]}}])
    worker_main._serve(conn)
    assert conn.sent == [("done", {"exitstatus": 7})]


def test_serve_dispatches_each_session_kind(monkeypatch):
    monkeypatch.setattr(runner_pytest, "run_session", lambda args, conn: 1)
    monkeypatch.setattr(runner_pytest, "run_lazy_session", lambda args, conn: 2)
    conn = _FakeConn(
        [
            {"kind": "run_items_session", "payload": {"args": []}},
            {"kind": "run_lazy_session", "payload": {"args": []}},
        ]
    )
    worker_main._serve(conn)
    assert conn.sent == [
        ("done", {"exitstatus": 1}),
        ("done", {"exitstatus": 2}),
    ]


def test_serve_shutdown_breaks_loop():
    # commands after "shutdown" must not be processed
    conn = _FakeConn([{"kind": "shutdown"}, {"kind": "run_tests", "payload": {}}])
    worker_main._serve(conn)
    assert conn.sent == []


def test_serve_ignores_unknown_kind():
    conn = _FakeConn([{"kind": "mystery", "payload": {}}])
    worker_main._serve(conn)
    assert conn.sent == []


def test_main_wires_fds_into_connection(monkeypatch):
    monkeypatch.setattr(worker_main.sys, "argv", ["prog", "3", "4"])
    monkeypatch.setattr(worker_main.os, "name", "posix")
    built = {}

    def fake_connection(cmd_fd, evt_fd):
        built["fds"] = (cmd_fd, evt_fd)
        return "CONN"

    served = {}
    monkeypatch.setattr(worker_main.protocol, "Connection", fake_connection)
    monkeypatch.setattr(worker_main, "_serve", lambda conn: served.setdefault("conn", conn))

    worker_main.main()

    assert built["fds"] == (3, 4)  # raw fds passed through on posix
    assert served["conn"] == "CONN"


def test_main_converts_windows_handles_via_msvcrt(monkeypatch):
    # On Windows the orchestrator passes HANDLE values; main() converts them to
    # CRT file descriptors via msvcrt.open_osfhandle. Simulate nt + a fake
    # msvcrt so this branch is covered on any platform (CI runs on Linux).
    monkeypatch.setattr(worker_main.sys, "argv", ["prog", "100", "200"])
    monkeypatch.setattr(worker_main.os, "name", "nt")

    opened = []
    fake_msvcrt = types.ModuleType("msvcrt")

    def open_osfhandle(handle, flags):
        opened.append((handle, flags))
        return 1000 + handle  # a distinct fd per handle

    monkeypatch.setattr(fake_msvcrt, "open_osfhandle", open_osfhandle, raising=False)
    monkeypatch.setitem(sys.modules, "msvcrt", fake_msvcrt)

    built = {}
    monkeypatch.setattr(
        worker_main.protocol,
        "Connection",
        lambda cmd_fd, evt_fd: built.setdefault("fds", (cmd_fd, evt_fd)),
    )
    monkeypatch.setattr(worker_main, "_serve", lambda conn: None)

    worker_main.main()

    # Each HANDLE converted with the right access flags, in order (cmd, evt).
    assert opened == [
        (100, worker_main.os.O_RDONLY),
        (200, worker_main.os.O_APPEND),
    ]
    # The converted fds (not the raw handles) are what Connection receives.
    assert built["fds"] == (1100, 1200)


def test_main_swallows_broken_pipe(monkeypatch):
    monkeypatch.setattr(worker_main.sys, "argv", ["prog", "3", "4"])
    monkeypatch.setattr(worker_main.os, "name", "posix")
    monkeypatch.setattr(worker_main.protocol, "Connection", lambda c, e: object())

    def raise_broken(conn):
        raise BrokenPipeError

    monkeypatch.setattr(worker_main, "_serve", raise_broken)

    def fake_exit(code):
        raise SystemExit(code)

    monkeypatch.setattr(worker_main.os, "_exit", fake_exit)

    with pytest.raises(SystemExit) as exc:
        worker_main.main()
    assert exc.value.code == 0
