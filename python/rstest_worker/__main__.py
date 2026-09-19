from __future__ import annotations

import contextlib
import os
import sys

# The vendored pytest core (see ../VENDOR.md) must shadow any pytest
# installed in the target venv. Research spike 4: sys.path-prepend gives a
# complete, class-identity-preserving shadow; partial namespaces don't fall
# back, so _vendor carries the WHOLE core. This must happen before anything
# imports `pytest`.
# Idempotent: multiprocessing-spawn/anyio to_process children re-execute
# this file after inheriting the worker's sys.path (which already has
# _vendor first); inserting again would make child sys.path != parent's.
_vendor = os.path.join(os.path.dirname(os.path.abspath(__file__)), "_vendor")
if _vendor not in sys.path:
    sys.path.insert(0, _vendor)

# Absolute (not relative) imports: multiprocessing-spawn and anyio
# to_process re-execute the parent's __main__ file via runpy without
# package context, where relative imports raise ImportError.
from rstest_worker._internal import protocol, runner_pytest


def main() -> None:
    # Fork-prewarm pool (Unix only). The orchestrator spawns ONE zygote that
    # imports the vendored pytest core exactly once (already done at module load
    # above), then forks N warm children instead of paying that import N times
    # across freshly spawned interpreters. See _fork_pool.
    if len(sys.argv) > 1 and sys.argv[1] == "--fork-pool":
        _fork_pool(sys.argv[2:])
        return
    cmd_arg, evt_arg = int(sys.argv[1]), int(sys.argv[2])
    if os.name == "nt":
        # The orchestrator passes HANDLE values on Windows; convert to
        # CRT file descriptors so os.read/os.write work uniformly.
        import msvcrt

        cmd_fd = msvcrt.open_osfhandle(cmd_arg, os.O_RDONLY)
        evt_fd = msvcrt.open_osfhandle(evt_arg, os.O_APPEND)
    else:
        cmd_fd, evt_fd = cmd_arg, evt_arg
    conn = protocol.Connection(cmd_fd, evt_fd)
    try:
        _serve(conn)
    except BrokenPipeError:
        # Orchestrator left first (e.g. it refused mismatched collections
        # and exited); nothing useful to say to a closed pipe.
        os._exit(0)


def _fork_pool(argv: list[str]) -> None:
    """Zygote entry: `--fork-pool <count> <report_fd> <cmd0> <evt0> ...`.

    The vendored pytest core is imported once (module load, above); each child
    inherits it copy-on-write, so the per-worker import cost the plain spawn path
    pays N times is paid once here. Unix only: relies on os.fork.

    Per-worker identity (`gwN`, RSTEST_SEND_IDS) is applied in each child by
    index rather than via distinct child environments, since all children share
    the one zygote environment the orchestrator set. Run-wide vars (RUN_UID,
    WORKER_COUNT, BASETEMP, DOCTOR, ...) are identical across workers and ride
    the inherited environment untouched.
    """
    count = int(argv[0])
    report_fd = int(argv[1])
    fds = [int(a) for a in argv[2:]]
    # cmd/evt fds are interleaved per worker: [cmd0, evt0, cmd1, evt1, ...].
    pairs = [(fds[2 * i], fds[2 * i + 1]) for i in range(count)]
    all_fds = set(fds) | {report_fd}

    pids = []
    for idx, (cmd_fd, evt_fd) in enumerate(pairs):
        pid = os.fork()
        if pid == 0:
            # Child: keep only this worker's own pipe pair; a sibling's evt-write
            # fd left open here would keep the orchestrator's read end from ever
            # seeing EOF when that sibling dies, breaking crash detection.
            for fd in all_fds - {cmd_fd, evt_fd}:
                with contextlib.suppress(OSError):
                    os.close(fd)
            os.environ["RSTEST_WORKER_ID"] = f"gw{idx}"
            # Exactly worker 0 ships the full id list (matches build_worker_command).
            os.environ["RSTEST_SEND_IDS"] = "1" if idx == 0 else "0"
            conn = protocol.Connection(cmd_fd, evt_fd)
            try:
                _serve(conn)
            except BrokenPipeError:
                os._exit(0)
            os._exit(0)
        pids.append(pid)

    # Parent: report the child pids to the orchestrator (which tracks them for
    # kill/watchdog), then exit. The children are reparented to init/launchd,
    # which reaps them on exit, so the orchestrator never needs to waitpid them.
    report = os.fdopen(report_fd, "w")
    report.write("\n".join(str(p) for p in pids) + "\n")
    report.flush()
    report.close()
    # Close inherited worker fds before exiting so no evt-write end lingers.
    for fd in set(fds):
        with contextlib.suppress(OSError):
            os.close(fd)
    os._exit(0)


def _serve(conn) -> None:
    for cmd in conn.commands():
        kind = cmd["kind"]
        if kind == "shutdown":
            break
        if kind == "run_tests":
            exitstatus = runner_pytest.run(cmd["payload"]["args"], conn)
            conn.send("done", {"exitstatus": exitstatus})
        elif kind == "run_items_session":
            exitstatus = runner_pytest.run_session(cmd["payload"]["args"], conn)
            conn.send("done", {"exitstatus": exitstatus})
        elif kind == "run_lazy_session":
            exitstatus = runner_pytest.run_lazy_session(cmd["payload"]["args"], conn)
            conn.send("done", {"exitstatus": exitstatus})


# Guarded like multiprocessing requires: child runtimes (multiprocessing
# spawn, anyio to_process) re-import this file as __mp_main__ and must
# not start a second worker loop.
if __name__ == "__main__":  # pragma: no cover
    main()
