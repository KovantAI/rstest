"""e2e gate sections: serve_watch."""

import json
import os
import subprocess
import time

from _harness import REPO, check


def gate_watch_mode(g, args, binary):
    print("== watch mode ==")
    wd = g.tmp / "watch"
    wd.mkdir(exist_ok=True)
    (wd / "helper.py").write_text("VALUE = 1\n")
    (wd / "test_w.py").write_text("from helper import VALUE\n\ndef test_one(): assert VALUE >= 1\n")
    (wd / "test_other.py").write_text("def test_other(): assert True\n")
    env = dict(
        os.environ,
        VIRTUAL_ENV=str(g.venv),
        RSTEST_WORKER_PATH=str(REPO / "python"),
    )
    proc = subprocess.Popen(
        [str(binary), "--watch", "-n", "2"],
        cwd=str(wd),
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )

    import queue
    import threading

    lines: queue.Queue[str] = queue.Queue()

    def _pump():
        assert proc.stdout is not None
        for line in proc.stdout:
            lines.put(line)

    threading.Thread(target=_pump, daemon=True).start()

    def wait_for(needle, timeout=30):
        # Never block on readline: a wrong expectation must time out,
        # not hang the gate.
        buf = []
        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                line = lines.get(timeout=0.25)
            except queue.Empty:
                continue
            buf.append(line)
            if needle in line:
                return True, "".join(buf)
        return False, "".join(buf)

    try:
        ok1, _ = wait_for("waiting for changes")
        check("watch initial run", ok1)
        time.sleep(0.5)
        with open(wd / "test_w.py", "a") as f:
            f.write("\ndef test_two(): assert True\n")
        ok2, out2 = wait_for("rerunning changed files")
        ok3, out3 = wait_for("passed")
        check("watch targeted rerun", ok2 and ok3, (out2 + out3)[-200:])
        ok4, _ = wait_for("waiting for changes")
        time.sleep(0.5)
        (wd / "helper.py").write_text("VALUE = 2\n")
        ok5, out5 = wait_for("rerunning affected tests")
        ok6, out6 = wait_for("2 passed")  # test_w.py holds 2 tests by now
        check(
            "watch source change -> affected tests only",
            ok4 and ok5 and ok6,
            (out5 + out6)[-300:],
        )
    finally:
        proc.kill()


def gate_try(g, args, binary):
    print("== try (pytest-vs-rstest parity proof) ==")
    # A clean all-pass suite: pytest and rstest -n auto agree, so `try` reports
    # identical parity and exits 0. Exercises run_try end-to-end (pytest baseline
    # + rstest run + parity/speed diff). pytest is available via the pytest-cov
    # dep in the gate venv.
    g.write(
        "tryfix/test_t.py",
        "def test_a(): assert True\ndef test_b(): assert True\ndef test_c(): assert True\n",
    )
    r = g.run("try", cwd=g.tmp / "tryfix")
    check(
        "try: identical parity + speed line + drop-in verdict, exit 0",
        r.returncode == 0
        and "rstest try" in r.stdout
        and "identical outcomes to pytest" in r.stdout
        and "at -n auto" in r.stdout
        and "drop-in ready" in r.stdout,
        f"rc={r.returncode} " + r.stdout[-400:] + r.stderr[-200:],
    )


def gate_migrate_check(g, args, binary):
    print("== migrate-check (parallel-readiness preflight) ==")
    # Clean suite: stable ids across two collections, no parallel-only failures
    # -> ready at -n auto, exit 0. Drives the full preflight: collect-twice +
    # the -n auto parallel phase + failure classification (with zero failures).
    g.write(
        "mcclean/test_ok.py",
        "def test_a(): assert True\ndef test_b(): assert True\n"
        "def test_c(): assert True\ndef test_d(): assert True\n",
    )
    r = g.run("migrate-check", cwd=g.tmp / "mcclean")
    check(
        "migrate-check: clean suite is parallel-ready (exit 0)",
        r.returncode == 0
        and "tests collected" in r.stdout
        and "UNSTABLE NODEIDS: none" in r.stdout
        and "PARALLEL: ready" in r.stdout,
        f"rc={r.returncode} " + r.stdout[-500:] + r.stderr[-200:],
    )
    # Unstable parametrize ids: fresh uuid4 per collection -> the two collections
    # disagree, the classifier tags them `uuid` (a per-process-unstable kind that
    # forces -n 0). Blocks the run (exit 1) before the parallel phase.
    g.write(
        "mcunstable/test_u.py",
        "import uuid\nimport pytest\n\n"
        # Dashed uuid form so the classifier's uuid regex matches (a WILL-bail
        # kind); .hex (undashed) would fall through to the may-bail "other".
        "@pytest.mark.parametrize('x', [str(uuid.uuid4()), str(uuid.uuid4())])\n"
        "def test_u(x):\n    assert True\n",
    )
    jpath = g.tmp / "mc.json"
    r = g.run("migrate-check", "--migrate-check-json", str(jpath), cwd=g.tmp / "mcunstable")
    check(
        "migrate-check: unstable uuid ids force -n 0 (exit 1)",
        r.returncode == 1 and "UNSTABLE NODEIDS:" in r.stdout and "force -n 0" in r.stdout,
        f"rc={r.returncode} " + r.stdout[-500:] + r.stderr[-200:],
    )
    doc = json.loads(jpath.read_text(encoding="utf-8"))
    check(
        "migrate-check-json: versioned findings doc marks not-ready",
        doc["ready"] is False and doc["will_bail_count"] >= 1 and bool(doc["unstable_ids"]),
        str(doc)[:300],
    )
