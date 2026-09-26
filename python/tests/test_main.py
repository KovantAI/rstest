"""Unit tests for the worker entrypoint's command loop and dispatch."""

import os
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


def test_main_routes_fork_pool(monkeypatch):
    # `--fork-pool ...` dispatches to _fork_pool with the trailing argv, not the
    # single-worker Connection path.
    monkeypatch.setattr(worker_main.sys, "argv", ["prog", "--fork-pool", "2", "9", "10"])
    seen = {}
    monkeypatch.setattr(worker_main, "_fork_pool", lambda argv: seen.setdefault("argv", argv))
    # Guard: the single-worker path must not run.
    monkeypatch.setattr(
        worker_main.protocol,
        "Connection",
        lambda *a: pytest.fail("single-worker path taken for --fork-pool"),
    )
    worker_main.main()
    assert seen["argv"] == ["2", "9", "10"]


def test_fork_pool_parent_reports_pids_and_exits(monkeypatch):
    # Drive only the PARENT half: os.fork returns fake child pids, so the child
    # branch is never entered. The parent must write the pids to the report fd,
    # close the inherited worker fds, wait for the release pipe's EOF, and
    # os._exit(0).
    report_r, report_w = os.pipe()
    release_r, release_w = os.pipe()
    # The orchestrator has already released the zygote: read sees EOF at once.
    os.close(release_w)

    forks = iter([111, 222])
    monkeypatch.setattr(worker_main.os, "fork", lambda: next(forks))

    closed = []
    real_close = os.close

    def record_close(fd):
        closed.append(fd)
        # The fake worker fds (10,20,30,40) aren't real; only close the real one.
        if fd == report_w:
            real_close(fd)

    monkeypatch.setattr(worker_main.os, "close", record_close)
    monkeypatch.setattr(
        worker_main.os, "_exit", lambda code: (_ for _ in ()).throw(SystemExit(code))
    )

    # count=2, report_fd, release_fd, then cmd0 evt0 cmd1 evt1
    argv = ["2", str(report_w), str(release_r), "10", "20", "30", "40"]
    with pytest.raises(SystemExit) as exc:
        worker_main._fork_pool(argv)
    assert exc.value.code == 0

    pids = os.read(report_r, 64).decode()
    real_close(report_r)
    real_close(release_r)
    assert pids.split() == ["111", "222"]
    # All four inherited worker fds were closed in the parent before exit.
    assert {10, 20, 30, 40} <= set(closed)


def test_fork_pool_child_exits_normally_after_serve(monkeypatch):
    # Drive the CHILD half: os.fork returns 0. After a clean _serve the child
    # must exit via sys.exit (running atexit/logging/stdio teardown like a
    # spawned worker), never os._exit.
    monkeypatch.setattr(worker_main.os, "fork", lambda: 0)
    monkeypatch.setattr(worker_main.os, "close", lambda fd: None)
    monkeypatch.setattr(worker_main.protocol, "Connection", lambda c, e: (c, e))
    served = []
    monkeypatch.setattr(worker_main, "_serve", served.append)
    monkeypatch.setattr(worker_main.os, "_exit", lambda code: pytest.fail("child used os._exit"))
    # The child writes its identity into os.environ; setenv restores it after.
    monkeypatch.setenv("RSTEST_WORKER_ID", "")
    monkeypatch.setenv("RSTEST_SEND_IDS", "")

    with pytest.raises(SystemExit) as exc:
        worker_main._fork_pool(["1", "9", "8", "10", "20"])
    assert exc.value.code == 0
    assert served == [(10, 20)]
    assert os.environ["RSTEST_WORKER_ID"] == "gw0"
    assert os.environ["RSTEST_SEND_IDS"] == "1"


def test_fork_pool_child_swallows_broken_pipe(monkeypatch):
    # A forked child whose orchestrator already left exits via os._exit(0),
    # same as the spawned worker's BrokenPipeError path, never a traceback.
    monkeypatch.setattr(worker_main.os, "fork", lambda: 0)
    monkeypatch.setattr(worker_main.os, "close", lambda fd: None)
    monkeypatch.setattr(worker_main.protocol, "Connection", lambda c, e: object())

    def raise_broken(conn):
        raise BrokenPipeError

    monkeypatch.setattr(worker_main, "_serve", raise_broken)

    def fake_exit(code):
        raise SystemExit(("os._exit", code))

    monkeypatch.setattr(worker_main.os, "_exit", fake_exit)
    monkeypatch.setenv("RSTEST_WORKER_ID", "")
    monkeypatch.setenv("RSTEST_SEND_IDS", "")

    with pytest.raises(SystemExit) as exc:
        worker_main._fork_pool(["1", "9", "8", "10", "20"])
    assert exc.value.code == ("os._exit", 0)


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
