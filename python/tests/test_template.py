"""The `--watch --fork-pool=app` template server: its control loop, the forked
child's fixups and the connect-back handshake."""

from __future__ import annotations

import io
import json
import os
import socket
import sys
import tempfile
import threading
from types import ModuleType, SimpleNamespace

import pytest

if sys.platform == "win32":
    pytest.skip("the template server is Unix-only", allow_module_level=True)

from rstest_worker._internal import template, warm


class _Exit(BaseException):
    """Stands in for os._exit, which would end the test process."""


def _raise_exit(code):
    raise _Exit(code)


def test_reap_collects_every_exited_child_then_stops(monkeypatch):
    results = iter([(11, 0), (12, 0), (0, 0)])
    monkeypatch.setattr(template.os, "waitpid", lambda pid, opts: next(results))
    template._reap(0, None)
    assert next(results, "drained") == "drained"


def test_reap_stops_when_no_children_are_left(monkeypatch):
    def none_left(pid, opts):
        raise ChildProcessError

    monkeypatch.setattr(template.os, "waitpid", none_left)
    template._reap(0, None)


def test_reports_imported_files_and_site_dirs(monkeypatch):
    with_file = ModuleType("a")
    with_file.__file__ = "/x/a.py"
    monkeypatch.setattr(sys, "modules", {"a": with_file, "b": ModuleType("b"), "c": with_file})
    monkeypatch.setattr(sys, "path", ["/proj", "/v/lib/site-packages", "/v/lib/dist-packages"])
    assert template.imported_files() == ["/x/a.py"]
    assert template.site_dirs() == ["/v/lib/site-packages"]


def test_connect_sends_the_handshake_line(monkeypatch):
    # AF_UNIX paths are capped near 104 bytes: keep the socket dir short.
    path = os.path.join(tempfile.mkdtemp(prefix="rt", dir="/tmp"), "s")
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(path)
    server.listen(1)
    got = []

    def accept():
        conn, _ = server.accept()
        with conn:
            got.append(conn.makefile().readline())

    t = threading.Thread(target=accept)
    t.start()
    fd = template._connect(path, "tok", 3, "err")
    os.close(fd)
    t.join(5)
    server.close()
    os.unlink(path)
    assert got == ["tok 3 err\n"]


@pytest.fixture
def child(monkeypatch):
    """_child with every fd operation stubbed: records dup2s and connections."""
    rec = SimpleNamespace(dup2=[], connects=[], served=[], reseeded=0)
    monkeypatch.setattr(template.signal, "signal", lambda *a: None)
    monkeypatch.setattr(template.os, "open", lambda *a: 99)
    monkeypatch.setattr(template.os, "close", lambda fd: None)
    # os is shared with pytest's capture, which dup2s too: keep only ours.
    monkeypatch.setattr(
        template.os, "dup2", lambda a, b: rec.dup2.append((a, b)) if a in (99, 60) else None
    )
    monkeypatch.setattr(template.os, "_exit", _raise_exit)

    def connect(path, token, idx, kind):
        rec.connects.append((path, token, idx, kind))
        return {"conn": 50, "err": 60}[kind]

    monkeypatch.setattr(template, "_connect", connect)
    monkeypatch.setattr(warm, "reseed_after_fork", lambda: setattr(rec, "reseeded", 1))
    # The child writes its identity and the request's env into os.environ.
    for key in ("RSTEST_WORKER_ID", "RSTEST_SEND_IDS", "T_SET", "T_UNSET"):
        monkeypatch.setenv(key, "before")
    return rec


@pytest.mark.parametrize("idx,send_ids,warm_used", [(0, "1", True), (2, "0", False)])
def test_child_becomes_a_pool_worker(child, idx, send_ids, warm_used):
    req = {"token": "tok", "env": {"T_SET": "v", "T_UNSET": None}}

    def run(conn):
        child.served.append((conn._cmd_fd, conn._evt_fd))

    with pytest.raises(_Exit) as exc:
        template._child("/s", req, idx, run, warm_used)
    assert exc.value.args == (0,)
    # stdin/stdout -> devnull, stderr -> its own connection.
    assert child.dup2 == [(99, 0), (99, 1), (60, 2)]
    assert child.connects == [("/s", "tok", idx, "conn"), ("/s", "tok", idx, "err")]
    assert child.served == [(50, 50)]
    assert child.reseeded == int(warm_used)
    assert os.environ["T_SET"] == "v" and "T_UNSET" not in os.environ
    assert os.environ["RSTEST_WORKER_ID"] == f"gw{idx}"
    assert os.environ["RSTEST_SEND_IDS"] == send_ids


@pytest.fixture
def server(monkeypatch):
    """serve() fed a scripted stdin, its replies captured, os._exit raising."""
    monkeypatch.setattr(template.signal, "signal", lambda *a: None)
    monkeypatch.setattr(template.os, "_exit", _raise_exit)
    monkeypatch.setattr(template, "imported_files", lambda: ["/x/a.py"])
    monkeypatch.setattr(template, "site_dirs", lambda: ["/v/site-packages"])

    def run(lines, fork, child=None):
        # Set at call time: capture swaps sys.stdout between fixture and test.
        out = io.StringIO()
        monkeypatch.setattr(sys, "stdout", out)
        monkeypatch.setattr(sys, "stdin", io.StringIO("".join(json.dumps(x) + "\n" for x in lines)))
        monkeypatch.setattr(template.os, "fork", fork)
        if child is not None:
            monkeypatch.setattr(template, "_child", child)
        with pytest.raises(_Exit) as exc:
            template.serve("/s", lambda conn: None, warm_used=False)
        return exc.value.args[0], [json.loads(x) for x in out.getvalue().splitlines()]

    return run


def test_serve_reports_imports_then_forks_on_request_until_exit(server, monkeypatch):
    monkeypatch.setenv("RSTEST_WARM_MANIFEST", "/m.json")
    pids = iter([101, 102, 103])
    reqs = [
        {"op": "fork", "idx": [0, 1], "token": "t", "env": {}},
        {"op": "fork", "idx": [0], "token": "t", "env": {}},
        {"op": "exit"},
        {"op": "fork", "idx": [0], "token": "t", "env": {}},  # never read
    ]
    code, replies = server(reqs, lambda: next(pids))
    assert code == 0
    assert replies == [
        {"files": ["/x/a.py"], "site": ["/v/site-packages"], "warm": True},
        {"pids": [101, 102]},
        {"pids": [103]},
    ]


def test_serve_exits_at_stdin_eof_and_reports_a_cold_template(server, monkeypatch):
    monkeypatch.delenv("RSTEST_WARM_MANIFEST", raising=False)
    code, replies = server([], lambda: pytest.fail("forked with no request"))
    assert code == 0 and replies == [
        {"files": ["/x/a.py"], "site": ["/v/site-packages"], "warm": False}
    ]


def test_a_child_that_fails_to_start_prints_why_and_exits_1(server, capsys):
    def broken(*a):
        raise RuntimeError("no socket")

    code, _ = server([{"op": "fork", "idx": [0], "token": "t", "env": {}}], lambda: 0, broken)
    assert code == 1
    assert "RuntimeError: no socket" in capsys.readouterr().err


def test_a_child_that_exits_cleanly_unwinds(server):
    def clean_exit(*a):
        sys.exit(0)

    with pytest.raises(SystemExit):
        server([{"op": "fork", "idx": [0], "token": "t", "env": {}}], lambda: 0, clean_exit)
