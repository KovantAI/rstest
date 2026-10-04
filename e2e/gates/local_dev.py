"""e2e gate sections: daily local developer (DV-* scenarios).

An engineer in the edit-run-fix loop: runs a subset, reads the traceback,
reruns the failures with --lf, drops a breakpoint() or uses --pdb, Ctrl-Cs a
slow run, leaves --watch running and uses --changed before pushing.
Interactive parts (pty, signals, watch) run under hard timeouts and always
kill the process group, so a hang fails a check instead of hanging the gate.
Checks tagged `known_bug=True` pin current failures: they xfail today and turn
the gate red once the bug is fixed, so the marker gets dropped and the check
becomes a regression guard.
"""

import contextlib
import json
import os
import queue
import re
import select
import shutil
import signal
import subprocess
import threading
import time

from _harness import REPO, WINDOWS, check, git, git_init_commit, venv_bin

_ANSI = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]")


def _out(r):
    return r.stdout + r.stderr


def _env(g, extra=None):
    """The environment Gate.run builds, for the Popen / pty drivers."""
    env = dict(os.environ, VIRTUAL_ENV=str(g.venv), RSTEST_WORKER_PATH=str(REPO / "python"))
    for k in (
        "PYTEST_ADDOPTS",
        "GITHUB_STEP_SUMMARY",
        "BUILDKITE",
        "GITHUB_BASE_REF",
        "CI_MERGE_REQUEST_DIFF_BASE_SHA",
        "CI_MERGE_REQUEST_TARGET_BRANCH_NAME",
        "BUILDKITE_PULL_REQUEST_BASE_BRANCH",
        "NO_COLOR",
        "FORCE_COLOR",
        "PY_COLORS",
    ):
        env.pop(k, None)
    env["TERM"] = "xterm-256color"
    if extra:
        env.update(extra)
    return env


def _kill_group(pid):
    with contextlib.suppress(ProcessLookupError, PermissionError):
        os.killpg(pid, signal.SIGKILL)


def _pty_run(argv, cwd, env, feed=b"", when=b"(Pdb)", cols=120, timeout=30):
    """Run argv on a fresh pty (POSIX only). Writes `feed` once `when` shows
    up in the output. Returns (exit code or None on timeout, decoded output).
    The child is a session leader (pty.fork calls setsid), so killing its
    process group also takes down any worker it spawned."""
    import fcntl
    import pty
    import struct
    import termios

    pid, fd = pty.fork()
    if pid == 0:  # child
        try:
            os.chdir(str(cwd))
            os.execve(str(argv[0]), [str(a) for a in argv], env)
        finally:
            os._exit(127)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 50, cols, 0, 0))
    buf, fed, status = b"", not feed, None
    deadline = time.time() + timeout
    try:
        while time.time() < deadline:
            ready, _, _ = select.select([fd], [], [], 0.2)
            if ready:
                try:
                    chunk = os.read(fd, 65536)
                except OSError:
                    chunk = b""
                if not chunk:
                    break
                buf += chunk
            if not fed and when in buf:
                os.write(fd, feed)
                fed = True
            done, st = os.waitpid(pid, os.WNOHANG)
            if done:
                status = st
                break
        while status is None and time.time() < deadline:
            done, st = os.waitpid(pid, os.WNOHANG)
            if done:
                status = st
                break
            time.sleep(0.05)
    finally:
        if status is None:
            _kill_group(pid)
            with contextlib.suppress(ChildProcessError):
                os.waitpid(pid, 0)
        os.close(fd)
    rc = os.waitstatus_to_exitcode(status) if status is not None else None
    return rc, buf.decode("utf-8", "replace")


def _report_tests(path):
    try:
        return json.loads(path.read_text(encoding="utf-8"))["tests"]
    except (OSError, ValueError, KeyError):
        return None


def _read_json(path):
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None


def _log_rows(d, prefix):
    """Rows `<time> <worker> <nodeid>` from per-process log files."""
    rows = []
    for p in d.glob(prefix + ".*"):
        for ln in p.read_text(encoding="utf-8").splitlines():
            t, w, nid = ln.split(" ", 2)
            rows.append((float(t), w, nid))
    return sorted(rows)


def _clear(d, prefix):
    for p in d.glob(prefix + ".*"):
        p.unlink()


_ORDER_CONFTEST = (
    "import os, time, pytest\n\n"
    "@pytest.fixture(autouse=True)\n"
    "def _dv_order_log(request):\n"
    "    root = str(request.config.rootpath)\n"
    "    with open(os.path.join(root, f'order.{os.getpid()}'), 'a') as f:\n"
    "        w = os.environ.get('PYTEST_XDIST_WORKER', '-')\n"
    "        f.write(f'{time.time():.6f} {w} {request.node.nodeid}\\n')\n"
    "    yield\n"
)


def gate_local_dev_selection(g, args, binary):
    print("== local dev: selection parity and -s (DV-01, DV-12) ==")

    # DV-01: every selection flavour picks the same nodeid set as
    # `pytest --co -q` with the same args, at -n 0 and in the pool.
    # Unescaped ids, as a project with non-ASCII parametrize ids configures.
    g.write(
        "dv_sel/pytest.ini",
        "[pytest]\nmarkers =\n    slow: slow\n"
        "disable_test_id_escaping_and_forfeit_all_rights_to_community_support = true\n",
    )
    g.write(
        "dv_sel/tests/test_sel.py",
        "import pytest\n\n"
        "class TestCls:\n"
        "    def test_a(self): pass\n\n"
        "    @pytest.mark.slow\n"
        "    def test_b(self): pass\n\n"
        "@pytest.mark.parametrize('v', ['a::b', 'x[y', 'with space', '\u00fcn\u00ef'], ids=str)\n"
        "def test_p(v): pass\n\n"
        "@pytest.mark.slow\n"
        "def test_m(): pass\n",
    )
    g.write("dv_sel/tests/test_other.py", "def test_other(): pass\n")
    g.write("dv_sel/tests/sub/test_sub.py", "def test_sub(): pass\n")
    g.write("dv_sel/tests/sub/test_glob_x.py", "def test_glob(): pass\n")
    g.write("dv_sel/tests/ign/test_ign.py", "def test_ign(): pass\n")
    cwd = g.tmp / "dv_sel"
    py = venv_bin(g.venv, "python")
    cases = [
        ("-k", ["-k", "TestCls or space"]),
        ("-m", ["-m", "slow"]),
        ("-m not", ["-m", "not slow"]),
        ("file::Class::test", ["tests/test_sel.py::TestCls::test_b"]),
        ("file::test[id with ::]", ["tests/test_sel.py::test_p[a::b]"]),
        ("file::test[id with []", ["tests/test_sel.py::test_p[x[y]"]),
        ("file::test[id with space]", ["tests/test_sel.py::test_p[with space]"]),
        ("file::test[unicode id]", ["tests/test_sel.py::test_p[\u00fcn\u00ef]"]),
        ("directory", ["tests/sub"]),
        ("--deselect", ["--deselect", "tests/test_sel.py::TestCls::test_a"]),
        ("--ignore", ["--ignore", "tests/ign"]),
        ("--ignore-glob", ["--ignore-glob", "*_glob_*"]),
    ]
    rj = cwd / "dv-sel.json"
    for label, sel in cases:
        o = subprocess.run(
            [str(py), "-m", "pytest", "--co", "-q", "-p", "no:cacheprovider", *sel],
            cwd=cwd,
            capture_output=True,
            text=True,
            encoding="utf-8",
            env=_env(g),
            timeout=60,
        )
        expected = {ln for ln in o.stdout.splitlines() if "::" in ln}
        check(
            f"DV-01 setup: pytest --co selects tests for {label}",
            o.returncode == 0 and len(expected) >= 1,
            f"rc={o.returncode} " + o.stdout[-200:],
        )
        for n in ("0", "2"):
            rj.unlink(missing_ok=True)
            r = g.run("-n", n, "-q", "--report-json", str(rj), *sel, cwd=cwd)
            got = _report_tests(rj)
            got = set(got) if got is not None else None
            check(
                f"DV-01 -n {n} {label}: selection equals pytest --co",
                r.returncode == 0 and got == expected,
                f"rc={r.returncode} missing={expected - (got or set())} "
                f"extra={(got or set()) - expected}",
            )

    # DV-12: -s in the pool. rstest runs the session in one process (pytest's
    # own output) and says so once; the prints reach stdout.
    for i in range(3):
        g.write(f"dv_cap/test_p{i}.py", f"def test_p{i}():\n    print('dv-live-print-{i}')\n")
    cwd = g.tmp / "dv_cap"
    r = g.run("-n", "2", cwd=cwd)
    check(
        "DV-12 setup: without -s the prints are captured",
        r.returncode == 0 and "dv-live-print" not in r.stdout,
        r.stdout[-200:],
    )
    r = g.run("-n", "2", "-s", cwd=cwd)
    out = _out(r)
    check(
        "DV-12 -n 2 -s: every print appears in stdout, exit 0",
        r.returncode == 0 and all(f"dv-live-print-{i}" in r.stdout for i in range(3)),
        f"rc={r.returncode} " + r.stdout[-300:],
    )
    check(
        "DV-12 -n 2 -s: the capture-mode notice is printed once",
        out.count("-s runs the session") == 1,
        out[:300],
    )


def _dv_lf_suite(g, root):
    for i in range(5):
        body = "".join(
            f"def test_{i}_{j}():\n    assert not os.path.exists('fail_{i}_{j}')\n\n"
            for j in range(2)
        )
        g.write(f"{root}/tests/test_m{i}.py", "import os\n\n" + body)
    g.write(f"{root}/tests/conftest.py", _ORDER_CONFTEST)
    g.write(f"{root}/fail_1_0", "")
    g.write(f"{root}/fail_4_1", "")


def gate_local_dev_rerun_history(g, args, binary):
    print("== local dev: --lf / --ff / --nf history and a clean tree (DV-02, DV-11) ==")

    # DV-02: the same sequence under pytest (oracle) and `rstest -n 2`; after
    # each step lastfailed must match what pytest wrote.
    py = venv_bin(g.venv, "python")
    _dv_lf_suite(g, "dv_lf_rs")
    _dv_lf_suite(g, "dv_lf_py")
    rs, pyd = g.tmp / "dv_lf_rs", g.tmp / "dv_lf_py"
    rj = rs / "dv-lf.json"
    remaining = "tests/test_m4.py::test_4_1"
    fixed = "tests/test_m1.py::test_1_0"

    def step(*a):
        subprocess.run(
            [str(py), "-m", "pytest", "-q", "-p", "no:randomly", *a],
            cwd=pyd,
            capture_output=True,
            env=_env(g),
            timeout=60,
        )
        _clear(rs, "order")
        rj.unlink(missing_ok=True)
        r = g.run("-n", "2", "-q", "--report-json", str(rj), *a, cwd=rs)
        lf_rs = _read_json(rs / ".pytest_cache/v/cache/lastfailed")
        lf_py = _read_json(pyd / ".pytest_cache/v/cache/lastfailed")
        return r, lf_rs, lf_py

    r, lf_rs, lf_py = step()
    check(
        "DV-02 setup: run 1 has 2 failures out of 10",
        r.returncode == 1 and "2 failed, 8 passed" in r.stdout,
        f"rc={r.returncode} " + r.stdout[-200:],
    )
    first_plain = _log_rows(rs, "order")
    check(
        "DV-02 setup: without --ff the remaining failure is not dispatched first",
        bool(first_plain) and first_plain[0][2] != remaining,
        str(first_plain[:3]),
    )
    check(
        "DV-02 run 1: lastfailed matches pytest",
        lf_rs == lf_py and lf_py is not None,
        f"rstest={lf_rs} pytest={lf_py}",
    )

    (rs / "fail_1_0").unlink()
    (pyd / "fail_1_0").unlink()
    r, lf_rs, lf_py = step("--lf")
    ran = set(_report_tests(rj) or {})
    check(
        "DV-02 --lf: runs exactly the 2 previous failures (1 failed, 1 passed)",
        ran == {fixed, remaining} and "1 failed, 1 passed" in r.stdout,
        f"ran={sorted(ran)} " + r.stdout[-150:],
    )
    check(
        "DV-02 --lf: lastfailed matches pytest",
        lf_rs == lf_py,
        f"rstest={lf_rs} pytest={lf_py}",
    )

    r, lf_rs, lf_py = step("--ff")
    rows = _log_rows(rs, "order")
    check(
        "DV-02 --ff: the remaining failure is dispatched first, all 10 run",
        bool(rows) and rows[0][2] == remaining and "1 failed, 9 passed" in r.stdout,
        f"{rows[:3]} " + r.stdout[-150:],
    )
    check(
        "DV-02 --ff: lastfailed matches pytest",
        lf_rs == lf_py,
        f"rstest={lf_rs} pytest={lf_py}",
    )

    new_test = "def test_new():\n    pass\n"
    g.write("dv_lf_rs/tests/test_new.py", new_test)
    g.write("dv_lf_py/tests/test_new.py", new_test)
    r, lf_rs, lf_py = step("--nf")
    # Dispatch order, not start time: both workers' first items start within
    # microseconds of each other. The replay journal records each worker's
    # exact assignment order; the head of the --nf order is the first item of
    # the first chunk handed out, so it leads its worker's list.
    journal = _read_json(rs / ".rstest_cache" / "replay" / "latest.json") or {}
    assignment = journal.get("assignment") or []
    check(
        "DV-02 --nf: the new test file is dispatched first",
        any(w and w[0] == "tests/test_new.py::test_new" for w in assignment),
        str([w[:2] for w in assignment]),
    )
    check(
        "DV-02 --nf: lastfailed matches pytest",
        lf_rs == lf_py,
        f"rstest={lf_rs} pytest={lf_py}",
    )

    # B4: a passing subset run must keep the other file's failure.
    r, lf_rs, lf_py = step("tests/test_m0.py")
    check(
        "DV-02 setup: subset run passes",
        r.returncode == 0 and remaining in (lf_py or {}),
        f"rc={r.returncode} pytest={lf_py}",
    )
    check(
        "DV-02 subset run: lastfailed keeps unrelated failures (matches pytest)",
        lf_rs == lf_py,
        f"rstest={lf_rs} pytest={lf_py}",
    )

    # DV-11: a committed repo stays clean (porcelain empty) after a run from
    # the root and from a subdirectory, as with plain pytest.
    for name in ("dv_tree_rs", "dv_tree_py"):
        g.write(f"{name}/.gitignore", "__pycache__/\n")
        g.write(f"{name}/pyproject.toml", "[tool.pytest.ini_options]\ntestpaths = ['tests']\n")
        g.write(
            f"{name}/tests/test_a.py", "def test_a():\n    pass\n\ndef test_b():\n    assert 0\n"
        )
        g.write(f"{name}/tests/test_c.py", "def test_c():\n    pass\n")
        git_init_commit(g.tmp / name)

    def porcelain(d):
        return subprocess.run(
            ["git", "status", "--porcelain", "-uall"],
            cwd=d,
            capture_output=True,
            text=True,
            check=True,
        ).stdout.strip()

    pt = g.tmp / "dv_tree_py"
    for cwd in (pt, pt / "tests"):
        subprocess.run(
            [str(py), "-m", "pytest", "-q"], cwd=cwd, capture_output=True, env=_env(g), timeout=60
        )
    check(
        "DV-11 setup: pytest (root + subdir runs) leaves the tree clean",
        porcelain(pt) == "" and (pt / ".pytest_cache").is_dir(),
        porcelain(pt)[:300],
    )
    rt = g.tmp / "dv_tree_rs"
    r = g.run("-n", "2", cwd=rt)
    check(
        "DV-11 setup: root run went through the pool",
        r.returncode == 1 and "1 failed, 2 passed" in r.stdout,
        r.stdout[-200:],
    )
    check(
        "DV-11 root run: git status --porcelain stays empty",
        porcelain(rt) == "",
        porcelain(rt)[:300],
    )
    (rt / ".pytest_cache" / "v" / "cache" / "lastfailed").unlink(missing_ok=True)
    g.run("-n", "2", cwd=rt / "tests")
    check(
        "DV-11 subdir run: lastfailed lands in the rootdir's .pytest_cache, as pytest",
        (rt / ".pytest_cache" / "v" / "cache" / "lastfailed").is_file()
        and not (rt / "tests" / ".pytest_cache").exists(),
        str(sorted(p.name for p in (rt / "tests").iterdir())),
    )

    # DV-13: --incremental from the root and from tests/unit/ share the
    # rootdir's .rstest_cache. Its records must be rootdir-relative, so the
    # subdirectory run neither poisons nor erases what the root run recorded.
    g.write(
        "dv_incr/pyproject.toml",
        "[tool.pytest.ini_options]\ntestpaths = ['tests']\npythonpath = ['.']\n",
    )
    g.write("dv_incr/app/__init__.py", "")
    g.write("dv_incr/app/core.py", "def one():\n    return 1\n")
    g.write("dv_incr/app/util.py", "def two():\n    return 2\n")
    g.write(
        "dv_incr/tests/test_top.py",
        "from app import core\n\ndef test_t1():\n    assert core.one() == 1\n\n"
        "def test_t2():\n    assert core.one() + 1 == 2\n",
    )
    g.write(
        "dv_incr/tests/unit/test_u.py",
        "from app import util\n\ndef test_u1():\n    assert util.two() == 2\n\n"
        "def test_u2():\n    assert util.two() * 2 == 4\n",
    )
    ir = g.tmp / "dv_incr"
    incr = ["-n", "2", "--cov=app", "--cov-context=test", "--cov-report=", "--incremental"]

    def incr_run(cwd):
        for stale in [*ir.glob(".coverage*"), *(ir / "tests" / "unit").glob(".coverage*")]:
            stale.unlink()
        return g.run(*incr, cwd=cwd)

    r1 = incr_run(ir)
    outcomes = _read_json(ir / ".rstest_cache" / "incremental_outcomes.json") or {}
    recorded = sorted(outcomes.get("green") or [])
    check(
        "DV-13 setup: root --incremental run is green and records all 4 tests",
        r1.returncode == 0 and "4 passed" in r1.stdout and len(recorded) == 4,
        f"rc={r1.returncode} green={recorded} " + r1.stdout[-200:] + r1.stderr[-300:],
    )
    # Touch the unit tests' dependency so the subdir run really executes them
    # and rewrites the coverage index from tests/unit/.
    g.write("dv_incr/app/util.py", "def two():\n    return 2  # touched\n")
    r2 = incr_run(ir / "tests" / "unit")
    check(
        "DV-13 setup: the tests/unit/ run re-runs its 2 tests, green",
        r2.returncode == 0 and "2 passed" in r2.stdout and "cached" not in r2.stdout,
        f"rc={r2.returncode} " + r2.stdout[-200:] + r2.stderr[-300:],
    )
    index = _read_json(ir / ".rstest_cache" / "coverage_index.json") or {}
    keys = sorted(index.get("files") or {})
    check(
        "DV-13 coverage index keys stay rootdir-relative after the subdir run",
        bool(keys) and all((ir / k).is_file() for k in keys),
        str(keys),
    )
    r3 = incr_run(ir)
    n = len(recorded)
    check(
        "DV-13 --incremental root, tests/unit/, root: last run skips what the first recorded",
        r3.returncode == 0
        and f"{n} of {n} test(s) unchanged" in r3.stderr
        and f"({n} cached)" in r3.stdout,
        f"rc={r3.returncode} " + r3.stderr[-300:] + r3.stdout[-200:],
    )


class _Watch:
    """`rstest --watch` driven over pipes; every wait has a deadline."""

    def __init__(self, g, binary, cwd, *args):
        self.proc = subprocess.Popen(
            [str(binary), "--watch", *args],
            cwd=str(cwd),
            env=_env(g),
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            encoding="utf-8",
            start_new_session=not WINDOWS,
        )
        self.lines = queue.Queue()
        threading.Thread(target=self._pump, daemon=True).start()

    def _pump(self):
        for line in self.proc.stdout:
            self.lines.put(line)

    def wait_for(self, needle, timeout=30):
        buf, deadline = [], time.time() + timeout
        while time.time() < deadline:
            try:
                line = self.lines.get(timeout=0.25)
            except queue.Empty:
                continue
            buf.append(line)
            if needle in line:
                return True, "".join(buf)
        return False, "".join(buf)

    def cycle(self, edit, timeout=30):
        """Apply one edit and collect the output up to the next idle prompt.
        The idle prompt of the previous cycle has already been consumed."""
        time.sleep(0.5)
        edit()
        return self.wait_for("waiting for changes", timeout)

    def stop(self):
        try:
            if self.proc.poll() is None:
                self.proc.stdin.write("q\n")
                self.proc.stdin.flush()
                self.proc.wait(timeout=10)
        except (OSError, subprocess.TimeoutExpired):
            pass
        finally:
            if self.proc.poll() is None:
                if WINDOWS:
                    self.proc.kill()
                else:
                    _kill_group(self.proc.pid)
                self.proc.wait(timeout=10)


def _conftest_importer_project(g, root):
    g.write(f"{root}/app/__init__.py", "")
    g.write(f"{root}/app/other.py", "def mul(a, b):\n    return a * b\n")
    g.write(
        f"{root}/tests/conftest.py",
        "import pytest\nfrom app.other import mul\n\n"
        "@pytest.fixture\ndef doubled():\n    return mul(2, 3)\n",
    )
    g.write(f"{root}/tests/test_calc.py", "def test_calc(doubled):\n    assert doubled == 6\n")
    g.write(
        f"{root}/tests/test_other.py",
        "from app.other import mul\n\ndef test_other():\n    assert mul(2, 3) == 6\n",
    )
    g.write(f"{root}/tests/test_plain.py", "def test_plain():\n    assert True\n")
    g.write(
        f"{root}/pyproject.toml",
        "[tool.pytest.ini_options]\ntestpaths = ['tests']\npythonpath = ['.']\n",
    )


# A different byte length from the original, so a same-second rewrite can
# never be served from a stale .pyc (Python keys pycs on mtime + size).
_BROKEN_MUL = "def mul(a, b):\n    return a * b + 1\n"


def gate_local_dev_changed_watch(g, args, binary):
    print("== local dev: --changed and --watch import-graph selection (DV-03, DV-04) ==")

    # DV-03 --changed: conftest.py imports app.other.mul for a fixture.
    _conftest_importer_project(g, "dv_chg")
    cwd = g.tmp / "dv_chg"
    git_init_commit(cwd)
    g.write("dv_chg/app/other.py", _BROKEN_MUL)
    r = g.run("-n", "2", cwd=cwd)
    check(
        "DV-03 setup: full run after breaking mul has 2 failures",
        "2 failed, 1 passed" in r.stdout,
        r.stdout[-200:],
    )
    r = g.run("--changed", "-n", "2", "-v", cwd=cwd)
    check(
        "DV-03 --changed: the direct importer runs and fails",
        "test_other.py::test_other FAILED" in r.stdout,
        r.stdout[-300:],
    )
    check(
        "DV-03 --changed: conftest importer's subtree runs (2 failed)",
        "test_calc.py::test_calc FAILED" in r.stdout and "2 failed" in r.stdout,
        r.stdout[-300:] + r.stderr[-200:],
    )
    r = g.run("--changed-strict", "-n", "2", cwd=cwd)
    check(
        "DV-03 --changed-strict: conftest importer's subtree runs (2 failed)",
        "2 failed" in r.stdout,
        r.stdout[-200:],
    )
    git(cwd, "checkout", "-q", "--", "app/other.py")

    # DV-04: the edit kinds gate_watch_mode does not cover. (a) and the
    # plain source-importer half of (b) live there.
    _conftest_importer_project(g, "dv_watch")
    g.write("dv_watch/tests/data.txt", "x\n")
    wd = g.tmp / "dv_watch"
    w = _Watch(g, binary, wd, "-n", "2", "-v")
    try:
        ok, out = w.wait_for("waiting for changes", 60)
        check("DV-04 setup: watch initial run is green", ok and "3 passed" in out, out[-300:])

        ok, out = w.cycle(lambda: g.write("dv_watch/app/other.py", _BROKEN_MUL))
        check(
            "DV-04 (b) source edit: direct importer reruns and fails",
            ok and "test_other.py::test_other FAILED" in out,
            out[-300:],
        )
        check(
            "DV-04 (b) source edit: conftest importer's subtree reruns (2 failed)",
            ok and "test_calc.py::test_calc FAILED" in out and "2 failed" in out,
            out[-300:],
        )
        ok, out = w.cycle(
            lambda: g.write("dv_watch/app/other.py", "def mul(a, b):\n    return a * b\n")
        )
        check("DV-04 setup: restoring mul goes green again", ok and "failed" not in out, out[-300:])

        ok, out = w.cycle(
            lambda: g.write("dv_watch/tests/test_new.py", "def test_new():\n    assert True\n")
        )
        check(
            "DV-04 (c) new test file: runs just that file",
            ok and "test_new.py::test_new PASSED" in out and "1 passed" in out,
            out[-300:],
        )

        ok, out = w.cycle(lambda: (wd / "tests" / "test_new.py").unlink())
        check(
            "DV-04 (d) deleted test file: no error, keeps watching",
            ok
            and "Traceback" not in out
            and "error" not in out.lower().replace("last exit: 0", "")
            and w.proc.poll() is None,
            out[-300:],
        )

        ok, out = w.cycle(lambda: g.write("dv_watch/tests/test_plain.py", "def test_plain(:\n"))
        check(
            "DV-04 (e) syntax error: shown, and watch keeps running",
            ok and "SyntaxError" in out and w.proc.poll() is None,
            out[-300:],
        )
        ok, out = w.cycle(
            lambda: g.write(
                "dv_watch/tests/test_plain.py", "def test_plain():\n    assert 1 == 1\n"
            )
        )
        check(
            "DV-04 (e) syntax error fixed: recovers green",
            ok and "test_plain.py::test_plain PASSED" in out and "1 passed" in out,
            out[-300:],
        )

        time.sleep(0.5)
        g.write("dv_watch/tests/data.txt", "changed data\n")
        rerun, out = w.wait_for("changed;", 4)
        check(
            "DV-04 (f) data file edit: ignored (no rerun)",
            not rerun and w.proc.poll() is None,
            out[-300:],
        )
    finally:
        w.stop()


def gate_local_dev_debugging(g, args, binary):
    print("== local dev: breakpoint() and --pdb on a pty (DV-05, DV-06) ==")
    if WINDOWS:
        print("  (skipped on Windows: needs a POSIX pty)")
        return
    g.write(
        "dv_bp/tests/test_bp.py",
        "def test_bp():\n    x = 1\n    breakpoint()\n    assert x == 1\n\n"
        "def test_after_bp():\n    pass\n",
    )
    g.write("dv_bp/tests/test_more.py", "def test_more():\n    pass\n")
    g.write(
        "dv_bp/tests/test_pdb.py",
        "def test_fail():\n    assert 1 == 2\n\ndef test_ok():\n    pass\n",
    )
    cwd = g.tmp / "dv_bp"
    env = _env(g)

    # DV-05 oracle: single-worker mode is pytest itself and drops into Pdb.
    rc, out = _pty_run([binary, "-n", "0", "tests/test_bp.py"], cwd, env, feed=b"c\n")
    check(
        "DV-05 setup: -n 0 drops into (Pdb) and finishes green after 'c'",
        rc == 0 and "(Pdb)" in out and "2 passed" in out,
        f"rc={rc} " + _ANSI.sub("", out)[-300:],
    )
    rj = cwd / "dv-bp.json"
    rc, out = _pty_run(
        [binary, "-n", "2", "--report-json", str(rj), "tests/test_bp.py"],
        cwd,
        env,
        feed=b"c\n",
    )
    plain = _ANSI.sub("", out)
    # The banner itself mentions -n 0; only the rest of the output counts.
    body = "\n".join(ln for ln in plain.splitlines() if not ln.startswith("rstest "))
    check("DV-05 -n 2 breakpoint(): run ends (no hang)", rc is not None, plain[-300:])
    check(
        "DV-05 -n 2 breakpoint(): (Pdb) prompt, or a hint naming -s / -n 0",
        "(Pdb)" in body or "-n 0" in body or " -s" in body,
        f"rc={rc} " + plain[-300:],
    )
    tests = _report_tests(rj) or {}
    check(
        "DV-05 -n 2 breakpoint(): every collected test is accounted for",
        "tests/test_bp.py::test_after_bp" in tests and len(tests) == 2,
        str(sorted(tests)),
    )

    # DV-06: --pdb in the pool gets a real prompt; 'q' ends the run with
    # pytest's exit code 2 (interrupted).
    rc, out = _pty_run([binary, "-n", "2", "--pdb", "tests/test_pdb.py"], cwd, env, feed=b"q\n")
    check(
        "DV-06 -n 2 --pdb: (Pdb) prompt appears, 'q' exits 2",
        rc == 2 and "(Pdb)" in out,
        f"rc={rc} " + _ANSI.sub("", out)[-300:],
    )


_TB_TEST = (
    "def helper(v):\n"
    "    total = v + 1\n"
    "    assert total == 0, 'helper says no'\n\n\n"
    "def test_multi():\n"
    "    data = [1, 2, 3]\n"
    "    print('dv-captured-marker')\n"
    "    helper(len(data))\n"
)


def _failure_body(text, header_re):
    """Lines of the single failure block, after its header, up to the
    captured-output section; leading blank lines dropped."""
    lines = text.splitlines()
    start = next((i for i, ln in enumerate(lines) if re.search(header_re, ln)), None)
    if start is None:
        return []
    body = []
    for ln in lines[start + 1 :]:
        if ln.startswith("---") or ln.startswith("==="):
            break
        body.append(ln)
    while body and not body[0].strip():
        body.pop(0)
    while body and not body[-1].strip():
        body.pop()
    return body


def gate_local_dev_output(g, args, binary):
    print("== local dev: traceback styles and terminal handling (DV-07, DV-10) ==")
    g.write("dv_tb/test_tb.py", _TB_TEST)
    cwd = g.tmp / "dv_tb"
    py = venv_bin(g.venv, "python")

    def pytest_tb(style):
        return subprocess.run(
            [str(py), "-m", "pytest", "-q", "-p", "no:cacheprovider", f"--tb={style}"],
            cwd=cwd,
            capture_output=True,
            text=True,
            encoding="utf-8",
            env=_env(g),
            timeout=60,
        ).stdout

    def rstest_tb(style):
        return g.run("-n", "2", "-q", "-p", "no:cacheprovider", f"--tb={style}", cwd=cwd).stdout

    py_hdr, rs_hdr = r"^_+ test_multi _+$", r"^--- FAILED .*test_multi ---$"
    for style in ("auto", "long"):
        exp = _failure_body(pytest_tb(style), py_hdr)
        got = _failure_body(rstest_tb(style), rs_hdr)
        check(
            f"DV-07 --tb={style}: failure body after the first line matches pytest",
            len(exp) > 5 and got[1:] == exp[1:],
            f"\nexp={exp}\ngot={got}",
        )
        check(
            f"DV-07 --tb={style}: first source line keeps its 4-space indent",
            bool(got) and got[0] == exp[0] == "    def test_multi():",
            f"got={got[:1]} exp={exp[:1]}",
        )
    exp = _failure_body(pytest_tb("short"), py_hdr)
    got = _failure_body(rstest_tb("short"), rs_hdr)
    check(
        "DV-07 --tb=short: failure body matches pytest",
        len(exp) > 3 and got == exp,
        f"\nexp={exp}\ngot={got}",
    )
    exp = _failure_body(pytest_tb("native"), py_hdr)
    got = _failure_body(rstest_tb("native"), rs_hdr)
    check(
        "DV-07 --tb=native: traceback ends like pytest's (test frames + exception)",
        len(exp) > 4 and got[-4:] == exp[-4:] and got[0] == "Traceback (most recent call last):",
        f"\nexp={exp[-4:]}\ngot={got[-4:]}",
    )
    out = rstest_tb("line")
    check(
        "DV-07 --tb=line: one 'path:line: msg' line per failure",
        re.search(r"test_tb\.py:3: AssertionError: helper says no", out) is not None,
        out[-300:],
    )
    out = rstest_tb("no")
    check(
        "DV-07 setup: --tb=no run still reports the failure",
        "1 failed" in out,
        out[-200:],
    )
    check(
        "DV-07 --tb=no: no failure block and no captured output",
        "--- FAILED" not in out and "dv-captured-marker" not in out,
        out[-300:],
    )

    # DV-10: terminals. pty runs are POSIX-only; the piped FORCE_COLOR case
    # runs everywhere.
    for i in range(4):
        g.write(
            f"dv_term/test_t{i}.py",
            "import time, pytest\n\n"
            "@pytest.mark.parametrize('i', range(3))\n"
            f"def test_a_rather_long_test_name_to_force_wrapping_{i}(i):\n"
            "    time.sleep(0.15)\n\n"
            f"def test_fail_{i}():\n"
            "    assert {'a': 1, 'b': 2} == {'a': 1, 'b': 3}\n",
        )
    cwd = g.tmp / "dv_term"
    r = g.run("-n", "2", cwd=cwd, env_extra={"FORCE_COLOR": "1"})
    last = r.stdout.strip().splitlines()[-1] if r.stdout.strip() else ""
    check(
        "DV-10 setup: piped run finished with the expected failures",
        r.returncode == 1 and "4 failed" in last,
        last,
    )
    check(
        "DV-10 (d) piped FORCE_COLOR=1: colored consistently (all or nothing)",
        "\x1b" not in r.stdout or "\x1b" in last,
        f"esc={r.stdout.count(chr(27))} summary={last!r}",
    )
    if WINDOWS:
        print("  (pty parts of DV-10 skipped on Windows)")
        return
    rc, out = _pty_run([binary, "-n", "2"], cwd, _env(g))
    check(
        "DV-10 setup: a plain pty run is colored (escape checks are not vacuous)",
        rc == 1 and "\x1b[" in out,
        f"rc={rc}",
    )
    rc, out = _pty_run([binary, "-n", "2"], cwd, _env(g), cols=40)
    footer = [
        ln
        for ln in (_ANSI.sub("", x) for x in re.split(r"\r\n|\n|\r", out))
        if re.search(r"\d+% \(\d+/\d+\)", ln) or re.match(r"gw\d+\s", ln)
    ]
    wide = [ln for ln in footer if len(ln) > 40]
    check("DV-10 setup: the live footer is drawn on a pty", rc == 1 and bool(footer), f"rc={rc}")
    check(
        "DV-10 (a) 40-column pty: live footer lines fit in 40 columns",
        not wide,
        f"{len(wide)} wide, e.g. {wide[:1]}",
    )
    for label, extra, flags in (
        ("(b) TERM=dumb", {"TERM": "dumb"}, []),
        ("(c) NO_COLOR=1", {"NO_COLOR": "1"}, []),
        ("(e) --color=no", {}, ["--color=no"]),
    ):
        rc, out = _pty_run([binary, "-n", "2", *flags], cwd, _env(g, extra))
        check(
            f"DV-10 {label} on a pty: zero escape sequences",
            rc == 1 and "\x1b" not in out,
            f"rc={rc} esc={out.count(chr(27))}",
        )


def gate_local_dev_interrupt_stop(g, args, binary):
    print("== local dev: Ctrl-C history and -x global stop (DV-08, DV-09) ==")

    # DV-09: test_f[0] fails at once, the rest sleep. After the failure is
    # reported no new test may start; pytest prints the stop banner.
    g.write(
        "dv_x/test_x.py",
        "import os, time, pytest\n"
        "LOG = os.path.join(os.path.dirname(__file__), 'starts')\n\n"
        "def _log(kind, i):\n"
        "    with open(f'{LOG}.{os.getpid()}', 'a') as f:\n"
        "        f.write(f'{time.time():.6f} {kind} {i}\\n')\n\n"
        "@pytest.mark.parametrize('i', range(8))\n"
        "def test_f(i):\n"
        "    _log('start', i)\n"
        "    if i == 0:\n"
        "        _log('fail', i)\n"
        "        assert False, 'boom'\n"
        "    time.sleep(0.3)\n",
    )
    xd = g.tmp / "dv_x"
    _clear(xd, "starts")
    r = g.run("-n", "2", "-x", "-v", cwd=xd, timeout=60)
    rows = _log_rows(xd, "starts")
    fail_ts = next((t for t, kind, _ in rows if kind == "fail"), None)
    late = [(round(t - fail_ts, 3), i) for t, kind, i in rows if kind == "start" and fail_ts]
    late = [x for x in late if x[0] > 0.1]
    check(
        "DV-09 -n 2 -x: exit 1 with exactly 1 failure",
        r.returncode == 1 and "1 failed" in r.stdout and fail_ts is not None,
        f"rc={r.returncode} " + r.stdout[-200:],
    )
    check(
        "DV-09 -n 2 -x: no test starts after the failure is reported",
        not late,
        f"started after fail (+s, id): {late}",
    )
    check(
        "DV-09 -n 2 -x: prints 'stopping after 1 failures'",
        "stopping after 1 failures" in _out(r),
        r.stdout[-200:],
    )

    # DV-08: Ctrl-C (SIGINT to the foreground process group) mid-run.
    if WINDOWS:
        print("  (DV-08 skipped on Windows: needs POSIX process-group SIGINT)")
        return
    g.write(
        "dv_intr/test_slow.py",
        "import time, pytest\n\n"
        "@pytest.mark.parametrize('i', range(40))\n"
        "def test_slow(i):\n    time.sleep(0.5)\n",
    )
    idir = g.tmp / "dv_intr"
    shutil.rmtree(idir / ".pytest_cache", ignore_errors=True)
    shutil.rmtree(idir / ".rstest_cache", ignore_errors=True)
    proc = subprocess.Popen(
        [str(binary), "-n", "4"],
        cwd=str(idir),
        env=_env(g),
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        encoding="utf-8",
        start_new_session=True,
    )
    try:
        time.sleep(2.5)
        os.killpg(proc.pid, signal.SIGINT)
        try:
            out, _ = proc.communicate(timeout=30)
        except subprocess.TimeoutExpired:
            _kill_group(proc.pid)
            out, _ = proc.communicate(timeout=10)
            out = "HANG after SIGINT\n" + out
    finally:
        if proc.poll() is None:
            _kill_group(proc.pid)
            proc.wait(timeout=10)
    inflight = set(re.findall(r"^\s+gw\d+\s+(\S+::\S+)", out, re.M))
    check(
        "DV-08 SIGINT: exit 2 and the output says interrupted",
        proc.returncode == 2 and "interrupted" in out,
        f"rc={proc.returncode} " + out[-300:],
    )
    check(
        "DV-08 setup: the interrupt caught tests in flight",
        bool(inflight) and "HANG" not in out,
        out[-300:],
    )
    check(
        "DV-08 SIGINT: output says how many tests did not run",
        re.search(r"\b\d+ (tests? )?(not run|did not run|not started|unrun)", out) is not None,
        out[-300:],
    )
    lf = _read_json(idir / ".pytest_cache/v/cache/lastfailed") or {}
    check(
        "DV-08 SIGINT: lastfailed does not list the in-flight tests",
        not (inflight & set(lf)),
        f"in lastfailed: {sorted(inflight & set(lf))}",
    )
    flakes = _read_json(idir / ".rstest_cache/flakes.json") or {}
    check(
        "DV-08 SIGINT: flakes.json has no entries for the in-flight tests",
        not (inflight & set(flakes)),
        f"in flakes.json: {sorted(inflight & set(flakes))}",
    )
    durs = _read_json(idir / ".rstest_cache/durations.json") or {}
    zero = sorted(n for n in inflight if n in durs and durs[n].get("secs") == 0.0)
    check(
        "DV-08 SIGINT: durations.json has no 0.0 entries for the in-flight tests",
        not zero,
        f"zero durations: {zero}",
    )
