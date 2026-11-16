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
    monkeypatch.setattr(worker_main, "_peak_rss", lambda: None)
    monkeypatch.setattr(runner_pytest, "run", lambda args, conn: 7)
    conn = _FakeConn([{"kind": "run_tests", "payload": {"args": ["-x"]}}])
    worker_main._serve(conn)
    assert conn.sent == [("done", {"exitstatus": 7})]


def test_serve_dispatches_each_session_kind(monkeypatch):
    monkeypatch.setattr(worker_main, "_peak_rss", lambda: 4096)
    monkeypatch.setattr(runner_pytest, "run_session", lambda args, conn: 1)
    monkeypatch.setattr(runner_pytest, "run_lazy_session", lambda args, conn: 2)
    conn = _FakeConn(
        [
            {"kind": "run_items_session", "payload": {"args": []}},
            {"kind": "run_lazy_session", "payload": {"args": []}},
        ]
    )
    worker_main._serve(conn)
    # Each session's end carries the worker's peak RSS (memory-aware -n auto).
    assert conn.sent == [
        ("done", {"exitstatus": 1, "peak_rss": 4096}),
        ("done", {"exitstatus": 2, "peak_rss": 4096}),
    ]


@pytest.mark.skipif(sys.platform == "win32", reason="no resource module")
def test_peak_rss_is_this_process_in_bytes():
    import resource

    peak = worker_main._peak_rss()
    raw = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    # A real interpreter's peak is tens of MiB: KiB (Linux) scaled to bytes.
    assert peak is not None and peak >= raw and peak > 10 * 1024 * 1024


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
        # The fake worker fds (10..60) aren't real; only close the real one.
        if fd == report_w:
            real_close(fd)

    monkeypatch.setattr(worker_main.os, "close", record_close)
    monkeypatch.setattr(
        worker_main.os, "_exit", lambda code: (_ for _ in ()).throw(SystemExit(code))
    )

    # count=2, report_fd, release_fd, then cmd0 evt0 err0 cmd1 evt1 err1
    argv = ["2", str(report_w), str(release_r), "10", "20", "30", "40", "50", "60"]
    with pytest.raises(SystemExit) as exc:
        worker_main._fork_pool(argv)
    assert exc.value.code == 0

    pids = os.read(report_r, 64).decode()
    real_close(report_r)
    real_close(release_r)
    assert pids.split() == ["111", "222"]
    # All six inherited worker fds were closed in the parent before exit.
    assert {10, 20, 30, 40, 50, 60} <= set(closed)


def test_fork_pool_child_exits_normally_after_serve(monkeypatch):
    # Drive the CHILD half: os.fork returns 0. After a clean _serve the child
    # must exit via sys.exit (running atexit/logging/stdio teardown like a
    # spawned worker), never os._exit.
    monkeypatch.setattr(worker_main.os, "fork", lambda: 0)
    monkeypatch.setattr(worker_main.os, "close", lambda fd: None)
    dup2 = []
    monkeypatch.setattr(worker_main.os, "dup2", lambda fd, to: dup2.append((fd, to)))
    monkeypatch.setattr(worker_main.protocol, "Connection", lambda c, e: (c, e))
    served = []
    monkeypatch.setattr(worker_main, "_serve", served.append)
    monkeypatch.setattr(worker_main.os, "_exit", lambda code: pytest.fail("child used os._exit"))
    # The child writes its identity into os.environ; setenv restores it after.
    monkeypatch.setenv("RSTEST_WORKER_ID", "")
    monkeypatch.setenv("RSTEST_SEND_IDS", "")

    with pytest.raises(SystemExit) as exc:
        worker_main._fork_pool(["1", "9", "8", "10", "20", "30"])
    assert exc.value.code == 0
    assert served == [(10, 20)]
    # Its own stderr pipe replaces the zygote's (deduped by the orchestrator).
    assert dup2 == [(30, 2)]
    assert os.environ["RSTEST_WORKER_ID"] == "gw0"
    assert os.environ["RSTEST_SEND_IDS"] == "1"


def test_fork_pool_child_swallows_broken_pipe(monkeypatch):
    # A forked child whose orchestrator already left exits via os._exit(0),
    # same as the spawned worker's BrokenPipeError path, never a traceback.
    monkeypatch.setattr(worker_main.os, "fork", lambda: 0)
    monkeypatch.setattr(worker_main.os, "close", lambda fd: None)
    monkeypatch.setattr(worker_main.os, "dup2", lambda fd, to: None)
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
        worker_main._fork_pool(["1", "9", "8", "10", "20", "30"])
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


def test_fork_pool_parent_waits_past_release_bytes_until_eof(monkeypatch):
    # Bytes on the release pipe are not the release: the parent reads through
    # them and exits only at EOF (the orchestrator closing its write end).
    report_r, report_w = os.pipe()
    release_r, release_w = os.pipe()
    os.write(release_w, b"xyz")
    os.close(release_w)
    monkeypatch.setattr(worker_main.os, "fork", lambda: 111)
    reads = []
    real_read = os.read

    def record_read(fd, n):
        data = real_read(fd, n)
        reads.append(data)
        return data

    monkeypatch.setattr(worker_main.os, "read", record_read)
    monkeypatch.setattr(
        worker_main.os, "_exit", lambda code: (_ for _ in ()).throw(SystemExit(code))
    )
    with pytest.raises(SystemExit) as exc:
        worker_main._fork_pool(["1", str(report_w), str(release_r), "-1", "-1", "-1"])
    assert exc.value.code == 0
    assert reads == [b"x", b"y", b"z", b""]
    assert real_read(report_r, 64).decode().split() == ["111"]
    os.close(report_r)
    os.close(release_r)


def test_peak_rss_is_none_without_resource_module(monkeypatch):
    # Windows has no `resource`: the done message just omits peak RSS.
    monkeypatch.setitem(sys.modules, "resource", None)
    assert worker_main._peak_rss() is None


# ── --fork-pool=app / --template-server ─────────────────────────────────────


def test_main_routes_template_server(monkeypatch):
    monkeypatch.setattr(worker_main.sys, "argv", ["prog", "--template-server", "/s"])
    seen = {}
    monkeypatch.setattr(worker_main, "_template_server", lambda argv: seen.setdefault("argv", argv))
    worker_main.main()
    assert seen["argv"] == ["/s"]


class _Execv(BaseException):
    pass


def _raise_execv(path, args):
    raise _Execv(args)


@pytest.mark.parametrize("entry", ["--fork-pool", "--template-server"])
def test_a_tripped_guard_restarts_cold_with_the_same_argv(monkeypatch, entry):
    from rstest_worker._internal import warm

    monkeypatch.setenv("RSTEST_WARM_MANIFEST", "/m.json")
    monkeypatch.setattr(warm, "prewarm", lambda manifest, result: False)
    monkeypatch.setattr(worker_main.os, "execv", _raise_execv)
    fn = worker_main._fork_pool if entry == "--fork-pool" else worker_main._template_server
    argv = ["1", "9", "8", "10", "20", "30"] if entry == "--fork-pool" else ["/s"]
    with pytest.raises(_Execv) as exc:
        fn(argv)
    assert exc.value.args[0] == [sys.executable, "-m", "rstest_worker", entry, *argv]
    # The restarted process must not try the manifest again.
    assert "RSTEST_WARM_MANIFEST" not in os.environ


def test_a_warm_fork_pool_child_reseeds(monkeypatch):
    from rstest_worker._internal import warm

    monkeypatch.setenv("RSTEST_WARM_MANIFEST", "/m.json")
    monkeypatch.setattr(warm, "prewarm", lambda manifest, result: True)
    reseeded = []
    monkeypatch.setattr(warm, "reseed_after_fork", lambda: reseeded.append(1))
    monkeypatch.setattr(worker_main.os, "fork", lambda: 0)
    monkeypatch.setattr(worker_main.os, "close", lambda fd: None)
    monkeypatch.setattr(worker_main.os, "dup2", lambda fd, to: None)
    monkeypatch.setattr(worker_main.protocol, "Connection", lambda c, e: object())
    monkeypatch.setattr(worker_main, "_serve", lambda conn: None)
    monkeypatch.setenv("RSTEST_WORKER_ID", "")
    monkeypatch.setenv("RSTEST_SEND_IDS", "")
    with pytest.raises(SystemExit):
        worker_main._fork_pool(["1", "9", "8", "10", "20", "30"])
    assert reseeded == [1]


@pytest.mark.parametrize("manifest", [None, "/m.json"])
def test_template_server_serves_after_the_imports(monkeypatch, manifest):
    from rstest_worker._internal import template, warm

    if manifest:
        monkeypatch.setenv("RSTEST_WARM_MANIFEST", manifest)
    else:
        monkeypatch.delenv("RSTEST_WARM_MANIFEST", raising=False)
    monkeypatch.setattr(warm, "prewarm", lambda manifest, result: True)
    seen = {}
    monkeypatch.setattr(
        template, "serve", lambda path, run, warm_used: seen.update(path=path, warm=warm_used)
    )
    worker_main._template_server(["/s"])
    assert seen == {"path": "/s", "warm": bool(manifest)}


def test_template_worker_exits_like_a_spawned_worker(monkeypatch):
    monkeypatch.setattr(worker_main, "_serve", lambda conn: None)
    with pytest.raises(SystemExit) as exc:
        worker_main._template_worker(object())
    assert exc.value.code == 0

    def raise_broken(conn):
        raise BrokenPipeError

    monkeypatch.setattr(worker_main, "_serve", raise_broken)
    monkeypatch.setattr(
        worker_main.os, "_exit", lambda code: (_ for _ in ()).throw(SystemExit(("os._exit", code)))
    )
    with pytest.raises(SystemExit) as exc:
        worker_main._template_worker(object())
    assert exc.value.code == ("os._exit", 0)


def test_done_carries_the_warm_record(monkeypatch):
    from rstest_worker._internal import warm

    monkeypatch.setenv("RSTEST_WARM_RECORD", "1")
    monkeypatch.setattr(worker_main, "_peak_rss", lambda: None)
    monkeypatch.setattr(runner_pytest, "run", lambda args, conn: 0)
    monkeypatch.setattr(warm, "env_at_start", None)
    monkeypatch.setattr(warm, "recorded", ["app"])
    monkeypatch.setattr(warm, "rewrite_marked", ["plug"])
    monkeypatch.setattr(warm, "env_changed", ["K"])
    monkeypatch.setattr(warm, "coverage_active", lambda: True)
    conn = _FakeConn([{"kind": "run_tests", "payload": {"args": []}}])
    worker_main._serve(conn)
    # _serve snapshots the environment so record() can diff it later.
    assert warm.env_at_start == dict(os.environ)
    assert conn.sent == [
        (
            "done",
            {
                "exitstatus": 0,
                "imported": ["app"],
                "rewrite": ["plug"],
                "env": ["K"],
                "sys_path": list(sys.path),
                "coverage": True,
            },
        )
    ]


def test_done_without_a_recorded_manifest_omits_it(monkeypatch):
    from rstest_worker._internal import warm

    monkeypatch.setenv("RSTEST_WARM_RECORD", "1")
    monkeypatch.setattr(worker_main, "_peak_rss", lambda: None)
    monkeypatch.setattr(warm, "env_at_start", None)
    monkeypatch.setattr(warm, "recorded", None)
    conn = _FakeConn([])
    worker_main._done(conn, 3)
    assert conn.sent == [("done", {"exitstatus": 3})]
