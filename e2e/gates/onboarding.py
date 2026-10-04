"""e2e gate sections: first-time evaluator onboarding (EV-* scenarios).

A developer installs rstest into an existing pytest project, runs `rstest try`
and plain `rstest`, makes the usual newbie mistakes, and reads the output.
Checks tagged `known_bug=True` pin current failures: they xfail today and turn
the gate red once the bug is fixed, so the marker gets dropped and the check
becomes a regression guard.
"""

import json
import os
import re
import subprocess
import sys

from _harness import check, find_python, venv_bin


def _out(r):
    return r.stdout + r.stderr


def _run_workers(g, cwd, *args):
    """Run rstest and return (result, worker count). The count comes from
    report-json `meta.workers` (the banner is hidden under -q); single-worker
    mode records 0 there and is reported as 1."""
    rj = cwd / ".onb-report.json"
    rj.unlink(missing_ok=True)
    r = g.run(*args, "--report-json", str(rj), cwd=cwd)
    try:
        n = json.loads(rj.read_text(encoding="utf-8"))["meta"]["workers"]
    except (OSError, ValueError, KeyError):
        return r, None
    return r, max(n, 1)


def _bare_venv(path):
    """A venv with no packages at all (no pip): fast, and like a project venv
    that rstest was never installed into. Returns (python, purelib)."""
    subprocess.run([find_python(), "-m", "venv", "--without-pip", str(path)], check=True)
    py = venv_bin(path, "python")
    return py, _purelib(py)


def _purelib(py):
    return subprocess.run(
        [str(py), "-c", "import sysconfig; print(sysconfig.get_paths()['purelib'])"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()


def gate_onboarding_try(g, args, binary):
    print("== onboarding: rstest try verdicts (EV-01..EV-04) ==")

    # EV-01: healthy multi-file suite. Parity holds today; the speed line
    # should also say how many workers `-n auto` actually used.
    for i in range(3):
        g.write(
            f"ob_try_ok/test_ok{i}.py",
            "import time, pytest\n\n"
            "@pytest.mark.parametrize('i', range(6))\n"
            "def test_t(i):\n    time.sleep(0.05)\n",
        )
    r = g.run("try", cwd=g.tmp / "ob_try_ok")
    check(
        "EV-01 try: healthy suite reports identical parity, exit 0",
        r.returncode == 0 and "18 tests" in r.stdout and "identical outcomes" in r.stdout,
        f"rc={r.returncode} " + r.stdout[-400:] + r.stderr[-200:],
    )
    speed = next((ln for ln in r.stdout.splitlines() if "speed:" in ln), "")
    check(
        "EV-01 try: speed line names the worker count used",
        re.search(r"-n \d+", speed) is not None,
        speed,
        known_bug=True,
    )

    # EV-02: a syntax error in one file. pytest exits 2 and nothing was
    # compared, so try must not bless the suite.
    g.write("ob_try_syntax/test_ok.py", "def test_ok(): pass\n")
    g.write("ob_try_syntax/test_bad.py", "def test_x(:\n    pass\n")
    r = g.run("try", cwd=g.tmp / "ob_try_syntax")
    check(
        "EV-02 try: collection error is not 'drop-in ready'",
        "drop-in ready" not in r.stdout,
        r.stdout[-400:],
        known_bug=True,
    )
    check(
        "EV-02 try: collection error exits non-zero",
        r.returncode != 0,
        f"rc={r.returncode}",
        known_bug=True,
    )

    # EV-03: an empty project: zero tests compared is not parity.
    g.write("ob_try_empty/.keep", "")
    r = g.run("try", cwd=g.tmp / "ob_try_empty")
    check(
        "EV-03 try: empty suite is not 'drop-in ready' and exits non-zero",
        "drop-in ready" not in r.stdout and r.returncode != 0,
        f"rc={r.returncode} " + r.stdout[-400:],
        known_bug=True,
    )

    # EV-04: a failing unittest subTest. Several files so -n auto really
    # builds a pool (at one worker the outcome is pytest's own and correct).
    for i in range(4):
        g.write(
            f"ob_try_subtest/test_sub{i}.py",
            "import unittest\n\n"
            "class T(unittest.TestCase):\n"
            "    def test_sub(self):\n"
            "        for i in range(3):\n"
            "            with self.subTest(i=i):\n"
            "                self.assertNotEqual(i, 1)\n\n"
            "def test_ok():\n    pass\n",
        )
    r = g.run("try", cwd=g.tmp / "ob_try_subtest")
    silent_ok = "drop-in ready" in r.stdout and "already red" not in r.stdout
    check(
        "EV-04 try: failing subtests are not hidden behind a parity verdict",
        "(0 failing)" not in r.stdout and not silent_ok,
        f"rc={r.returncode} " + r.stdout[-400:],
        known_bug=True,
    )


def gate_onboarding_first_run(g, args, binary):
    print("== onboarding: zero-config first run (EV-05..EV-07) ==")

    # EV-05: plain `rstest` on a small multi-file suite is parallel and
    # pytest-shaped.
    for i in range(4):
        g.write(
            f"ob_first/test_f{i}.py",
            "import time, pytest\n\n"
            "@pytest.mark.parametrize('i', range(10))\n"
            "def test_f(i):\n    time.sleep(0.05)\n",
        )
    r, n = _run_workers(g, g.tmp / "ob_first")
    check(
        "EV-05 first run: parallel by default, pytest-style summary, exit 0",
        r.returncode == 0 and n is not None and n >= 2 and "40 passed" in r.stdout,
        f"rc={r.returncode} workers={n} " + r.stdout[-200:],
    )

    # EV-06: one-file wait-bound suite. --dist load splits within a file, so
    # once the duration cache is warm -n auto should not cap at one worker.
    g.write(
        "ob_onefile/test_io.py",
        "import time, pytest\n\n"
        "@pytest.mark.parametrize('i', range(20))\n"
        "def test_io(i):\n    time.sleep(0.2)\n",
    )
    g.run("-q", cwd=g.tmp / "ob_onefile")  # warm the duration cache
    r, n = _run_workers(g, g.tmp / "ob_onefile", "-q")
    check(
        "EV-06 -n auto: warm one-file wait-bound suite runs on >= 2 workers",
        r.returncode == 0 and n is not None and n >= 2,
        f"rc={r.returncode} workers={n} " + r.stdout[:120],
        known_bug=True,
    )

    # EV-07: selecting one test out of many files must not start a full pool.
    for i in range(12):
        g.write(f"ob_select/tests/test_{i}.py", f"def test_{i}():\n    pass\n")
    r, n = _run_workers(g, g.tmp / "ob_select", "tests/test_1.py::test_1")
    check(
        "EV-07 -n auto: one selected test runs in single-worker mode",
        r.returncode == 0 and n == 1,
        f"rc={r.returncode} workers={n} " + r.stdout[:120],
        known_bug=True,
    )


def gate_onboarding_interpreter(g, args, binary):
    print("== onboarding: interpreter discovery (EV-08, EV-09) ==")
    gate_py = venv_bin(g.venv, "python")
    gate_purelib = _purelib(gate_py)
    sep = ";" if sys.platform == "win32" else ":"
    test_src = "import onb_dep\n\ndef test_dep():\n    assert onb_dep.VALUE == 1\n"

    # EV-08: the project .venv has the project's deps but rstest was never
    # installed into it; another interpreter with rstest is on PATH. rstest
    # must say it skipped .venv (or which interpreter it used), not just fail
    # with the project's ImportError.
    proj = g.tmp / "ob_venv_missing"
    (proj / ".git").mkdir(parents=True, exist_ok=True)  # bound the .venv walk
    g.write("ob_venv_missing/deps/onb_dep.py", "VALUE = 1\n")
    g.write("ob_venv_missing/tests/test_dep.py", test_src)
    _, purelib = _bare_venv(proj / ".venv")
    with open(f"{purelib}/onb_deps.pth", "w", encoding="utf-8") as f:
        f.write(str(proj / "deps") + "\n")
    path = str(gate_py.parent) + sep + os.environ.get("PATH", "")
    r = g.run("-n", "2", cwd=proj, env_extra={"PATH": path}, env_drop=("VIRTUAL_ENV",))
    out = _out(r)
    check(
        "EV-08 setup: rstest fell back past the shim-less project .venv",
        "onb_dep" in out and r.returncode != 0,
        f"rc={r.returncode} " + out[-300:],
    )
    check(
        "EV-08 skipped project .venv is named in the output",
        ".venv" in out,
        out[-300:],
        known_bug=True,
    )

    # EV-09: the project .venv is usable, but a stale .python-version pins a
    # different minor. Either the venv wins (soft pin) or the error names the
    # file the pin came from.
    proj = g.tmp / "ob_pyversion"
    (proj / ".git").mkdir(parents=True, exist_ok=True)
    g.write("ob_pyversion/tests/test_ok.py", "def test_ok():\n    pass\n")
    _, purelib = _bare_venv(proj / ".venv")
    with open(f"{purelib}/onb_gate.pth", "w", encoding="utf-8") as f:
        f.write(gate_purelib + "\n")  # msgpack + pytest: the shim imports
    minor = sys.version_info.minor
    stale = f"3.{minor - 1}" if minor > 10 else f"3.{minor + 1}"
    g.write("ob_pyversion/.python-version", stale + "\n")
    r = g.run("-n", "2", cwd=proj, env_drop=("VIRTUAL_ENV",))
    out = _out(r)
    check(
        "EV-09 setup: the project .venv was rejected only for the version pin",
        r.returncode == 0 or ("/.venv/" in out.replace("\\", "/") and "does not satisfy" in out),
        out[-300:],
    )
    check(
        "EV-09 stale .python-version: project .venv used, or the pin's file is named",
        r.returncode == 0 or ".python-version" in out,
        f"rc={r.returncode} pin={stale} " + out[-300:],
        known_bug=True,
    )


def gate_onboarding_mistakes(g, args, binary):
    print("== onboarding: newbie mistakes (EV-10..EV-12, EV-14) ==")
    for i in range(4):
        g.write(f"ob_mistake/tests/test_{i}.py", "def test_a():\n    pass\n")
    cwd = g.tmp / "ob_mistake"

    # EV-10: nonexistent nodeid / path. Exit 4 like pytest, error printed once.
    r = g.run("-n", "4", "tests/test_1.py::nope", cwd=cwd)
    out = _out(r)
    check("EV-10 bad nodeid exits 4", r.returncode == 4, f"rc={r.returncode}")
    check(
        "EV-10 bad nodeid: 'not found' printed once",
        out.count("not found:") == 1,
        f"count={out.count('not found:')}",
        known_bug=True,
    )
    r = g.run("-n", "4", "tests/missing.py", cwd=cwd)
    out = _out(r)
    check("EV-10 missing path exits 4", r.returncode == 4, f"rc={r.returncode}")
    check(
        "EV-10 missing path: error printed once",
        out.count("file or directory not found") == 1,
        f"count={out.count('file or directory not found')}",
        known_bug=True,
    )

    # EV-11: a typo'd flag (pytest has --lf / --last-failed, not --lastfailed).
    r = g.run("-n", "4", "--lastfailed", cwd=cwd)
    out = _out(r)
    check("EV-11 unknown flag exits 4", r.returncode == 4, f"rc={r.returncode}")
    check(
        "EV-11 unknown flag: error printed once",
        out.count("unrecognized arguments") == 1,
        f"count={out.count('unrecognized arguments')}",
        known_bug=True,
    )
    check(
        "EV-11 unknown flag: usage line does not say pytest.main()",
        "pytest.main()" not in out,
        out[-300:],
        known_bug=True,
    )

    # EV-12: a conftest that cannot import. One traceback, not one per worker.
    for i in range(3):
        g.write(f"ob_conftest/tests/test_{i}.py", "def test_a():\n    pass\n")
    g.write("ob_conftest/tests/conftest.py", "import onb_nonexistent_mod\n")
    r = g.run("-n", "3", cwd=g.tmp / "ob_conftest")
    out = _out(r)
    check("EV-12 broken conftest exits non-zero", r.returncode != 0, f"rc={r.returncode}")
    check(
        "EV-12 broken conftest: ImportError header printed once",
        out.count("ImportError while loading conftest") == 1,
        f"count={out.count('ImportError while loading conftest')}",
        known_bug=True,
    )

    # EV-14: a global option before the subcommand must not turn the
    # subcommand into a test path.
    r = g.run("-q", "try", cwd=cwd)
    check(
        "EV-14 'rstest -q try' runs try or explains the argument order",
        "file or directory not found: try" not in _out(r),
        f"rc={r.returncode} " + _out(r)[-300:],
        known_bug=True,
    )


def gate_onboarding_location(g, args, binary):
    print("== onboarding: run location (EV-13) ==")
    # EV-13: running from a subdirectory must use the rootdir's cache, as
    # pytest does with .pytest_cache.
    g.write("ob_loc/pyproject.toml", "[tool.pytest.ini_options]\ntestpaths = ['tests']\n")
    g.write("ob_loc/tests/unit/test_a.py", "def test_a():\n    pass\n")
    g.write("ob_loc/tests/unit/test_b.py", "def test_b():\n    pass\n")
    root = g.tmp / "ob_loc"
    r = g.run("-n", "2", cwd=root)
    check(
        "EV-13 root run writes .rstest_cache at the rootdir",
        r.returncode == 0 and (root / ".rstest_cache").is_dir(),
        f"rc={r.returncode} " + r.stdout[-200:],
    )
    r = g.run("-n", "2", cwd=root / "tests" / "unit")
    check(
        "EV-13 subdirectory run does not create a second .rstest_cache",
        r.returncode == 0 and not (root / "tests" / "unit" / ".rstest_cache").exists(),
        f"rc={r.returncode} " + r.stdout[-200:],
        known_bug=True,
    )


def gate_onboarding_summary(g, args, binary):
    print("== onboarding: summary wording (EV-15) ==")
    # EV-15: singular nouns as pytest prints them.
    g.write(
        "ob_summary/test_a.py",
        "import warnings, pytest\n\n"
        "@pytest.fixture\n"
        "def broken():\n    raise RuntimeError('setup boom')\n\n"
        "def test_err(broken):\n    pass\n\n"
        "def test_ok():\n    pass\n",
    )
    g.write(
        "ob_summary/test_b.py",
        "import warnings\n\ndef test_warn():\n    warnings.warn('old', DeprecationWarning)\n",
    )
    for mode, bug in (("0", False), ("2", True)):
        r = g.run("-n", mode, cwd=g.tmp / "ob_summary")
        last = r.stdout.strip().splitlines()[-1] if r.stdout.strip() else ""
        ok = (
            re.search(r"\b1 error\b(?!s)", last) is not None
            and re.search(r"\b1 warning\b(?!s)", last) is not None
        )
        check(
            f"EV-15 -n {mode}: summary says '1 error' and '1 warning'",
            ok,
            last,
            known_bug=bug,
        )
