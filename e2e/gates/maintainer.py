"""e2e gate sections: suite maintainer (MT-* scenarios).

A tech lead owns suite health over months: reads `rstest --doctor`, turns its
metrics into CI gates (`--doctor-fail-on`, `--durations-regress`,
`--fail-on-leak`, `--cov-fail-under`), and handles flaky and order-dependent
tests (reruns, quarantine, `replay`, `bisect`). The numbers must be right and
every gate must fire exactly when its condition is true.
Checks tagged `known_bug=True` pin current failures: they xfail today and turn
the gate red once the bug is fixed, so the marker gets dropped and the check
becomes a regression guard. Timing-based checks assert on ratios with wide
margins, not on tight wall times.
"""

import json
import re
import subprocess
import time
import uuid
from types import SimpleNamespace

from _harness import REPO, check, venv_bin


def _out(r):
    return r.stdout + r.stderr


def _run(g, cwd, *args, env=None, timeout=90):
    """g.run that turns a hang into a failed result (rc None) instead of an
    exception, so a hang is a failed check, not a crashed gate."""
    try:
        return g.run(*args, cwd=cwd, env_extra=env, timeout=timeout)
    except subprocess.TimeoutExpired:
        return SimpleNamespace(returncode=None, stdout="", stderr=f"TIMEOUT after {timeout}s")


def _timed(g, cwd, *args, env=None):
    t0 = time.monotonic()
    r = _run(g, cwd, *args, env=env)
    return r, time.monotonic() - t0


def _json(path):
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}


def _counts(path):
    return _json(path).get("meta", {}).get("counts", {})


def _breached(stderr, metric):
    """True when the doctor gate failure block has a breach line for metric
    (`  wait_pct = 44.00 > 10.00 (wait_pct>10)`)."""
    return re.search(rf"^\s+{re.escape(metric)} = ", stderr, re.M) is not None


# --------------------------------------------------------------------------
# MT-01, MT-08, MT-13: doctor numbers
# --------------------------------------------------------------------------


def gate_maintainer_doctor_numbers(g, args, binary):
    print("== maintainer: doctor numbers (MT-01, MT-08, MT-13) ==")

    # MT-01: time lives in a function fixture (0.25s setup + 0.1s teardown),
    # the call itself is 0.02s. Doctor must count the whole protocol.
    g.write(
        "mt_doctor_fx/test_fx.py",
        "import time, pytest\n\n"
        "@pytest.fixture\n"
        "def slow():\n"
        "    time.sleep(0.25)\n"
        "    yield\n"
        "    time.sleep(0.1)\n\n"
        "@pytest.mark.parametrize('i', range(12))\n"
        "def test_fx(slow, i):\n"
        "    time.sleep(0.02)\n",
    )
    cwd = g.tmp / "mt_doctor_fx"
    d0, d4 = cwd / "d0.json", cwd / "d4.json"
    r0, serial_wall = _timed(g, cwd, "-n", "0", "--doctor", "--doctor-json", str(d0))
    r4, pool_wall = _timed(
        g,
        cwd,
        "-n",
        "4",
        "--doctor",
        "--doctor-json",
        str(d4),
        "--doctor-fail-on",
        "parallel_efficiency<30",
    )
    actual = serial_wall / max(pool_wall, 1e-6)
    check(
        "MT-01 setup: -n 4 really ran in parallel (>= 2x faster than -n 0)",
        r0.returncode == 0 and "12 passed" in r4.stdout and actual >= 2.0,
        f"rc0={r0.returncode} serial={serial_wall:.2f}s pool={pool_wall:.2f}s",
    )
    doc = _json(d4)
    pe = doc.get("parallel_efficiency") or {}
    realized = pe.get("realized_speedup", 0.0)
    check(
        "MT-01 doctor realized speedup within 40% of wall(-n 0)/wall(-n 4)",
        actual > 0 and abs(realized / actual - 1) <= 0.4,
        f"realized={realized:.2f} actual={actual:.2f}",
        known_bug=True,
    )
    check(
        "MT-01 doctor parallel_efficiency > 50 on a ~3.5x run",
        pe.get("efficiency_pct", 0.0) > 50,
        f"efficiency_pct={pe.get('efficiency_pct')}",
        known_bug=True,
    )
    files = doc.get("slowest_files") or [{}]
    check(
        "MT-01 SLOWEST FILES counts fixture time (>= 2s of the ~4.4s)",
        files[0].get("total_seconds", 0.0) >= 2.0,
        str(files[0]),
        known_bug=True,
    )
    check(
        "MT-01 WAIT-BOUND section present for a sleep-bound fixture suite",
        "WAIT-BOUND" in r4.stdout,
        r4.stdout[-300:],
        known_bug=True,
    )
    check(
        "MT-01 'parallel_efficiency<30' does not fire on a ~3.5x run",
        r4.returncode == 0,
        f"rc={r4.returncode} " + r4.stderr[-200:],
        known_bug=True,
    )

    # MT-13: the -n 0 run above is single-worker: it must not say 0 workers
    # or talk about "all workers".
    check(
        "MT-13 -n 0 doctor says 1 worker / single-worker, not 0 workers",
        "0 workers" not in r0.stdout and ("1 worker" in r0.stdout or "single-worker" in r0.stdout),
        next((ln for ln in r0.stdout.splitlines() if "test time" in ln), ""),
        known_bug=True,
    )
    check(
        "MT-13 -n 0 doctor-json workers >= 1",
        _json(d0).get("workers", 0) >= 1,
        f"workers={_json(d0).get('workers')}",
        known_bug=True,
    )
    check(
        "MT-13 -n 0 doctor headings do not say 'across all workers'",
        "across all workers" not in r0.stdout,
        next((ln for ln in r0.stdout.splitlines() if "across all workers" in ln), ""),
        known_bug=True,
    )

    # MT-08: CPU-bound work in a child process is computing, not waiting.
    g.write(
        "mt_doctor_cli/test_cli.py",
        "import subprocess, sys, pytest\n\n"
        "LOOP = 'n = 0\\nfor i in range(15_000_000):\\n    n += i\\n'\n\n"
        "@pytest.mark.parametrize('i', range(2))\n"
        "def test_cli_cpu(i):\n"
        "    subprocess.run([sys.executable, '-c', LOOP], check=True)\n",
    )
    cwd = g.tmp / "mt_doctor_cli"
    dj = cwd / "d.json"
    r = _run(g, cwd, "-n", "2", "--doctor", "--doctor-json", str(dj))
    doc = _json(dj)
    check(
        "MT-08 setup: CPU suite is above the WAIT-BOUND display floor (>= 1s)",
        r.returncode == 0 and doc.get("test_time_seconds", 0.0) >= 1.0,
        f"rc={r.returncode} test_time={doc.get('test_time_seconds')}",
    )
    wb = doc.get("wait_bound") or {}
    check(
        "MT-08 subprocess CPU is not reported as waiting (wait_pct < 70)",
        wb.get("wait_pct", 0.0) < 70 and "test_cli_cpu" not in r.stdout.split("WAIT-BOUND")[-1],
        f"wait_pct={wb.get('wait_pct')}",
        known_bug=True,
    )


# --------------------------------------------------------------------------
# MT-02, MT-03, MT-04: --doctor-fail-on
# --------------------------------------------------------------------------

# One 0.8s sleep + 1.0s of CPU: wait ~44% of 1.8s, long pole 0.8s.
_WAIT_SUITE = (
    "import time\n\n"
    "def _spin(sec):\n"
    "    end = time.process_time() + sec\n"
    "    while time.process_time() < end:\n"
    "        pass\n\n"
    "def test_sleep():\n    time.sleep(0.8)\n\n"
    "def test_cpu_a():\n    _spin(0.5)\n\n"
    "def test_cpu_b():\n    _spin(0.5)\n"
)

# (condition, true on the suite above, bug id when it does not fire today).
# The -n 0 table leaves out the pool-only metrics (efficiency, speedup,
# imbalance): cli.md documents them as not measured without a pool.
_GATES_ANY = [
    ("wall_seconds>0.5", None),
    ("test_time_seconds>1.0", None),
    ("cpu_time_seconds>0.5", None),
    ("tests>2", None),
    ("wait_seconds>0.5", "P6"),
    ("wait_pct>10", "P6"),
]
_GATES_N0 = [*_GATES_ANY, ("long_pole_seconds>0.5", "P6")]
_GATES_N2 = [
    *_GATES_ANY,
    ("workers>1", None),
    ("long_pole_seconds>0.5", None),
    ("parallel_efficiency<101", None),
    ("efficiency_pct<101", None),
    ("realized_speedup>0", None),
    ("imbalance_pct>=0", None),
]


def gate_maintainer_doctor_gates(g, args, binary):
    print("== maintainer: --doctor-fail-on gates (MT-02..MT-04) ==")
    g.write("mt_gate_wait/test_w.py", _WAIT_SUITE)
    cwd = g.tmp / "mt_gate_wait"

    # MT-02: every metric whose condition is true fires a breach line. One
    # run per mode with all conditions; each breach line is checked on its own.
    for n, table in (("0", _GATES_N0), ("2", _GATES_N2)):
        fail_on = [a for cond, _ in table for a in ("--doctor-fail-on", cond)]
        r = _run(g, cwd, "-n", n, "-q", *fail_on)
        check(
            f"MT-02 -n {n}: a breached gate exits 1",
            r.returncode == 1 and "doctor gate failures" in r.stderr,
            f"rc={r.returncode} " + r.stderr[-300:],
        )
        for cond, bug in table:
            metric = re.split(r"[<>=!]", cond, maxsplit=1)[0]
            check(
                f"MT-02 -n {n}: '{cond}' fires (breach line, never 'condition skipped')",
                _breached(r.stderr, metric) and f"'{cond}' not measured" not in r.stderr,
                next((ln for ln in r.stderr.splitlines() if cond in ln), "")[:160],
                known_bug=bug is not None,
            )

    # MT-04: a skipped condition is not a passed one.
    r = _run(
        g,
        cwd,
        "-n",
        "0",
        "-q",
        "--doctor-fail-on",
        "parallel_efficiency<1",
        "--doctor-fail-on",
        "tests>100",
    )
    check(
        "MT-04 setup: one condition skipped, none breached, exit 0",
        r.returncode == 0 and "not measured" in r.stderr,
        f"rc={r.returncode} " + r.stderr[-300:],
    )
    check(
        "MT-04 summary does not say 'all 2 condition(s) passed' when one was skipped",
        "all 2 condition(s) passed" not in r.stderr and "skipped" in r.stderr.splitlines()[-1],
        r.stderr.strip().splitlines()[-1] if r.stderr.strip() else "",
        known_bug=True,
    )

    # MT-03: grammar validation. Bad conditions abort before any test runs
    # and name the bad part. rstest's own flag errors exit 1 (exit-codes.md),
    # so "non-zero, nothing ran" is the contract, not pytest's 4.
    g.write("mt_gate_grammar/test_a.py", "def test_a():\n    pass\n")
    cwd = g.tmp / "mt_gate_grammar"
    cases = [
        ("unknown_metric>1", "unknown_metric", False),
        ("tests>>1", ">1", False),
        ("", "no comparison operator", False),
        ("wait_pct>NaN", "NaN", True),
        ("tests!=inf", "inf", True),
    ]
    for cond, named, bug in cases:
        r = _run(g, cwd, "-n", "2", "--doctor-fail-on", cond)
        check(
            f"MT-03 '{cond}' rejected before the run, naming '{named}'",
            r.returncode not in (0, None)
            and " passed" not in r.stdout
            and "Error" in r.stderr
            and named in r.stderr,
            f"rc={r.returncode} " + _out(r)[-200:],
            known_bug=bug,
        )


# --------------------------------------------------------------------------
# MT-05: --durations-regress
# --------------------------------------------------------------------------

_POLL = (
    "import os, time\n\n"
    "def test_poll():\n"
    "    assert not os.environ.get('MT_FAIL'), 'fails fast'\n"
    "    time.sleep(float(os.environ.get('MT_D', '0.1')))\n\n"
    "def test_other():\n"
    "    time.sleep(0.02)\n"
)


def _explain_secs(g, cwd, nodeid):
    r = _run(g, cwd, "explain", nodeid)
    m = re.search(r"duration\s+([\d.]+)s", r.stdout)
    return float(m.group(1)) if m else None


def gate_maintainer_durations_regress(g, args, binary):
    print("== maintainer: --durations-regress baseline (MT-05) ==")

    # A: baseline 0.1s, then the same 1.0s regression twice. The gate must
    # keep firing until the test is fixed, not adopt the regressed time.
    g.write("mt_dreg_twice/test_p.py", _POLL)
    cwd = g.tmp / "mt_dreg_twice"
    _run(g, cwd, "-n", "2", "-q", env={"MT_D": "0.1"})
    regress = ("-n", "2", "-q", "--durations-regress", "2")
    r1 = _run(g, cwd, *regress, env={"MT_D": "1.0"})
    check(
        "MT-05 setup: first 0.1s -> 1.0s run flags test_poll, exit 1",
        r1.returncode == 1 and "test_poll" in _out(r1),
        f"rc={r1.returncode} " + _out(r1)[-300:],
    )
    r2 = _run(g, cwd, *regress, env={"MT_D": "1.0"})
    check(
        "MT-05 second identical regressed run still exits 1 and names test_poll",
        r2.returncode == 1 and "test_poll" in _out(r2),
        f"rc={r2.returncode} " + r2.stderr[-200:],
        known_bug=True,
    )

    # B: baseline 0.3s, then a run where test_poll fails fast. A failed run's
    # 0.0001s must not become the baseline: explain keeps ~0.3s and a later
    # 1.0s run is still a regression.
    g.write("mt_dreg_fail/test_p.py", _POLL)
    cwd = g.tmp / "mt_dreg_fail"
    _run(g, cwd, "-n", "2", "-q", env={"MT_D": "0.3"})
    before = _explain_secs(g, cwd, "test_p.py::test_poll")
    rf = _run(g, cwd, "-n", "2", "-q", env={"MT_FAIL": "1"})
    check(
        "MT-05 setup: baseline ~0.3s recorded, fail-fast run exits 1",
        before is not None and before >= 0.25 and rf.returncode == 1,
        f"before={before} rc={rf.returncode}",
    )
    after = _explain_secs(g, cwd, "test_p.py::test_poll")
    check(
        "MT-05 failed run does not overwrite the baseline (explain still ~0.3s)",
        after is not None and after >= 0.25,
        f"explain duration={after}",
        known_bug=True,
    )
    r = _run(g, cwd, *regress, env={"MT_D": "1.0"})
    check(
        "MT-05 after a failed run, a 0.3s -> 1.0s regression still fires",
        r.returncode == 1 and "test_poll" in _out(r),
        f"rc={r.returncode} " + r.stderr[-200:],
        known_bug=True,
    )


# --------------------------------------------------------------------------
# MT-06, MT-07: --fail-on-leak
# --------------------------------------------------------------------------


def gate_maintainer_leaks(g, args, binary):
    print("== maintainer: leak gate attribution (MT-06, MT-07) ==")

    # MT-06a: a module-scoped server fixture's teardown runs inside the last
    # test, cancelling that test's own permanent thread.
    g.write(
        "mt_leak_module/test_srv.py",
        "import threading, time, pytest\n\n"
        "@pytest.fixture(scope='module')\n"
        "def server():\n"
        "    stop = threading.Event()\n"
        "    t = threading.Thread(target=stop.wait, daemon=True)\n"
        "    t.start()\n"
        "    yield\n"
        "    stop.set()\n"
        "    t.join()\n\n"
        "def test_warmup():\n    pass\n\n"
        "def test_uses_server(server):\n    pass\n\n"
        "def test_last_leaks(server):\n"
        "    threading.Thread(target=time.sleep, args=(60,), daemon=True).start()\n",
    )
    # MT-06b: test_b's thread ends during test_c (made deterministic with an
    # event + join), while test_c starts a permanent one. Net 0 hides test_c.
    g.write(
        "mt_leak_late/test_late.py",
        "import threading, time\n\n"
        "GO = threading.Event()\n"
        "T = []\n\n"
        "def test_a_warmup():\n    pass\n\n"
        "def test_b_short_thread():\n"
        "    T.append(threading.Thread(target=GO.wait, daemon=True))\n"
        "    T[0].start()\n\n"
        "def test_c_permanent():\n"
        "    GO.set()\n"
        "    T[0].join()\n"
        "    threading.Thread(target=time.sleep, args=(60,), daemon=True).start()\n",
    )
    # --dist loadfile keeps the file on one worker in file order, so the
    # warm-up test is the first one there (resource-leaks.md).
    for n in ("0", "2"):
        dist = ("--dist", "loadfile") if n == "2" else ()
        r = _run(g, g.tmp / "mt_leak_module", "-n", n, "-q", "--fail-on-leak", *dist)
        check(f"MT-06a -n {n}: a leak fails the run (exit 1)", r.returncode == 1, _out(r)[-300:])
        check(
            f"MT-06a -n {n}: test_last_leaks is blamed, not test_uses_server",
            "test_last_leaks" in r.stderr and "test_uses_server" not in r.stderr,
            r.stderr[-300:],
            known_bug=True,
        )
        r = _run(g, g.tmp / "mt_leak_late", "-n", n, "-q", "--fail-on-leak", *dist)
        check(
            f"MT-06b -n {n}: test_c_permanent's permanent thread is reported",
            r.returncode == 1 and "test_c_permanent" in r.stderr,
            f"rc={r.returncode} " + r.stderr[-300:],
            known_bug=True,
        )

    # MT-07: common clean patterns never trip the gate. Two files so -n 2
    # puts one file per worker; each starts with an unchecked warm-up test.
    g.write(
        "mt_leak_clean/test_io.py",
        "import subprocess, sys\n\n"
        "def test_0_warmup():\n    pass\n\n"
        "def test_capfd(capfd):\n"
        "    print('hello')\n"
        "    sys.stderr.write('err\\n')\n"
        "    assert capfd.readouterr() == ('hello\\n', 'err\\n')\n\n"
        "def test_subprocess_run():\n"
        "    r = subprocess.run(\n"
        "        [sys.executable, '-c', 'print(1)'], capture_output=True, text=True\n"
        "    )\n"
        "    assert r.stdout.strip() == '1'\n\n"
        "def test_tmp_file(tmp_path):\n"
        "    with open(tmp_path / 'f.txt', 'w') as fh:\n"
        "        fh.write('x')\n"
        "    assert (tmp_path / 'f.txt').read_text() == 'x'\n",
    )
    g.write(
        "mt_leak_clean/test_conc.py",
        "import asyncio\n"
        "from concurrent.futures import ThreadPoolExecutor\n\n"
        "def test_0_warmup():\n    pass\n\n"
        "def test_asyncio_run():\n"
        "    async def f():\n"
        "        await asyncio.sleep(0.01)\n"
        "        return 1\n"
        "    assert asyncio.run(f()) == 1\n\n"
        "def test_thread_pool_with():\n"
        "    with ThreadPoolExecutor(max_workers=4) as ex:\n"
        "        assert sum(ex.map(lambda x: x * 2, range(10))) == 90\n",
    )
    for n in ("0", "2"):
        r = _run(g, g.tmp / "mt_leak_clean", "-n", n, "-q", "--fail-on-leak", "--dist", "loadfile")
        check(
            f"MT-07 -n {n}: capfd/asyncio.run/executor/subprocess suite passes the leak gate",
            r.returncode == 0 and "no thread/fd leaks detected" in r.stderr,
            f"rc={r.returncode} " + r.stderr[-300:],
        )

    # MT-07: a session-scoped server that is shut down at session end. The
    # spec expects no leak; resource-leaks.md documents it as a known false
    # positive charged to the first test that uses the fixture.
    g.write(
        "mt_leak_session/conftest.py",
        "import threading, pytest\n"
        "from http.server import HTTPServer, BaseHTTPRequestHandler\n\n"
        "@pytest.fixture(scope='session')\n"
        "def http_server():\n"
        "    srv = HTTPServer(('127.0.0.1', 0), BaseHTTPRequestHandler)\n"
        "    t = threading.Thread(target=srv.serve_forever, daemon=True)\n"
        "    t.start()\n"
        "    yield srv.server_address\n"
        "    srv.shutdown()\n"
        "    srv.server_close()\n"
        "    t.join()\n",
    )
    g.write(
        "mt_leak_session/test_srv.py",
        "import pytest\n\n"
        "def test_0_warmup():\n    pass\n\n"
        "@pytest.mark.parametrize('i', range(2))\n"
        "def test_uses_server(http_server, i):\n"
        "    assert http_server[1] > 0\n",
    )
    r = _run(g, g.tmp / "mt_leak_session", "-n", "0", "-q", "--fail-on-leak")
    check(
        "MT-07 -n 0: shut-down session-scoped server is not a leak",
        r.returncode == 0 and "no thread/fd leaks detected" in r.stderr,
        f"rc={r.returncode} " + r.stderr[-200:],
        known_bug=True,
    )


# --------------------------------------------------------------------------
# MT-09: coverage gate in every report mode
# --------------------------------------------------------------------------

_COV_CALC = (
    "def add(a, b):\n    return a + b\n\n\n"
    "def sub(a, b):\n    return a - b\n\n\n"
    "def classify(n):\n"
    "    if n < 0:\n        return 'neg'\n"
    "    if n == 0:\n        return 'zero'\n"
    "    return 'pos'\n\n\n"
    "def unused_one(x):\n    y = x + 1\n    return y\n\n\n"
    "def unused_two(x):\n    return -x\n"
)

# Report modes of MT-09 and the bug that breaks "exit 1 + exactly one
# `FAIL Required test coverage` line" in that mode today.
_COV_MODES = [
    ("empty", ["--cov-report="], {"2": "P5"}),
    ("term", ["--cov-report=term"], {"0": "S5"}),
    ("term-missing:skip-covered", ["--cov-report=term-missing:skip-covered"], {"0": "S5"}),
    ("annotate", ["--cov-report=annotate"], {"2": "P5"}),
    ("xml", ["--cov-report=xml"], {"0": "S5"}),
    ("term+xml", ["--cov-report=term", "--cov-report=xml"], {"0": "S5", "2": "S5"}),
]


def _write_cov_project(g, name):
    g.write(f"{name}/pkg/__init__.py", "")
    g.write(f"{name}/pkg/full.py", "def double(x):\n    return x * 2\n")
    g.write(f"{name}/pkg/calc.py", _COV_CALC)
    g.write(
        f"{name}/tests/test_a.py",
        "from pkg.calc import add, classify\n"
        "from pkg.full import double\n\n"
        "def test_add():\n    assert add(1, 2) == 3\n\n"
        "def test_classify():\n"
        "    assert [classify(n) for n in (-1, 0, 1)] == ['neg', 'zero', 'pos']\n\n"
        "def test_double():\n    assert double(2) == 4\n",
    )
    g.write(
        f"{name}/tests/test_b.py",
        "from pkg.calc import sub\n\ndef test_sub():\n    assert sub(3, 1) == 2\n",
    )
    return g.tmp / name


def _totals(text):
    return [" ".join(ln.split()) for ln in text.splitlines() if ln.startswith("TOTAL")]


def _fail_lines(text):
    return text.count("FAIL Required test coverage")


def _table_rows(text, fname):
    return [ln for ln in text.splitlines() if ln.startswith("pkg/") and fname in ln]


def gate_maintainer_coverage_gate(g, args, binary):
    print("== maintainer: --cov-fail-under in every report mode (MT-09) ==")
    py = str(venv_bin(g.venv, "python"))
    cwd = _write_cov_project(g, "mt_cov")

    # Oracle: pytest-cov on the same project.
    o = subprocess.run(
        [py, "-m", "pytest", "-q", "-p", "no:cacheprovider", "--cov=pkg", "--cov-report=term"],
        cwd=cwd,
        capture_output=True,
        text=True,
        timeout=120,
    )
    oracle = _totals(o.stdout)
    check(
        "MT-09 setup: pytest-cov reports the suite at ~82%",
        o.returncode == 0 and len(oracle) == 1 and oracle[0].endswith(" 82%"),
        str(oracle) + o.stdout[-200:],
    )
    o = subprocess.run(
        [py, "-m", "pytest", "-q", "-p", "no:cacheprovider", "--cov=pkg", "--cov-fail-under=95"],
        cwd=cwd,
        capture_output=True,
        text=True,
        timeout=120,
    )
    check(
        "MT-09 setup: pytest-cov --cov-fail-under=95 exits 1",
        o.returncode == 1 and _fail_lines(o.stdout) == 1,
        f"rc={o.returncode}",
    )

    for n in ("0", "2"):
        totals_seen = []
        for label, reports, bugs in _COV_MODES:
            r = _run(g, cwd, "-n", n, "--cov=pkg", *reports, "--cov-fail-under=95")
            out = _out(r)
            totals_seen += _totals(out)
            check(
                f"MT-09 -n {n} --cov-report {label}: exit 1, exactly one FAIL line",
                r.returncode == 1 and _fail_lines(out) == 1,
                f"rc={r.returncode} fail_lines={_fail_lines(out)}",
                known_bug=n in bugs,
            )
            if label == "term-missing:skip-covered":
                check(
                    f"MT-09 -n {n} skip-covered: fully covered pkg/full.py not listed",
                    "pkg/full.py" not in out and "pkg/calc.py" in out,
                    str(_table_rows(out, "full.py")),
                    known_bug=True,
                )
        check(
            f"MT-09 -n {n}: every TOTAL line equals pytest-cov's",
            bool(totals_seen) and set(totals_seen) == set(oracle),
            f"rstest={sorted(set(totals_seen))} pytest={oracle}",
        )

    # [report] fail_under + show_missing from .coveragerc, no flag.
    cwd = _write_cov_project(g, "mt_cov_rc")
    g.write("mt_cov_rc/.coveragerc", "[report]\nfail_under = 95\nshow_missing = True\n")
    for n in ("0", "2"):
        r = _run(g, cwd, "-n", n, "--cov=pkg", "--cov-report=term")
        out = _out(r)
        check(
            f"MT-09 -n {n} .coveragerc fail_under=95: exit 1, one FAIL line",
            r.returncode == 1 and _fail_lines(out) == 1,
            f"rc={r.returncode} fail_lines={_fail_lines(out)}",
            known_bug=n == "2",
        )
        check(
            f"MT-09 -n {n} .coveragerc show_missing honoured (Missing column)",
            "18-19, 23" in out,
            str(_table_rows(out, "calc.py")),
            known_bug=n == "2",
        )

    # --cov-config pointing at a config with a non-default data_file.
    cwd = _write_cov_project(g, "mt_cov_cfg")
    g.write("mt_cov_cfg/cov.cfg", "[run]\ndata_file = covdata/.coverage\n")
    for n in ("0", "2"):
        r = _run(
            g,
            cwd,
            "-n",
            n,
            "--cov=pkg",
            "--cov-config=cov.cfg",
            "--cov-report=term",
            "--cov-fail-under=95",
        )
        out = _out(r)
        check(
            f"MT-09 -n {n} --cov-config: exit 1, one FAIL line, totals match, no 'No data'",
            r.returncode == 1
            and _fail_lines(out) == 1
            and _totals(out) == oracle
            and "No data to report" not in out,
            f"rc={r.returncode} fail_lines={_fail_lines(out)} totals={_totals(out)}",
            known_bug=True,
        )


# --------------------------------------------------------------------------
# MT-10, MT-14: pool scheduling a maintainer relies on
# --------------------------------------------------------------------------


def gate_maintainer_pool(g, args, binary):
    print("== maintainer: serial session reuse + long-pole spread (MT-10, MT-14) ==")

    # MT-10: an expensive session fixture (a DB) used by parallel and
    # @serial tests. Serial tests run on a worker that already has the
    # session, so they add no set-ups.
    g.write(
        "mt_serial/conftest.py",
        "import os, uuid, pytest\n\n"
        "@pytest.fixture(scope='session')\n"
        "def db():\n"
        "    w = os.environ.get('RSTEST_WORKER_ID') or 'main'\n"
        "    p = os.path.join(os.environ['MT_LOG_DIR'], f'up.{w}.{uuid.uuid4().hex}')\n"
        "    with open(p, 'w') as f:\n"
        "        f.write(w)\n"
        "    yield\n",
    )
    g.write(
        "mt_serial/test_db.py",
        "import os, time, pytest\n\n"
        "def _ran(name):\n"
        "    p = os.path.join(os.environ['MT_LOG_DIR'], f'ran.{name}')\n"
        "    with open(p, 'w') as f:\n"
        "        f.write(os.environ.get('RSTEST_WORKER_ID') or 'main')\n\n"
        "@pytest.mark.parametrize('i', range(6))\n"
        "def test_par(db, i):\n"
        "    time.sleep(0.05)\n"
        "    _ran(f'par{i}')\n\n"
        "@pytest.mark.serial\n"
        "@pytest.mark.parametrize('i', range(3))\n"
        "def test_serial(db, i):\n"
        "    _ran(f'serial{i}')\n",
    )
    cwd = g.tmp / "mt_serial"
    for n in ("0", "2"):
        log = cwd / f"log{n}"
        log.mkdir(exist_ok=True)
        r = _run(g, cwd, "-n", n, "-q", "test_db.py", env={"MT_LOG_DIR": str(log)})
        ran = {p.name[4:]: p.read_text() for p in log.glob("ran.*")}
        ups = list(log.glob("up.*"))
        par_workers = {w for k, w in ran.items() if k.startswith("par")}
        serial_workers = {w for k, w in ran.items() if k.startswith("serial")}
        check(
            f"MT-10 -n {n} setup: 9 passed, serial tests on one worker",
            r.returncode == 0 and len(ran) == 9 and len(serial_workers) == 1,
            f"rc={r.returncode} ran={ran}",
        )
        check(
            f"MT-10 -n {n}: session set-ups == workers that ran parallel tests",
            len(ups) == len(par_workers),
            f"setups={len(ups)} parallel_workers={sorted(par_workers)}",
        )

    # MT-14: warm cache, four 1s tests + one fast test at -n 4. Cached long
    # poles go out first, one per worker.
    g.write(
        "mt_poles/test_poles.py",
        "import time, pytest\n\n"
        "@pytest.mark.parametrize('i', range(4))\n"
        "def test_slow(i):\n    time.sleep(1.0)\n\n"
        "def test_fast():\n    pass\n",
    )
    cwd = g.tmp / "mt_poles"
    _run(g, cwd, "-n", "4", "-q")  # warm the duration cache
    rj = cwd / "r.json"
    r = _run(g, cwd, "-n", "4", "-v", "--doctor", "--report-json", str(rj))
    rep = _json(rj)
    slow_workers = [t.get("worker") for k, t in rep.get("tests", {}).items() if "test_slow" in k]
    wall = rep.get("meta", {}).get("duration_seconds", 99.0)
    check(
        "MT-14 setup: 5 passed with 4 workers",
        r.returncode == 0 and "5 passed" in r.stdout and len(slow_workers) == 4,
        f"rc={r.returncode} " + r.stdout[-200:],
    )
    check(
        "MT-14 each 1s test runs on a different worker",
        len(set(slow_workers)) == 4,
        f"workers={slow_workers}",
    )
    check(
        "MT-14 wall < 1.75s (one long pole per worker, not two)",
        wall < 1.75,
        f"wall={wall}",
    )
    check(
        "MT-14 doctor does not report a PARALLEL FLOOR problem",
        "PARALLEL FLOOR" not in r.stdout,
        next((ln for ln in r.stdout.splitlines() if "PARALLEL FLOOR" in ln), ""),
        known_bug=True,
    )


# --------------------------------------------------------------------------
# MT-11, MT-12: reproducing and handling flaky / order-dependent tests
# --------------------------------------------------------------------------


_FILL = "def test_f():\n    pass\n\ndef test_g():\n    pass\n"


def gate_maintainer_repro(g, args, binary):
    print("== maintainer: reproduce a parallel-only failure (MT-11) ==")
    # Victim collects before its polluter; they only clash when shuffled
    # onto the same worker in polluter-first order.
    g.write("mt_repro/tests/__init__.py", "")
    g.write("mt_repro/tests/state.py", "DEBUG = False\n")
    g.write(
        "mt_repro/tests/test_report.py",
        "from tests import state\n\n"
        "def test_totals():\n"
        "    assert not state.DEBUG, 'debug left on by an earlier test'\n",
    )
    g.write(
        "mt_repro/tests/test_zz_debug.py",
        "from tests import state\n\ndef test_enable_debug():\n    state.DEBUG = True\n",
    )
    for i in range(4):
        g.write(f"mt_repro/tests/test_fill{i}.py", _FILL)
    cwd = g.tmp / "mt_repro"
    seed = None
    for s in range(1, 41):
        r = _run(g, cwd, "-n", "2", "-q", f"--shuffle={s}")
        if r.returncode == 1 and "test_totals" in r.stdout:
            seed = s
            break
    check("MT-11 setup: a shuffled -n 2 run hit the order-dependent failure", seed is not None)
    if seed is None:
        return
    fails = 0
    for _ in range(5):
        r = _run(g, cwd, "replay")
        fails += r.returncode == 1 and "test_totals" in r.stdout
    check(f"MT-11 replay reproduces the seed-{seed} failure 5/5", fails == 5, f"{fails}/5")
    r = _run(g, cwd, "bisect", "tests/test_report.py::test_totals")
    check(
        "MT-11 bisect names the polluter that collects after the victim",
        r.returncode == 0
        and "culprit" in r.stdout
        and "test_zz_debug.py::test_enable_debug" in r.stdout,
        f"rc={r.returncode} " + r.stdout[-300:],
    )
    cli = (REPO / "docs" / "reference" / "cli.md").read_text(encoding="utf-8")
    section = cli.split("### `--shuffle[=SEED]`", 1)[-1].split("\n### ", 1)[0]
    check(
        "MT-11 docs: --shuffle points to `rstest replay` for an exact repro",
        "replay" in section and "--dist loadfile` to keep the repro stable" not in section,
        section[:200].replace("\n", " "),
        known_bug=True,
    )


def gate_maintainer_flaky_policy(g, args, binary):
    print("== maintainer: flaky policy matrix (MT-12) ==")
    # Quarantined failure collects first, the real failure last, so -x /
    # --maxfail sees the quarantined one first.
    g.write("mt_policy/tests/test_0_quar.py", "def test_quarantined():\n    assert False\n")
    g.write(
        "mt_policy/tests/test_5_flaky.py",
        "import os, pathlib, pytest\n\n"
        "_flaky = pytest.mark.flaky(reruns=2) if os.environ.get('MT_MARK') else (lambda f: f)\n\n"
        "@_flaky\n"
        "def test_flaky_once():\n"
        "    marker = pathlib.Path(os.environ['MT_FLAKY_MARKER'])\n"
        "    if not marker.exists():\n"
        "        marker.write_text('attempted')\n"
        "        assert False, 'first attempt fails'\n",
    )
    g.write(
        "mt_policy/tests/test_9_real.py",
        "def test_always_fails():\n    assert False, 'real failure'\n",
    )
    for i in range(3):
        g.write(
            f"mt_policy/tests/test_ok{i}.py",
            "import pytest\n\n"
            "@pytest.mark.parametrize('i', range(10))\n"
            "def test_ok(i):\n    pass\n",
        )
    g.write("mt_policy/q.txt", "tests/test_0_quar.py::test_quarantined\n")
    cwd = g.tmp / "mt_policy"

    def run(n, *extra, mark=False):
        rj = cwd / f"r-{uuid.uuid4().hex}.json"
        env = {"MT_FLAKY_MARKER": str(cwd / f"m-{uuid.uuid4().hex}")}
        if mark:
            env["MT_MARK"] = "1"
        r = _run(g, cwd, "-n", n, "-q", "--report-json", str(rj), *extra, env=env, timeout=60)
        return r, _counts(rj)

    q = ("--quarantine", "q.txt")
    # Healthy baseline: --reruns + quarantine, all 33 tests accounted for.
    for n in ("0", "2"):
        r, c = run(n, "--reruns", "2", *q)
        check(
            f"MT-12 -n {n} --reruns 2 --quarantine: 1 failed/flaky/quarantined",
            r.returncode == 1
            and (c.get("failed"), c.get("flaky"), c.get("quarantined"), c.get("passed"))
            == (1, 1, 1, 30),
            f"rc={r.returncode} counts={c}",
        )

    # @flaky mark without --reruns (KNOWN_BUGS #1), real failure deselected.
    for n, bug in (("2", False), ("0", True)):
        r, c = run(n, *q, "-k", "not always", mark=True)
        check(
            f"MT-12 -n {n} @flaky without --reruns: exit 0, counted once as 1 flaky",
            r.returncode == 0 and (c.get("flaky"), c.get("passed"), c.get("failed")) == (1, 30, 0),
            f"rc={r.returncode} counts={c}",
            known_bug=bug,
        )

    # -x / --maxfail with reruns (A1): the flaky first attempt must not stop
    # the run; the always-failing test still fails it.
    for extra in (("-x", "--reruns", "1"), ("--maxfail", "1", "--reruns", "2")):
        r, c = run("2", *extra, "-k", "not quarantined")
        check(
            f"MT-12 -n 2 {' '.join(extra)}: always-failing test still exits 1",
            r.returncode == 1 and c.get("failed", 0) >= 1,
            f"rc={r.returncode} counts={c} " + r.stdout[-120:],
            known_bug=True,
        )

    # -x with a quarantined failure first (A2): it must not count toward -x;
    # the real failure later still runs and fails the run.
    for n in ("2", "0"):
        r, c = run(n, "-x", *q, "-k", "not flaky")
        check(
            f"MT-12 -n {n} -x --quarantine: quarantined failure does not trip -x, exit 1",
            r.returncode == 1 and c.get("failed") == 1,
            f"rc={r.returncode} counts={c}",
            known_bug=True,
        )
