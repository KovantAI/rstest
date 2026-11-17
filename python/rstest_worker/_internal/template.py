"""The `--watch --fork-pool=app` template server (Unix).

A process that has imported the app once and forks a worker pool on request,
for the whole watch session. Requests arrive as one JSON object per line on
stdin, replies go out the same way on stdout:

  {"op": "fork", "idx": [0, 1], "token": "...", "env": {"K": "v" | null}}
      -> {"pids": [p0, p1]}
  {"op": "exit"}

Its first line is `{"files": [path, ...], "site": [dir, ...], "warm": bool}`:
the module files it imported and the site-packages on its path, which the
orchestrator re-stats before each cycle to tell when the template is stale.
It exits at stdin EOF, so it never outlives the orchestrator.

A forked worker can't receive pipes from the orchestrator after the fact, so it
connects back to the orchestrator's Unix socket twice: once for the protocol
(commands and events), once for its stderr. Each connection starts with the
line `<token> <idx> conn|err`.
"""

from __future__ import annotations

import json
import os
import signal
import socket
import sys
from collections.abc import Callable
from typing import Any


def _reap(_signum: int, _frame: Any) -> None:
    """Collect every exited worker: this process is their parent for the whole
    session, so without this they would pile up as zombies."""
    while True:
        try:
            pid, _ = os.waitpid(-1, os.WNOHANG)
        except ChildProcessError:
            return
        if pid == 0:
            return


def imported_files() -> list[str]:
    """Every module file this process loaded."""
    paths = (getattr(mod, "__file__", None) for mod in list(sys.modules.values()))
    return sorted({p for p in paths if isinstance(p, str)})


def site_dirs() -> list[str]:
    """Each site-packages on `sys.path`: an install or uninstall moves it."""
    return [p for p in sys.path if p.endswith("site-packages")]


def _connect(path: str, token: str, idx: int, kind: str) -> int:
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(path)
    s.sendall(f"{token} {idx} {kind}\n".encode())
    return s.detach()


def _child(path: str, req: dict[str, Any], idx: int, run: Callable, warm_used: bool) -> None:
    """In a forked worker: become an ordinary pool worker. Never returns."""
    from rstest_worker._internal import protocol

    signal.signal(signal.SIGCHLD, signal.SIG_DFL)
    # stdin/stdout are the server's control pipe: not the worker's to read or
    # write (a test's print would corrupt the next reply).
    devnull = os.open(os.devnull, os.O_RDWR)
    os.dup2(devnull, 0)
    os.dup2(devnull, 1)
    os.close(devnull)
    for key, value in req["env"].items():
        if value is None:
            os.environ.pop(key, None)
        else:
            os.environ[key] = value
    os.environ["RSTEST_WORKER_ID"] = f"gw{idx}"
    # Exactly worker 0 ships the full id list (matches build_worker_command).
    os.environ["RSTEST_SEND_IDS"] = "1" if idx == 0 else "0"
    fd = _connect(path, req["token"], idx, "conn")
    err = _connect(path, req["token"], idx, "err")
    sys.stderr.flush()
    os.dup2(err, 2)
    os.close(err)
    if warm_used:
        from rstest_worker._internal import warm

        warm.reseed_after_fork()
    run(protocol.Connection(fd, fd))
    os._exit(0)  # pragma: no cover - run() exits


def serve(path: str, run: Callable, warm_used: bool) -> None:
    """Report what was imported, then fork workers on request until told to
    exit or stdin closes."""
    signal.signal(signal.SIGCHLD, _reap)
    out = sys.stdout
    # `warm`: still holding the manifest's imports (false after a guard
    # restarted this process cold).
    warm = bool(os.environ.get("RSTEST_WARM_MANIFEST"))
    ready = {"files": imported_files(), "site": site_dirs(), "warm": warm}
    out.write(json.dumps(ready) + "\n")
    out.flush()
    for line in sys.stdin:
        req = json.loads(line)
        if req.get("op") != "fork":
            break
        pids = []
        for idx in req["idx"]:
            pid = os.fork()
            if pid == 0:
                try:
                    _child(path, req, idx, run, warm_used)
                except SystemExit:
                    raise  # the worker's clean exit: unwind and run atexit
                except BaseException:
                    import traceback

                    traceback.print_exc()
                    os._exit(1)
            pids.append(pid)
        out.write(json.dumps({"pids": pids}) + "\n")
        out.flush()
    os._exit(0)
