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
if __name__ == "__main__":
    main()
