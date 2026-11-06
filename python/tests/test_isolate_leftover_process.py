"""`@pytest.mark.isolated`: a process the test forks and leaves running inherits
the result pipe's write end. The parent must not wait for that process to
exit before it reads the child's reports."""

from __future__ import annotations

import contextlib
import os
import signal
import subprocess
import sys
import textwrap
from pathlib import Path

import pytest

pytestmark = pytest.mark.skipif(not hasattr(os, "fork"), reason="isolation needs fork()")

PYTHON_DIR = Path(__file__).resolve().parents[1]


def test_leftover_grandchild_does_not_hang_the_parent(tmp_path):
    pidfile = tmp_path / "grandchild.pid"
    (tmp_path / "test_leaver.py").write_text(
        textwrap.dedent(
            f"""
            import os, time
            import pytest

            @pytest.mark.isolated
            def test_leaves_a_process_behind():
                pid = os.fork()
                if pid == 0:
                    time.sleep(60)
                    os._exit(0)
                open({str(pidfile)!r}, "w").write(str(pid))
                assert False, "fails before cleaning up its process"
            """
        )
    )
    runner = (
        "import sys\n"
        "from rstest_worker._internal import runner_pytest\n"
        "from rstest_worker._internal.isolate import IsolatedTests\n"
        "sys.exit(runner_pytest._pytest_main(\n"
        "    [sys.argv[1], '-q', '-p', 'no:cacheprovider', '-p', 'no:randomly'],\n"
        "    plugins=[IsolatedTests()],\n"
        "))\n"
    )
    # The vendored pytest core first, as the worker's __main__ sets it up.
    vendor = PYTHON_DIR / "rstest_worker" / "_vendor"
    env = {**os.environ, "PYTHONPATH": os.pathsep.join([str(vendor), str(PYTHON_DIR)])}
    # Output to a file, not a pipe: the leftover process also holds a dup of
    # the runner's stdout (pytest's capture saves one), and a pipe would make
    # this harness wait for it as well.
    out = tmp_path / "out.txt"
    try:
        with out.open("w") as fh:
            r = subprocess.run(
                [sys.executable, "-c", runner, str(tmp_path / "test_leaver.py")],
                stdout=fh,
                stderr=subprocess.STDOUT,
                timeout=20,
                env=env,
            )
    finally:
        if pidfile.exists():
            with contextlib.suppress(ProcessLookupError):
                os.kill(int(pidfile.read_text()), signal.SIGKILL)
    text = out.read_text()
    assert r.returncode == 1, text
    assert "fails before cleaning up its process" in text
