"""e2e gate sections: serve_watch."""

import contextlib
import json
import os
import subprocess
import time

from _harness import REPO, WINDOWS, check, venv_bin

_SERVE_CLIENT = r"""
import json, socket, sys, time, msgpack
sock = sys.argv[1]
c = socket.socket(socket.AF_UNIX)
for _ in range(80):
    try:
        c.connect(sock); break
    except OSError:
        time.sleep(0.1)
else:
    print(json.dumps({"error": "connect"})); sys.exit(0)
c.settimeout(30)
up = msgpack.Unpacker(raw=False)
def send(k, p): c.sendall(msgpack.packb({"kind": k, "payload": p}))
def recv():
    while True:
        for o in up:
            return o
        b = c.recv(65536)
        if not b:
            return None
        up.feed(b)
def run(rid, node_ids, patch=None):
    p = {"id": rid, "node_ids": node_ids}
    if patch is not None:
        p["patch"] = {"files": patch}
    send("run", p)
    reports = []
    while True:
        m = recv()
        if m["kind"] == "report":
            reports.append(m["payload"]["report"])
        elif m["kind"] == "run_done":
            return m["payload"], {r["nodeid"] + "|" + r["when"]: r["outcome"]
                                  for r in reports if r["when"] == "call"}
send("hello", {"proto": 1}); welcome = recv()
send("open_session", {"args": ["test_s.py"]}); ready = recv()
done, outcomes = run(7, ["test_s.py::test_a", "test_s.py::test_b"])
# Isolation: mutate mod.py so test_m fails (killed), then run clean again — the
# overlay must not leak (killed False, and mod.py restored on disk).
clean1, _ = run(10, ["test_m.py::test_m"])
mutated, _ = run(11, ["test_m.py::test_m"], {"mod.py": "def val():\n    return 999\n"})
clean2, _ = run(12, ["test_m.py::test_m"])
# Import-breaking mutant: mod.py no longer parses, so collecting test_m errors.
# Spec: killed = failed OR errored, so this must report killed (ran 0), then
# revert so the next run is clean again.
broke, _ = run(13, ["test_m.py::test_m"], {"mod.py": "def val(:\n"})
clean3, _ = run(14, ["test_m.py::test_m"])
send("shutdown", {}); bye = recv()
print(json.dumps({
    "welcome": welcome["kind"], "collected": ready["payload"].get("collected"),
    "done": done, "bye": bye["kind"], "outcomes": outcomes,
    "iso": {"clean1": clean1["killed"], "mutated": mutated["killed"], "clean2": clean2["killed"]},
    "broke": {"killed": broke["killed"], "ran": broke["ran"], "clean3": clean3["killed"]},
}))
"""


def gate_serve(g, args, binary):
    print("== serve daemon (--serve) ==")
    if WINDOWS:
        # --serve is a Unix-domain-socket daemon (std::os::unix::net); the binary
        # bails "--serve is only supported on Unix" and Windows Python has no
        # socket.AF_UNIX. Nothing to exercise here.
        print("  skip  serve: Unix-only feature")
        return
    sp = g.tmp / "serveproj"
    g.write(
        "serveproj/test_s.py",
        "def test_a(): assert True\ndef test_b(): assert 1 == 2\ndef test_c(): assert True\n",
    )
    # For the isolation check: a SUT module + a test asserting its value.
    g.write("serveproj/mod.py", "def val():\n    return 1\n")
    g.write("serveproj/test_m.py", "import mod\ndef test_m():\n    assert mod.val() == 1\n")
    g.write("serveproj/client.py", _SERVE_CLIENT)
    # Unix socket paths are length-limited (~104 chars); g.tmp is long, use /tmp.
    sock = f"/tmp/rstest-gate-serve-{os.getpid()}.sock"
    with contextlib.suppress(OSError):
        os.unlink(sock)
    env = dict(os.environ, VIRTUAL_ENV=str(g.venv), RSTEST_WORKER_PATH=str(REPO / "python"))
    proc = subprocess.Popen(
        [str(binary), "--serve", sock, "test_s.py"],
        cwd=str(sp),
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        # The client runs under the gate venv (has msgpack), not the launcher.
        r = subprocess.run(
            [str(venv_bin(g.venv, "python")), "client.py", sock],
            cwd=str(sp),
            capture_output=True,
            text=True,
            timeout=90,
        )
        try:
            out = json.loads(r.stdout.strip().splitlines()[-1])
        except (ValueError, IndexError):
            check("serve: client ran the protocol", False, r.stdout[-300:] + r.stderr[-300:])
            return
        done = out.get("done") or {}
        check(
            "serve: hello + open_session warms a 3-test session",
            out.get("welcome") == "welcome" and out.get("collected") == 3,
            str(out),
        )
        check(
            "serve: run streams the requested subset, test_c not run",
            done.get("id") == 7
            and done.get("ran") == 2
            and out["outcomes"].get("test_s.py::test_a|call") == "passed"
            and out["outcomes"].get("test_s.py::test_b|call") == "failed"
            and "test_s.py::test_c|call" not in out["outcomes"],
            str(out),
        )
        check(
            "serve: a failing test marks the run killed + shutdown replies bye",
            done.get("killed") is True and out.get("bye") == "bye",
            str(out),
        )
        iso = out.get("iso") or {}
        check(
            "serve: overlay mutation kills the test, and does NOT leak to the next run",
            iso.get("clean1") is False
            and iso.get("mutated") is True
            and iso.get("clean2") is False,
            str(iso),
        )
        broke = out.get("broke") or {}
        check(
            "serve: an import-breaking mutant is killed (errored counts) and reverts",
            broke.get("killed") is True and broke.get("ran") == 0 and broke.get("clean3") is False,
            str(broke),
        )
        check(
            "serve: the overlay is reverted on disk after the run",
            (sp / "mod.py").read_text() == "def val():\n    return 1\n",
            (sp / "mod.py").read_text(),
        )
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
        with contextlib.suppress(OSError):
            os.unlink(sock)


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
    print("== --try (pytest-vs-rstest parity proof) ==")
    # A clean all-pass suite: pytest and rstest -n auto agree, so --try reports
    # identical parity and exits 0. Exercises run_try end-to-end (pytest baseline
    # + rstest run + parity/speed diff). pytest is available via the pytest-cov
    # dep in the gate venv.
    g.write(
        "tryfix/test_t.py",
        "def test_a(): assert True\ndef test_b(): assert True\ndef test_c(): assert True\n",
    )
    r = g.run("--try", cwd=g.tmp / "tryfix")
    check(
        "try: identical parity + speed line + drop-in verdict, exit 0",
        r.returncode == 0
        and "rstest --try" in r.stdout
        and "identical outcomes to pytest" in r.stdout
        and "at -n auto" in r.stdout
        and "drop-in ready" in r.stdout,
        f"rc={r.returncode} " + r.stdout[-400:] + r.stderr[-200:],
    )


def gate_migrate_check(g, args, binary):
    print("== --migrate-check (parallel-readiness preflight) ==")
    # Clean suite: stable ids across two collections, no parallel-only failures
    # -> ready at -n auto, exit 0. Drives the full preflight: collect-twice +
    # the -n auto parallel phase + failure classification (with zero failures).
    g.write(
        "mcclean/test_ok.py",
        "def test_a(): assert True\ndef test_b(): assert True\n"
        "def test_c(): assert True\ndef test_d(): assert True\n",
    )
    r = g.run("--migrate-check", cwd=g.tmp / "mcclean")
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
    r = g.run("--migrate-check", "--migrate-check-json", str(jpath), cwd=g.tmp / "mcunstable")
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
