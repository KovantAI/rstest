#!/usr/bin/env python3
"""rstest test gate: end-to-end assertions for every shipped behavior.

Hermetic: builds its own venv (worker runtime deps) and fixture suites.
Usage: python3 tests/gate.py [--binary target/release/rstest]

Exit 0 = all gates green. Designed to be the single CI entry point.
"""

import argparse
import glob
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import xml.etree.ElementTree as ET
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
E2E = REPO / "tests" / "e2e"

WINDOWS = os.name == "nt"


def clear_hook_log(base: Path):
    # Conftest _log writes per-process files (base.<pid>): cross-process
    # appends to a single file are not atomic on Windows and drop lines.
    for p in glob.glob(str(base) + ".*"):
        os.unlink(p)


def read_hook_log(base: Path) -> str:
    parts = [Path(p).read_text(encoding="utf-8") for p in glob.glob(str(base) + ".*")]
    return "".join(parts)


def read_e2e_rows(base: Path) -> list:
    # Workers each write base.<worker>; gather and parse all lines.
    rows = []
    for p in glob.glob(str(base) + ".*"):
        for line in Path(p).read_text(encoding="utf-8").splitlines():
            if line.strip():
                rows.append(json.loads(line))
    return rows


def clear_e2e_log(base: Path):
    for p in glob.glob(str(base) + ".*"):
        Path(p).unlink()


def venv_bin(venv_dir: Path, name: str) -> Path:
    # venv layout differs: POSIX uses bin/, Windows uses Scripts/ + .exe.
    if WINDOWS:
        return venv_dir / "Scripts" / (name + ".exe")
    return venv_dir / "bin" / name

PASS = 0
FAIL = []


def check(name, cond, detail=""):
    global PASS
    if cond:
        PASS += 1
        print(f"  ok    {name}")
    else:
        FAIL.append(name)
        print(f"  FAIL  {name}  {detail}")


class Gate:
    def __init__(self, binary: Path, venv_dir: Path):
        self.binary = binary
        self.venv = venv_dir
        self.tmp = Path(tempfile.mkdtemp(prefix="rstest-gate-"))

    def run(self, *args, cwd=None, env_extra=None, timeout=120):
        env = dict(
            os.environ,
            VIRTUAL_ENV=str(self.venv),
            RSTEST_WORKER_PATH=str(REPO / "python"),
        )
        env.pop("PYTEST_ADDOPTS", None)
        # Doctor runs auto-publish to the CI job summary (GitHub step
        # summary / Buildkite annotation); keep the gate's fixture-suite
        # reports off the real run page.
        env.pop("GITHUB_STEP_SUMMARY", None)
        env.pop("BUILDKITE", None)
        # Bare --changed auto-targets the PR/MR base when a CI exposes it;
        # the gate's fixture repos have no origin, so a real PR CI run would
        # break every --changed check. Tests opt in via env_extra.
        for k in (
            "GITHUB_BASE_REF",
            "CI_MERGE_REQUEST_DIFF_BASE_SHA",
            "CI_MERGE_REQUEST_TARGET_BRANCH_NAME",
            "BUILDKITE_PULL_REQUEST_BASE_BRANCH",
        ):
            env.pop(k, None)
        if env_extra:
            env.update(env_extra)
        return subprocess.run(
            [str(self.binary), *args],
            cwd=cwd or str(self.tmp),
            env=env,
            capture_output=True,
            text=True,
            # rstest emits UTF-8 (✓ ✗ ─ etc). Without this, text=True decodes
            # with the locale encoding — cp1252 on Windows — mangling those
            # glyphs in the in-memory string (bytes still round-trip, so CI
            # logs look fine, but `"✓" in r.stdout` is False). Pin UTF-8.
            encoding="utf-8",
            timeout=timeout,
        )

    def write(self, relpath, content):
        p = self.tmp / relpath
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(content, encoding="utf-8")
        return p


def find_python() -> str:
    # Vendored core requires >=3.10 (pyproject requires-python).
    if sys.version_info >= (3, 10):
        return sys.executable
    for name in ("python3.13", "python3.12", "python3.11", "python3.10"):
        p = shutil.which(name)
        if p:
            return p
    sys.exit("gate needs python >= 3.10 on PATH")


# Worker runtime deps shared by every gate venv.
BASE_DEPS = [
    "msgpack",
    "pluggy>=1.5",
    "iniconfig",
    "packaging",
    "pygments",
    "coverage",
    "pytest-cov",
]


def make_venv(venv_dir: Path, extra_deps=None):
    if venv_bin(venv_dir, "python").exists():
        return
    py = find_python()
    print(f"creating gate venv at {venv_dir} (from {py})")
    subprocess.run([py, "-m", "venv", str(venv_dir)], check=True)
    subprocess.run(
        [str(venv_bin(venv_dir, "pip")), "install", "-q", *BASE_DEPS, *(extra_deps or [])],
        check=True,
    )


def parse_ndjson(text):
    """Parse stdout as newline-delimited JSON. Returns (all_lines_valid,
    [objects]). A single embedded raw newline would split an object and
    fail json.loads — exactly the regression this guards against."""
    objs, ok = [], True
    for ln in text.splitlines():
        if not ln.strip():
            continue
        try:
            objs.append(json.loads(ln))
        except Exception:
            ok = False
    return ok, objs


def main():
    ap = argparse.ArgumentParser()
    default_binary = REPO / "target" / "release" / ("rstest.exe" if WINDOWS else "rstest")
    ap.add_argument("--binary", default=str(default_binary))
    ap.add_argument("--venv", default=str(REPO / ".gate-venv"))
    args = ap.parse_args()

    binary = Path(args.binary).resolve()
    assert binary.exists(), f"binary missing: {binary} (cargo build --release first)"
    make_venv(Path(args.venv))
    g = Gate(binary, Path(args.venv).resolve())

    print("== basics ==")
    g.write("basic/test_basic.py", BASIC)
    r = g.run("basic/test_basic.py", "-n", "2")
    check("parallel counts", "2 failed, 2 passed" in r.stdout, r.stdout[-200:])
    check("parallel exit 1", r.returncode == 1)
    check("header line", r.stdout.startswith("rstest "), r.stdout[:80])
    r = g.run("basic/test_basic.py", "-n", "0", "-k", "passes")
    check("-n 0 exact mode + -k", "2 passed" in r.stdout and "pytest-exact" in r.stdout)
    r = g.run("basic/test_basic.py", "--co", "-q")
    check("--co passthrough", "test_basic.py::test_passes" in r.stdout)
    r = g.run("basic/test_basic.py", "-n", "2", "-v")
    check("-v verbose lines", "::test_passes PASSED" in r.stdout, r.stdout[-300:])
    check("-v worker attribution", "[gw" in r.stdout, r.stdout[-300:])
    check("failure header attribution", "FAILED [gw" in r.stdout, r.stdout[-300:])
    r = g.run("basic/test_basic.py", "-n", "0", "-v")
    check("-n 0 has no worker prefixes", "[gw" not in r.stdout, r.stdout[-200:])
    g.write("empty/.keep", "")
    r = g.run("empty")
    check("no tests exit 5", r.returncode in (4, 5), f"rc={r.returncode}")

    print("== collection error semantics ==")
    g.write("broken/test_broken.py", "import nonexistent_module_xyz\n")
    r = g.run("broken/test_broken.py", "-n", "0")
    check("collect error aborts (exit 2)", r.returncode == 2, f"rc={r.returncode}")
    g.write("broken/test_fine.py", "def test_ok(): assert True\n")
    r = g.run(".", "-n", "2", cwd=g.tmp / "broken")
    check(
        "pool: collect error aborts, runs nothing",
        r.returncode == 2 and " passed" not in r.stdout,
        f"rc={r.returncode} " + r.stdout[-200:],
    )

    print("== output styles ==")
    # --output bar (pytest-sugar-style): per-test lines + inline failures.
    # Non-tty here, so the live footer self-disables; the per-test lines and
    # summary must still appear, and failures must NOT be double-printed.
    r = g.run("basic/test_basic.py", "-n", "2", "--output", "bar", "--color=yes")
    bar_ok = (
        "✓" in r.stdout  # ✓ pass
        and "✗" in r.stdout  # ✗ fail
        and "test_basic.py::test_passes" in r.stdout
        and "AssertionError" in r.stdout  # failure repr shown inline
        and "2 failed" in r.stdout
        and "2 passed" in r.stdout
    )
    # inline only — the batched "--- FAILED ---" block must NOT also print
    no_dup = r.stdout.count("--- FAILED") == 0
    check("output bar: per-test lines + summary, failures once", bar_ok and no_dup, r.stdout[-400:])
    # unknown style warns, falls back, still runs
    r = g.run("basic/test_basic.py", "-n", "0", "--output", "nope")
    check(
        "output: unknown style falls back to dots",
        "unknown --output" in r.stderr and "2 passed" in r.stdout,
        r.stderr[-160:] + " || " + r.stdout[-160:],
    )
    # --output github: the normal human log PLUS a ::error workflow command
    # per failing test (GitHub renders them as inline PR annotations).
    r = g.run("basic/test_basic.py", "-n", "2", "--output", "github")
    ann = [ln for ln in r.stdout.splitlines() if ln.startswith("::error ")]
    gh_ok = (
        "2 passed" in r.stdout
        and "2 failed" in r.stdout  # human summary intact
        and len(ann) == 2  # one per failed test (deduped by nodeid)
        and all("file=" in a and "title=" in a and "line=" in a for a in ann)
        # title carries the nodeid; its `::` is percent-escaped (%3A%3A), so
        # match the file path the title embeds instead.
        and all("test_basic.py" in a for a in ann)
    )
    check("output github: human log + ::error per failure", gh_ok, r.stdout[-400:])

    # --output json: stdout is PURE NDJSON — no banner, every line parses,
    # closed by exactly one sessionfinish envelope. The machine-readable
    # inverse of the bar CI-stability rule: consumers must see no human chrome.
    r = g.run("basic/test_basic.py", "-n", "2", "--output", "json")
    ok, objs = parse_ndjson(r.stdout)
    reports = [o for o in objs if o.get("event") == "testreport"]
    finishes = [o for o in objs if o.get("event") == "sessionfinish"]
    json_ok = (
        ok
        and not r.stdout.startswith("rstest ")  # banner suppressed
        and "✓" not in r.stdout
        and "passed in" not in r.stdout  # no bar/summary chrome
        and reports
        and any(o["when"] == "call" and o["outcome"] == "failed" for o in reports)
        and any("worker" in o for o in reports)  # pool run → gwN tagged
        and len(finishes) == 1
        and finishes[0]["counts"]["failed"] == 2
        and finishes[0]["counts"]["passed"] == 2
        and finishes[0]["exitstatus"] == 1
    )
    check("output json: pure NDJSON + sessionfinish envelope", json_ok, r.stdout[-400:])

    # json + --doctor: the doctor's human report must NOT corrupt the stream.
    r = g.run("basic/test_basic.py", "-n", "2", "--output", "json", "--doctor")
    ok, _ = parse_ndjson(r.stdout)
    check(
        "output json + --doctor: stream stays pure",
        ok and "===" not in r.stdout and "wait-bound" not in r.stdout,
        r.stdout[-300:],
    )

    # --output tap: pure TAP stream — version header, one point per test,
    # failure text as `#` diagnostics, trailing plan matching the count.
    r = g.run("basic/test_basic.py", "-n", "2", "--output", "tap")
    lines = [ln for ln in r.stdout.splitlines() if ln]
    oks = [ln for ln in lines if ln.startswith("ok ")]
    notoks = [ln for ln in lines if ln.startswith("not ok ")]
    tap_ok = (
        lines
        and lines[0] == "TAP version 13"
        and len(oks) == 2
        and len(notoks) == 2
        and lines[-1] == "1..4"
        and any(ln.startswith("# ") for ln in lines)  # failure diagnostics
        and "passed in" not in r.stdout  # no human chrome
    )
    check("output tap: pure stream + trailing plan", tap_ok, r.stdout[-400:])

    # --output teamcity: a service-message group per test; failures carry
    # escaped details. Human summary stays (TeamCity ignores plain lines).
    r = g.run("basic/test_basic.py", "-n", "2", "--output", "teamcity")
    tc = [ln for ln in r.stdout.splitlines() if ln.startswith("##teamcity[")]
    tc_ok = (
        sum("testStarted" in ln for ln in tc) == 4
        and sum("testFinished" in ln for ln in tc) == 4
        and sum("testFailed" in ln for ln in tc) == 2
        and any("|n" in ln for ln in tc if "testFailed" in ln)  # escaping
        and "2 passed" in r.stdout
    )
    check("output teamcity: service messages + summary", tc_ok, r.stdout[-400:])

    # --output gitlab: dots log; each failure folded in a collapsed section.
    r = g.run("basic/test_basic.py", "-n", "2", "--output", "gitlab")
    gl_ok = (
        r.stdout.count("section_start:") == 2
        and r.stdout.count("section_end:") == 2
        and "[collapsed=true]" in r.stdout
        and "2 passed" in r.stdout
    )
    check("output gitlab: failures in collapsed sections", gl_ok, r.stdout[-400:])

    # --output buildkite: each failure under an auto-expanded +++ group.
    r = g.run("basic/test_basic.py", "-n", "2", "--output", "buildkite")
    bk = [ln for ln in r.stdout.splitlines() if ln.startswith("+++ ")]
    check(
        "output buildkite: failures under +++ groups",
        len(bk) == 2 and "2 passed" in r.stdout,
        r.stdout[-400:],
    )

    print("== multiprocessing-spawn children ==")
    # spawn-mode children re-import the worker's __main__ as __mp_main__
    # (runpy, no package context): the worker entry must import absolutely,
    # guard main(), and keep its sys.path bootstrap idempotent. anyio's
    # to_process exercises the same protocol.
    g.write("mpspawn/test_mpspawn.py", MP_SPAWN)
    r = g.run("mpspawn", "-n", "2")
    check("mp-spawn under pool", "2 passed" in r.stdout, r.stdout[-300:])

    print("== crash handling ==")
    g.write("crash/test_crash.py", CRASH)
    r = g.run("crash", "-n", "2")
    check("crash run completes", "1 failed, 5 passed" in r.stdout, r.stdout[-200:])
    check("crash attributed", "test_killer" in r.stdout and "crashed while running" in r.stdout)
    check("crash respawn notice", "respawning" in r.stderr, r.stderr[-200:])
    check("crash exit 1", r.returncode == 1)
    g.write("crashloop/test_loop.py", CRASHLOOP)
    r = g.run("crashloop", "-n", "2", timeout=60)
    check("crash-loop terminates", r.returncode != 0 and "passed" in r.stdout, r.stdout[-200:])

    print("== report-json contract ==")
    rj = g.tmp / "contract.json"
    g.run("basic/test_basic.py", "-n", "2", "--report-json", str(rj))
    doc = json.loads(rj.read_text(encoding="utf-8"))
    check("report-json schema version", doc["meta"].get("schema") == 5, str(doc["meta"])[:200])
    # schema 4: per-test source line (0-based, pytest report.location). BASIC
    # has a leading newline, so `test_passes` def sits on 0-based line 1.
    lines = {k.split("::")[-1]: v.get("lineno") for k, v in doc["tests"].items()}
    check(
        "report-json carries 0-based source lineno",
        lines.get("test_passes") == 1
        and all(isinstance(n, int) and n >= 0 for n in lines.values())
        and lines["test_passes"] < lines["test_fails"] < lines["test_also_passes"],
        str(lines),
    )
    check(
        "report-json envelope counts match outcomes",
        doc["meta"]["counts"]["passed"] == 2
        and doc["meta"]["counts"]["failed"] == 2
        and doc["meta"]["counts"]["errors"] == 0
        and doc["meta"]["workers"] == 2
        and doc["meta"]["duration_seconds"] > 0
        and doc["meta"]["started_at_epoch"] > 1_700_000_000,
        str(doc["meta"])[:300],
    )
    failed = [v for v in doc["tests"].values() if v.get("call") == "failed"]
    check(
        "report-json carries failure text",
        failed and all("longrepr" in v and v["longrepr"] for v in failed),
        str(failed[:1]),
    )
    rj2 = g.tmp / "contract_crash.json"
    g.run("crash", "-n", "2", "--report-json", str(rj2))
    doc2 = json.loads(rj2.read_text(encoding="utf-8"))
    crashed = [k for k, v in doc2["tests"].items() if v.get("crashed")]
    check(
        "report-json marks crash-fabricated outcomes",
        len(crashed) == 1 and "test_killer" in crashed[0],
        str(crashed),
    )

    print("== collect-only discovery json ==")
    g.write("disco/test_disco.py", DISCO)
    dj = g.tmp / "disco.json"
    g.run("disco", "--collect-only", "--report-json", str(dj))
    ddoc = json.loads(dj.read_text(encoding="utf-8"))
    byid = {t["nodeid"].split("::")[-1]: t for t in ddoc["tests"]}
    check(
        "discovery: kind + schema + count",
        ddoc["meta"]["kind"] == "discovery"
        and ddoc["meta"]["schema"] == 1
        and ddoc["meta"]["count"] == len(ddoc["tests"]) == 5,
        str(ddoc["meta"]),
    )
    check(
        "discovery: abs file + 0-based lineno in source order",
        all(os.path.isabs(t["file"]) and t["file"].endswith("test_disco.py") for t in ddoc["tests"])
        and all(isinstance(t["lineno"], int) and t["lineno"] >= 0 for t in ddoc["tests"])
        and byid["test_one"]["lineno"] < byid["test_two"]["lineno"] < byid["test_ser"]["lineno"],
        str(byid.get("test_one")),
    )
    check(
        "discovery: all marker names surfaced (sorted, deduped)",
        byid["test_one"]["markers"] == []
        and byid["test_ser"]["markers"] == ["serial"]
        and "test_p[1]" in byid
        and "test_p[2]" in byid
        and byid["test_p[1]"]["markers"] == ["parametrize"],
        str({k: v["markers"] for k, v in byid.items()}),
    )
    g.write(
        "xdistenv/test_env.py",
        "import os\n"
        "def test_env():\n"
        "    assert os.environ['PYTEST_XDIST_WORKER'].startswith('gw')\n"
        "    assert os.environ['PYTEST_XDIST_WORKER_COUNT'] == '2'\n"
        "def test_uid(request):\n"
        "    assert request.config.workerinput['testrun_uid']\n"
        # pytest-randomly reads this master-injected key; rstest derives a
        # single run-level seed from the shared run uid so every worker agrees.
        "def test_randomly_seed(request):\n"
        "    wi = request.config.workerinput\n"
        "    assert wi['randomly_seed'] == (int(wi['testrun_uid'], 16) & 0xFFFFFFFF)\n",
    )
    r = g.run("xdistenv", "-n", "2")
    check("PYTEST_XDIST_WORKER + testrun_uid", "3 passed" in r.stdout, r.stdout[-300:])

    print("== pytest-randomly (real plugin) ==")
    # Isolated venv: pytest-randomly shuffles collection order, so installing
    # it in the shared venv would break the source-order / lazy-order checks.
    # Here we prove the real plugin consumes rstest's synthesized workerinput
    # seed at -n >= 2 instead of KeyError-ing (the bug this PR fixes).
    rnd_venv = Path(args.venv + "-randomly").resolve()
    make_venv(rnd_venv, extra_deps=["pytest-randomly"])
    gr = Gate(binary, rnd_venv)
    gr.write(
        "rnd/test_rnd.py",
        "def test_a(): pass\n"
        "def test_b(): pass\n"
        "def test_c(): pass\n"
        # pytest-randomly resolves --randomly-seed from workerinput per worker.
        # A missing key -> KeyError at configure (no pass); a plugin that
        # ignored our key -> resolved seed != our derivation. Asserting the
        # plugin's *resolved* option (not just the raw workerinput value)
        # proves it actually consumed the key we synthesize.
        "def test_seed_consumed(request):\n"
        "    resolved = request.config.getoption('randomly_seed')\n"
        "    wi = request.config.workerinput\n"
        "    assert resolved == (int(wi['testrun_uid'], 16) & 0xFFFFFFFF)\n",
    )
    # Pin the run uid so a second invocation derives the same seed: same uid
    # -> same seed -> same shuffle. Reproducibility, end to end through the
    # real plugin.
    uid = "abc123def456789"
    r = gr.run("rnd", "-n", "2", env_extra={"RSTEST_RUN_UID": uid})
    check("randomly: consumes synthesized seed, no crash at -n 2", "4 passed" in r.stdout, r.stdout[-400:])
    r = gr.run("rnd", "-n", "2", env_extra={"RSTEST_RUN_UID": uid})
    check("randomly: reproducible seed with pinned uid", "4 passed" in r.stdout, r.stdout[-400:])

    print("== pytest-rerunfailures + xdist (no sock_port KeyError) ==")
    # Isolated venv: pytest-rerunfailures with pytest-xdist co-installed takes
    # its xdist *client* branch under the pool (every worker has a workerinput)
    # and reads workerinput["sock_port"] — a key only an xdist master sets.
    # rstest runs no master, so this KeyError'd at configure for -n >= 2 (a
    # KeyError raised via the historic pytest_configure call, before any
    # configure-time unregister could help). rstest now drops the plugin in
    # pytest_cmdline_main, before that call, and owns reruns natively.
    # xdist is required in the venv: without it rerunfailures takes a no-op
    # branch and never reads sock_port, so it would not reproduce the bug.
    rf_venv = Path(args.venv + "-rerunfailures").resolve()
    make_venv(rf_venv, extra_deps=["pytest-rerunfailures", "pytest-xdist"])
    grf = Gate(binary, rf_venv)
    grf.write(
        "rf/test_rf.py",
        "import pytest\n"
        "_calls = {}\n"
        "@pytest.mark.flaky(reruns=2)\n"
        "def test_recovers():\n"
        "    n = _calls.get('x', 0) + 1\n"
        "    _calls['x'] = n\n"
        "    assert n > 1\n"
        "def test_a(): assert True\n"
        "def test_b(): assert True\n",
    )
    r = grf.run("rf", "-n", "2", "--reruns", "2")
    check(
        "rerunfailures+xdist: no sock_port KeyError at -n 2",
        "sock_port" not in (r.stdout + r.stderr),
        (r.stdout + r.stderr)[-500:],
    )
    check(
        "rerunfailures+xdist: session completes, flaky recovered natively",
        r.returncode == 0 and "passed" in r.stdout and "failed" not in r.stdout,
        f"rc={r.returncode} " + (r.stdout + r.stderr)[-400:],
    )
    # -n 0: no RSTEST_WORKER_ID, so the plugin is NOT neutralized and keeps its
    # native single-process behavior (no sock_port branch, no crash).
    r = grf.run("rf", "-n", "0")
    check(
        "rerunfailures: -n 0 keeps native plugin, no crash",
        r.returncode == 0 and "sock_port" not in (r.stdout + r.stderr),
        f"rc={r.returncode} " + (r.stdout + r.stderr)[-400:],
    )

    print("== pytest-retry + xdist (server_port self-provision) ==")
    # pytest-retry gates master vs. worker on `has_plugin("xdist") and
    # numprocesses`. rstest keeps numprocesses visible under the pool (only
    # dist is forced off), so each worker takes the MASTER branch and
    # self-provisions its own ReportServer — it never reads the
    # controller-injected workerinput["server_port"] that has no source under
    # rstest. Without that (e.g. if numprocesses were nulled) the plugin would
    # take its client branch and KeyError at configure. xdist must be in the
    # venv for the master branch to be reachable at all.
    rt_venv = Path(args.venv + "-retry").resolve()
    make_venv(rt_venv, extra_deps=["pytest-retry", "pytest-xdist"])
    grt = Gate(binary, rt_venv)
    grt.write(
        "rt/test_rt.py",
        "import pytest\n"
        "from pytest_retry import retry_plugin\n"
        "from pytest_retry.server import ReportServer, ClientReporter\n"
        "_a = {}\n"
        "def test_retry_recovers():\n"
        # Only pytest-retry's --retries can pass this: no rstest --reruns, no
        # @mark.flaky. Fails on attempt 1, passes on attempt 2.
        "    _a['x'] = _a.get('x', 0) + 1\n"
        "    assert _a['x'] >= 2\n"
        "def test_master_branch(request):\n"
        # Prove the plugin self-provisioned (master), not the server_port client
        # branch — the direct evidence that the KeyError path is never taken.
        "    rep = retry_plugin.retry_manager.reporter\n"
        "    assert isinstance(rep, ReportServer), type(rep).__name__\n"
        "    assert not isinstance(rep, ClientReporter)\n"
        "    assert request.config.getoption('numprocesses', False)\n"
        "def test_real_failure_survives_retries():\n"
        # Retry must not mask a genuine always-failure.
        "    assert False\n",
    )
    r = grt.run("rt", "-n", "2", "--retries", "2")
    check(
        "retry+xdist: no server_port KeyError at -n 2",
        "server_port" not in (r.stdout + r.stderr) and "KeyError" not in (r.stdout + r.stderr),
        (r.stdout + r.stderr)[-500:],
    )
    check(
        "retry+xdist: master branch taken, engine recovers, real failure survives",
        r.returncode == 1 and "1 failed, 2 passed" in r.stdout,
        f"rc={r.returncode} " + (r.stdout + r.stderr)[-400:],
    )

    print("== interpreter probe cache (heals after deps installed) ==")
    # Regression: a NEGATIVE probe (interpreter present, worker shim not
    # importable — e.g. msgpack missing) must NOT be persisted to the on-disk
    # probe cache. The cache keys on the binary's mtime, which a `pip install`
    # into site-packages leaves unchanged, so a cached `false` would survive the
    # very install that fixes it — "no usable Python interpreter found" forever.
    bare = g.tmp / "bareenv"
    shutil.rmtree(bare, ignore_errors=True)
    subprocess.run([find_python(), "-m", "venv", str(bare)], check=True)  # no deps -> no msgpack
    barepy = venv_bin(bare, "python")
    probe_cache = g.tmp / "probecache"
    shutil.rmtree(probe_cache, ignore_errors=True)
    g.write("probe/test_p.py", "def test_ok(): assert True\n")
    # 1) msgpack absent: the interpreter runs but can't host a worker, so the
    #    probe is negative and (with the fix) is not written to the cache.
    r = g.run("probe", "--python", str(barepy),
              env_extra={"RSTEST_CACHE_DIR": str(probe_cache)})
    check(
        "probe: msgpack-less interpreter rejected",
        r.returncode != 0 and ("worker shim" in r.stderr or "no usable" in r.stderr.lower()),
        f"rc={r.returncode} " + r.stderr[-200:],
    )
    # 2) install the worker deps into the SAME interpreter (binary mtime
    #    unchanged), then rerun with the SAME cache dir: a stale cached negative
    #    would still reject it; the fix re-probes negatives so it now succeeds.
    subprocess.run([str(venv_bin(bare, "pip")), "install", "-q", *BASE_DEPS], check=True)
    r = g.run("probe", "--python", str(barepy),
              env_extra={"RSTEST_CACHE_DIR": str(probe_cache)})
    check(
        "probe: heals after deps installed (negative not cached)",
        r.returncode == 0 and "1 passed" in r.stdout,
        f"rc={r.returncode} " + r.stderr[-200:] + " || " + r.stdout[-200:],
    )

    print("== lazy collection ==")
    # D5 single-point collection: same fixtures, same outcomes, no
    # initial collection pass in any worker.
    r = g.run("basic/test_basic.py", "-n", "2", "--collect", "lazy")
    check("lazy: parallel counts", "2 failed, 2 passed" in r.stdout, r.stdout[-200:])
    check("lazy: exit 1", r.returncode == 1)
    r = g.run("basic", "-n", "2", "--collect", "lazy", "-k", "passes")
    check("lazy: -k filters per file", "2 passed" in r.stdout and "failed" not in r.stdout, r.stdout[-200:])
    r = g.run("crash", "-n", "2", "--collect", "lazy")
    check("lazy: crash completes", "1 failed, 5 passed" in r.stdout, r.stdout[-200:])
    check("lazy: crash attributed", "crashed while running" in r.stdout, r.stdout[-300:])
    r = g.run(".", "-n", "2", "--collect", "lazy", cwd=g.tmp / "broken")
    check(
        "lazy: collect error aborts",
        r.returncode == 2,
        f"rc={r.returncode} " + r.stdout[-200:],
    )
    g.write("lazyfix/test_one.py", LAZY_SESSION_A)
    g.write("lazyfix/test_two.py", LAZY_SESSION_B)
    g.write("lazyfix/conftest.py", LAZY_CONFTEST)
    r = g.run("lazyfix", "-n", "2", "--collect", "lazy")
    check(
        "lazy: session fixture once per worker",
        "3 passed" in r.stdout and "failed" not in r.stdout,
        r.stdout[-300:],
    )
    r = g.run("empty", "--collect", "lazy", "-n", "2")
    check("lazy: no tests exit 5", r.returncode in (4, 5), f"rc={r.returncode}")
    fdir = g.tmp / "flaky"
    marker = g.tmp / "flaky_marker_lazy"
    if marker.exists():
        marker.unlink()
    g.write("flaky/test_flaky.py", FLAKY)
    r = g.run("test_flaky.py", "-n", "2", "--collect", "lazy", "--reruns", "2", cwd=fdir,
              env_extra={"FLAKY_MARKER": str(marker)})
    check(
        "lazy: flaky passes with reruns",
        r.returncode == 0 and "1 flaky" in r.stdout,
        r.stdout[-200:],
    )
    log = g.tmp / "serial_lazy.jsonl"
    clear_e2e_log(log)
    g.write("serial/test_serial.py", SERIAL)
    r = g.run("serial", "-n", "3", "--collect", "lazy", env_extra={"RSTEST_E2E_LOG": str(log)})
    check("lazy: serial run green", "8 passed" in r.stdout, r.stdout[-200:])
    rows = read_e2e_rows(log)
    lser = [x for x in rows if x["name"].startswith("serial")]
    overlap = any(
        s["start"] < o["end"] and o["start"] < s["end"]
        for s in lser
        for o in rows
        if o is not s
    )
    check(
        "lazy: serial exclusive",
        not overlap and len({s["worker"] for s in lser}) == 1,
    )

    print("== serial mark ==")
    g.write("serial/test_serial.py", SERIAL)
    log = g.tmp / "serial.jsonl"
    clear_e2e_log(log)
    r = g.run("serial", "-n", "3", env_extra={"RSTEST_E2E_LOG": str(log)})
    check("serial run green", "8 passed" in r.stdout, r.stdout[-200:])
    rows = read_e2e_rows(log)
    serial = [x for x in rows if x["name"].startswith("serial")]
    par = [x for x in rows if x["name"].startswith("par")]
    overlap = any(
        s["start"] < o["end"] and o["start"] < s["end"]
        for s in serial
        for o in rows
        if o is not s
    )
    check("serial exclusive", not overlap and len({s["worker"] for s in serial}) == 1)
    check(
        "serial after parallel",
        min(s["start"] for s in serial) >= max(p["end"] for p in par),
    )

    print("== failure output ==")
    g.write("sections/test_sections.py", SECTIONS)
    r = g.run("sections", "-n", "2")
    check("captured stdout section", "Captured stdout call" in r.stdout and "the database said no" in r.stdout)

    print("== -x / --maxfail ==")
    g.write("maxfail/test_maxfail.py", MAXFAIL)
    r = g.run("maxfail", "-n", "2", "-x", timeout=60)
    full = g.run("maxfail", "-n", "2", timeout=60)
    ran_x = int(r.stdout.split(" passed")[0].rsplit(" ", 1)[-1]) if " passed" in r.stdout else 0
    check("-x stops early", "1 failed" in r.stdout and ran_x < 8, r.stdout[-120:])
    check("full run unaffected", "8 passed" in full.stdout, full.stdout[-120:])

    print("== --lf ==")
    lf = g.tmp / "lf"
    shutil.rmtree(lf / ".pytest_cache", ignore_errors=True)
    g.write("lf/test_lf.py", LF)
    g.run("test_lf.py", "-n", "2", cwd=lf)
    r = g.run("test_lf.py", "-n", "2", "--lf", cwd=lf)
    check("--lf reruns only failures", "1 failed" in r.stdout and "passed" not in r.stdout, r.stdout[-200:])

    print("== junitxml ==")
    xml_path = g.tmp / "junit.xml"
    g.run("maxfail", "-n", "2", "--junitxml", str(xml_path), timeout=60)
    ts = ET.parse(xml_path).getroot().find("testsuite")
    check(
        "junit counts",
        ts is not None and ts.get("tests") == "9" and ts.get("failures") == "1",
        str(dict(ts.attrib) if ts is not None else None),
    )

    print("== --shard K/N ==")
    g.write("shardsuite/test_a.py", "".join(f"def test_a{i}(): assert True\n" for i in range(4)))
    g.write("shardsuite/test_b.py", "".join(f"def test_b{i}(): assert True\n" for i in range(4)))

    # Sibling shards MUST partition from an identical duration cache (in CI
    # each job restores the same snapshot). Run every shard in an isolated
    # cwd and wipe its cache first so all shards see the same cold cache --
    # otherwise shard 1 writes timings that shard 2 reads, and the partition
    # is no longer disjoint/covering.
    shard_cwd = g.tmp / "shardsuite"

    def shard_ids(k, n, *extra):
        shutil.rmtree(shard_cwd / ".rstest_cache", ignore_errors=True)
        tag = "_".join([str(k), str(n), *extra]).replace("/", "").replace("-", "")
        xp = g.tmp / f"shard_{tag}.xml"
        g.run(".", "-n", "2", "--shard", f"{k}/{n}", "--junitxml", str(xp), *extra, cwd=str(shard_cwd))
        root = ET.parse(xp).getroot()
        return {(tc.get("classname"), tc.get("name")) for tc in root.iter("testcase")}

    s1, s2 = shard_ids(1, 2), shard_ids(2, 2)
    check("shard: buckets disjoint", s1.isdisjoint(s2), f"overlap={s1 & s2}")
    check("shard: buckets cover the suite", len(s1 | s2) == 8, f"union={len(s1 | s2)}")
    check("shard: no empty bucket", bool(s1) and bool(s2), f"sizes={len(s1)},{len(s2)}")
    r = g.run("shardsuite", "-n", "2", "--shard", "5/4")
    check("shard: K>N rejected", r.returncode != 0 and "1..=4" in r.stderr, r.stderr[-160:])
    r = g.run("shardsuite", "-n", "2", "--shard", "1/2", "--shuffle")
    check("shard: +shuffle rejected", r.returncode != 0 and "not supported with --shuffle" in r.stderr, r.stderr[-160:])
    r = g.run("shardsuite", "-n", "2", "--shard", "1/1")
    check("shard: 1/1 runs whole suite", "8 passed" in r.stdout, r.stdout[-160:])

    # Lazy-collect shards at file granularity via shard_files.
    l1, l2 = shard_ids(1, 2, "--collect", "lazy"), shard_ids(2, 2, "--collect", "lazy")
    check("shard lazy: buckets disjoint", l1.isdisjoint(l2), f"overlap={l1 & l2}")
    check("shard lazy: buckets cover the suite", len(l1 | l2) == 8, f"union={len(l1 | l2)}")

    # Affinity mode (loadfile): a file's tests must never split across shards.
    def by_file(ids):
        files = {}
        for cls, name in ids:
            files.setdefault(cls, set()).add(name)
        return files

    f1 = by_file(shard_ids(1, 2, "--dist", "loadfile"))
    f2 = by_file(shard_ids(2, 2, "--dist", "loadfile"))
    split = [cls for cls in set(f1) & set(f2)]
    check("shard loadfile: no file split across shards", not split, f"split files={split}")
    all_files = set(f1) | set(f2)
    check("shard loadfile: buckets cover both files", len(all_files) == 2, f"files={all_files}")

    print("== --dist each ==")
    r = g.run("basic/test_basic.py", "-n", "2", "--dist", "each")
    check("each: counts are per-worker", "4 failed, 4 passed" in r.stdout, r.stdout[-200:])
    check("each: exit 1", r.returncode == 1)
    check(
        "each: outcomes keyed per worker",
        "[gw0]" in r.stdout and "[gw1]" in r.stdout,
        r.stdout[-400:],
    )
    r = g.run("basic/test_basic.py", "-n", "2", "--dist", "each", "--reruns", "2")
    check("each: --reruns rejected", r.returncode != 0 and "not supported" in r.stderr, r.stderr[-200:])

    print("== --dist validation ==")
    # An invalid --dist value must be rejected the same way on every path —
    # the small-suite/lazy path used to accept garbage silently (exit 0).
    r = g.run("basic/test_basic.py", "--dist", "bogus")
    check(
        "dist: bogus rejected on small/lazy path",
        r.returncode != 0 and "unknown --dist mode" in r.stderr,
        f"rc={r.returncode} " + r.stderr[-200:],
    )
    r = g.run("basic/test_basic.py", "-n", "2", "--dist", "bogus")
    check(
        "dist: bogus rejected on pool path",
        r.returncode != 0 and "unknown --dist mode" in r.stderr,
        f"rc={r.returncode} " + r.stderr[-200:],
    )

    print("== testnodedown for crashed workers ==")
    g.write("nodecrash/conftest.py", NODECRASH_CONFTEST)
    g.write("nodecrash/test_crashy.py", NODECRASH_TEST)
    crash_log = g.tmp / "node_crash.log"
    clear_hook_log(crash_log)
    r = g.run("nodecrash", "-n", "2", env_extra={"NODE_HOOK_LOG": str(crash_log)})
    text = read_hook_log(crash_log)
    ups = {l.split(":", 1)[1] for l in text.splitlines() if l.startswith("up:")}
    downs = {l.split(":", 1)[1] for l in text.splitlines() if l.startswith("down:")}
    check(
        "every provisioned ident torn down (incl. crashed worker's)",
        ups and ups == downs,
        f"ups={sorted(ups)} downs={sorted(downs)}\n" + r.stdout[-200:],
    )
    check("crash still attributed", "crashed while running" in r.stdout, r.stdout[-300:])

    print("== xdist master-side hooks ==")
    g.write("nodehooks/conftest.py", NODEHOOKS_CONFTEST)
    g.write("nodehooks/test_node.py", NODEHOOKS_TEST)
    node_log = g.tmp / "node_hooks.log"
    clear_hook_log(node_log)
    r = g.run("nodehooks", "-n", "2", env_extra={"NODE_HOOK_LOG": str(node_log)})
    check("configure_node fills workerinput", "2 passed" in r.stdout, r.stdout[-300:])
    log_text = read_hook_log(node_log)
    check(
        "testnodedown fired per worker",
        "down:follower_gw0" in log_text and "down:follower_gw1" in log_text,
        log_text,
    )
    check("testnodeready fired", "ready:gw0" in log_text, log_text)

    print("== one-arg pytest_testnodedown ==")
    g.write("nodeonearg/conftest.py", NODEONEARG_CONFTEST)
    # Two tests so both workers get real work (scheduling may still put both
    # on gw0, but every worker fires its own testnodedown regardless).
    g.write("nodeonearg/test_node.py", "def test_a(): assert True\n\n\ndef test_b(): assert True\n")
    oa_log = g.tmp / "node_onearg.log"
    clear_hook_log(oa_log)
    r = g.run("nodeonearg", "-n", "2", env_extra={"NODE_HOOK_LOG": str(oa_log)})
    oa_text = read_hook_log(oa_log)
    check("one-arg testnodedown: run not crashed", "2 passed" in r.stdout, r.stdout[-300:])
    check(
        "one-arg testnodedown fired (no error= TypeError)",
        "down:oa_gw0" in oa_text and "down:oa_gw1" in oa_text,
        oa_text + "\n" + r.stdout[-300:],
    )

    print("== --durations ==")
    g.write("dur/test_dur.py", DURATIONS_FIXTURE)
    r = g.run("dur", "-n", "2", "--durations=5")
    check("durations block in pool", "slowest 5 durations" in r.stdout, r.stdout[-300:])
    check(
        "durations slow test listed",
        "call" in r.stdout and "test_sleepy" in r.stdout.split("slowest")[-1],
        r.stdout[-300:],
    )
    check("durations hidden note", "durations < 0.005s hidden" in r.stdout, r.stdout[-300:])
    r = g.run("dur", "-n", "0", "--durations=0", "-vv")
    check(
        "durations -n0, 0=all, -vv unhides",
        "slowest durations" in r.stdout and "hidden" not in r.stdout,
        r.stdout[-300:],
    )
    r = g.run("dur", "-n", "2")
    check("no durations block unrequested", "slowest" not in r.stdout, r.stdout[-200:])

    print("== --doctest-modules ==")
    g.write("doctests/mymod.py", DOCTEST_MOD)
    g.write("doctests/test_real.py", "def test_plain(): assert True\n")
    r = g.run(".", "-n", "2", "--doctest-modules", cwd=g.tmp / "doctests")
    check(
        "doctest-modules pool counts",
        "1 failed, 2 passed" in r.stdout,
        r.stdout[-200:],
    )
    check(
        "doctest failure rendered",
        "Expected:" in r.stdout and "Got:" in r.stdout,
        r.stdout[-400:],
    )
    r = g.run(".", "-n", "0", "--doctest-modules", cwd=g.tmp / "doctests")
    check("doctest-modules -n 0", "1 failed, 2 passed" in r.stdout, r.stdout[-200:])

    print("== monorepo ==")
    mono = g.tmp / "mono"
    shutil.rmtree(mono, ignore_errors=True)
    g.write("mono/libs/a/pytest.ini", "[pytest]\n")
    g.write("mono/libs/a/tests/test_a.py", "def test_a1(): pass\ndef test_a2(): pass\n")
    g.write(
        "mono/libs/b/pyproject.toml",
        '[tool.pytest.ini_options]\ntestpaths = ["tests"]\n',
    )
    g.write("mono/libs/b/tests/test_b.py", "def test_b1(): pass\ndef test_b2(): assert False\n")
    # a pyproject without a pytest section is NOT a project
    g.write("mono/libs/c/pyproject.toml", '[project]\nname = "c"\n')
    r = g.run("-n", "2", cwd=mono)
    check(
        "mono: discovers configured projects",
        "monorepo: 2 projects" in r.stdout and "libs/c" not in r.stdout,
        r.stdout[:300],
    )
    check(
        "mono: per-project results",
        "2 passed" in r.stdout and "1 failed, 1 passed" in r.stdout,
        r.stdout[-600:],
    )
    check(
        "mono: summary verdicts",
        "libs/a" in r.stdout and "FAILED (exit 1)" in r.stdout,
        r.stdout[-400:],
    )
    check("mono: merged exit", r.returncode == 1)
    rep = g.tmp / "mono-report.json"
    g.run("-n", "2", "--report-json", str(rep), cwd=mono)
    doc = json.loads(rep.read_text(encoding="utf-8"))
    check(
        "mono: merged report, root-relative keys",
        doc["meta"]["schema"] == 4
        and any(k.startswith("libs/a/") for k in doc["tests"])
        and any(k.startswith("libs/b/") for k in doc["tests"]),
        str(list(doc["tests"])[:4]),
    )
    check(
        "mono: merged report per-project meta + counts",
        doc["meta"]["projects"]["libs/b"]["exitstatus"] == 1
        and doc["meta"]["exitstatus"] == 1
        and doc["meta"]["counts"]["passed"] == 3
        and doc["meta"]["counts"]["failed"] == 1
        and doc["meta"]["projects"]["libs/b"]["counts"]["failed"] == 1,
        str(doc["meta"])[:400],
    )
    # --output forwards to each project. github annotations carry the
    # project's ROOT-relative path (children run with cwd=project, but GitHub
    # resolves annotation files from the repo root).
    r = g.run("-n", "2", "--output", "github", cwd=mono)
    ann = [ln for ln in r.stdout.splitlines() if ln.startswith("::error ")]
    check(
        "mono: github annotations use root-relative file paths",
        len(ann) == 1 and "file=libs/b/" in ann[0],
        (ann[0] if ann else "<no annotation>"),
    )
    # --output json is refused at a monorepo root (no clean merged stream);
    # the error must steer the user to --report-json.
    r = g.run("-n", "2", "--output", "json", cwd=mono)
    check(
        "mono: --output json refused with guidance",
        r.returncode != 0 and "--report-json" in r.stderr and "--output json" in r.stderr,
        r.stderr[-200:],
    )
    # Discovery (--collect-only --report-json) needs a single session, so a
    # monorepo ROOT refuses it (run per-project instead); INSIDE a project it
    # writes the discovery doc scoped to that project's rootdir.
    rootdisc = g.tmp / "mono-rootdisc.json"
    r = g.run("--collect-only", "--report-json", str(rootdisc), cwd=mono)
    check(
        "mono: discovery refused at root, points to per-project",
        r.returncode != 0 and "per project" in (r.stdout + r.stderr) and not rootdisc.exists(),
        (r.stdout + r.stderr)[-300:],
    )
    projdisc = g.tmp / "mono-projdisc.json"
    g.run("--collect-only", "--report-json", str(projdisc), cwd=mono / "libs" / "a")
    pdoc = json.loads(projdisc.read_text(encoding="utf-8"))
    check(
        "mono: per-project discovery scoped to the project",
        pdoc["meta"]["kind"] == "discovery"
        and pdoc["meta"]["rootdir"].replace("\\", "/").endswith("libs/a")
        and len(pdoc["tests"]) == 2
        and all(t["nodeid"].startswith("tests/test_a.py::") for t in pdoc["tests"]),
        str(pdoc["meta"]),
    )
    # explicit path args opt out of monorepo mode
    r = g.run("libs/a", "-n", "2", cwd=mono)
    check(
        "mono: path arg targets single project",
        "monorepo" not in r.stdout and "2 passed" in r.stdout,
        r.stdout[:200],
    )
    # concurrency: two 1.2s projects must overlap, not serialize
    g.write("mono2/p1/pytest.ini", "[pytest]\n")
    g.write("mono2/p1/test_one.py", "import time\ndef test_s(): time.sleep(1.2)\n")
    g.write("mono2/p2/pytest.ini", "[pytest]\n")
    g.write("mono2/p2/test_two.py", "import time\ndef test_s(): time.sleep(1.2)\n")
    t0 = time.monotonic()
    r = g.run("-n", "2", cwd=g.tmp / "mono2", timeout=120)
    wall = time.monotonic() - t0
    # Two 1.2s projects: concurrent ~= 1.2s + per-project startup, serial >=
    # 2.4s + 2x startup. Windows pays a much heavier process/interpreter spawn
    # cost, so the absolute wall is larger while still proving overlap.
    limit = 4.0 if WINDOWS else 2.4
    check(
        "mono: projects run concurrently",
        r.returncode == 0 and wall < limit,
        f"wall={wall:.2f}s rc={r.returncode} limit={limit}",
    )

    # --changed across projects: direct narrows, dependent runs full,
    # unaffected is skipped entirely
    cm = g.tmp / "monochg"
    shutil.rmtree(cm, ignore_errors=True)
    g.write(
        "monochg/libs/a/pyproject.toml",
        '[project]\nname = "pkg-a"\ndependencies = []\n\n[tool.pytest.ini_options]\n',
    )
    g.write("monochg/libs/a/src_a.py", "VALUE = 1\n")
    g.write("monochg/libs/a/test_a.py", "import src_a\ndef test_a(): assert src_a.VALUE\n")
    g.write("monochg/libs/a/test_a_other.py", "def test_other(): pass\n")
    g.write(
        "monochg/libs/b/pyproject.toml",
        '[project]\nname = "pkg-b"\ndependencies = ["pkg-a"]\n\n[tool.pytest.ini_options]\n',
    )
    g.write("monochg/libs/b/test_b.py", "def test_b(): pass\n")
    g.write(
        "monochg/libs/c/pyproject.toml",
        '[project]\nname = "pkg-c"\ndependencies = []\n\n[tool.pytest.ini_options]\n',
    )
    g.write("monochg/libs/c/test_c.py", "def test_c(): pass\n")
    subprocess.run(["git", "init", "-q"], cwd=cm, check=True)
    subprocess.run(["git", "add", "-A"], cwd=cm, check=True)
    subprocess.run(
        ["git", "-c", "user.email=g@g", "-c", "user.name=g", "commit", "-qm", "base"],
        cwd=cm,
        check=True,
    )
    (cm / "libs/a/src_a.py").write_text("VALUE = 2\n")
    r = g.run("-n", "2", "--changed", cwd=cm)
    check(
        "mono: --changed classifies projects",
        "2 of 3 projects affected" in r.stderr,
        r.stderr[-300:],
    )
    check(
        "mono: unaffected project skipped",
        "libs/c" in r.stdout and "skipped (no changes)" in r.stdout,
        r.stdout[-400:],
    )
    def section(text, rel):
        try:
            seg = text.split(f"project: {rel} ")[1]
        except IndexError:
            return ""
        return seg.split("=== project:")[0].split("=== monorepo")[0]

    sec_a = section(r.stdout, "libs/a")
    check(
        "mono: direct project narrows by import graph",
        "1 passed" in sec_a and "2 passed" not in sec_a,
        sec_a[-300:],
    )
    sec_b = section(r.stdout, "libs/b")
    check(
        "mono: dependent project runs full",
        "1 passed" in sec_b and "no tests affected" not in sec_b,
        sec_b[-300:],
    )

    # per-project venv: a project-local .venv wins over the inherited env
    pv = g.tmp / "monovenv"
    shutil.rmtree(pv, ignore_errors=True)
    g.write("monovenv/libs/a/pytest.ini", "[pytest]\n")
    g.write(
        "monovenv/libs/a/test_env.py",
        "import sys\ndef test_which(): print('PYEXE=' + sys.executable)\n",
    )
    local_venv = pv / "libs/a/.venv"
    local_venv.parent.mkdir(parents=True, exist_ok=True)
    if WINDOWS:
        # No exec shims on Windows. Junction the whole .venv to the gate venv:
        # Scripts/python.exe is then a real interpreter with msgpack on its
        # path, and discover_python's Scripts/python.exe probe resolves it.
        subprocess.run(
            ["cmd", "/c", "mklink", "/J", str(local_venv), str(g.venv)],
            check=True,
            capture_output=True,
        )
    else:
        local_bin = local_venv / "bin"
        local_bin.mkdir(parents=True, exist_ok=True)
        local_py = local_bin / "python"
        # An exec shim, not a symlink: a bare symlink loses pyvenv.cfg
        # resolution and lands on the base interpreter (no msgpack).
        local_py.write_text(f'#!/bin/sh\nexec "{g.venv}/bin/python" "$@"\n')
        local_py.chmod(0o755)
    g.write("monovenv/libs/b/pytest.ini", "[pytest]\n")
    g.write("monovenv/libs/b/test_b.py", "def test_b(): pass\n")
    r = g.run("-n", "1", "-v", "-s", "--co", cwd=pv)  # passthrough must bail
    check(
        "mono: passthrough flags bail",
        r.returncode != 0 and "single pytest session" in r.stderr,
        r.stderr[-200:],
    )
    r = g.run("-n", "2", cwd=pv)
    check(
        "mono: project-local venv used",
        r.returncode == 0 and "1 passed" in r.stdout,
        r.stdout[-400:],
    )

    # per-project [tool.rstest]: a numprocesses pin survives the planner
    pp = g.tmp / "monopin"
    shutil.rmtree(pp, ignore_errors=True)
    g.write(
        "monopin/libs/a/pyproject.toml",
        '[tool.pytest.ini_options]\ntestpaths = ["."]\n\n[tool.rstest]\nnumprocesses = 0\n',
    )
    g.write("monopin/libs/a/test_a.py", "def test_a(): pass\n")
    g.write("monopin/libs/b/pytest.ini", "[pytest]\n")
    g.write("monopin/libs/b/test_b.py", "def test_b(): pass\n")
    r = g.run("-n", "4", cwd=pp)
    check(
        "mono: per-project numprocesses pin",
        "libs/a:-n0" in r.stdout and "pytest-exact" in r.stdout and r.returncode == 0,
        r.stdout[:400],
    )

    # coverage under monorepo mode: per-project reports, no collision
    mc = g.tmp / "monocov"
    shutil.rmtree(mc, ignore_errors=True)
    g.write("monocov/libs/a/pytest.ini", "[pytest]\n")
    g.write("monocov/libs/a/pkg_a.py", "def f():\n    return 1\n")
    g.write("monocov/libs/a/test_a.py", "import pkg_a\ndef test_a(): assert pkg_a.f() == 1\n")
    g.write("monocov/libs/b/pytest.ini", "[pytest]\n")
    g.write("monocov/libs/b/pkg_b.py", "def g():\n    return 2\n")
    g.write("monocov/libs/b/test_b.py", "import pkg_b\ndef test_b(): assert pkg_b.g() == 2\n")
    r = g.run("-n", "2", "--cov=.", cwd=mc)
    sec_ca = r.stdout.split("project: libs/a ")[-1].split("=== project")[0]
    sec_cb = r.stdout.split("project: libs/b ")[-1].split("=== project")[0].split("=== monorepo")[0]
    check(
        "mono: per-project coverage reports",
        r.returncode == 0 and "pkg_a.py" in sec_ca and "pkg_b.py" in sec_cb,
        r.stdout[-600:],
    )
    check(
        "mono: coverage does not cross projects",
        "pkg_b.py" not in sec_ca and "pkg_a.py" not in sec_cb,
        sec_ca[-300:],
    )

    # --changed-strict: undeclared sibling import counts as an edge;
    # nothing-affected exits 5
    cs = g.tmp / "monostrict"
    shutil.rmtree(cs, ignore_errors=True)
    g.write(
        "monostrict/libs/a/pyproject.toml",
        '[project]\nname = "pkg-a"\ndependencies = []\n\n[tool.pytest.ini_options]\n',
    )
    g.write("monostrict/libs/a/pkg_a/__init__.py", "VALUE = 1\n")
    g.write("monostrict/libs/a/test_a.py", "from pkg_a import VALUE\ndef test_a(): assert VALUE\n")
    g.write(
        "monostrict/libs/b/pyproject.toml",
        '[project]\nname = "pkg-b"\ndependencies = []\n\n[tool.pytest.ini_options]\n',
    )
    # b's test imports pkg_a WITHOUT declaring it (shared-venv trap)
    g.write("monostrict/libs/b/test_b.py", "def test_b(): pass\n")
    g.write("monostrict/libs/b/helper.py", "import pkg_a\n")
    subprocess.run(["git", "init", "-q"], cwd=cs, check=True)
    subprocess.run(["git", "add", "-A"], cwd=cs, check=True)
    subprocess.run(
        ["git", "-c", "user.email=g@g", "-c", "user.name=g", "commit", "-qm", "base"],
        cwd=cs,
        check=True,
    )
    (cs / "libs/a/pkg_a/__init__.py").write_text("VALUE = 2\n")
    r_lax = g.run("-n", "2", "--changed", cwd=cs)
    r_strict = g.run("-n", "2", "--changed-strict", cwd=cs)
    check(
        "strict: undeclared sibling import counted",
        "skipped (no changes)" in r_lax.stdout
        and "skipped (no changes)" not in r_strict.stdout
        and "without declaring it" in r_strict.stderr,
        f"lax:{r_lax.stdout[-200:]}\nstrict:{r_strict.stderr[-200:]}",
    )
    # nothing affected at all -> exit 5 under strict, 0 without
    subprocess.run(["git", "add", "-A"], cwd=cs, check=True)
    subprocess.run(
        ["git", "-c", "user.email=g@g", "-c", "user.name=g", "commit", "-qm", "x"],
        cwd=cs,
        check=True,
    )
    r0 = g.run("-n", "2", "--changed", cwd=cs)
    r5 = g.run("-n", "2", "--changed-strict", cwd=cs)
    check(
        "strict: nothing affected exits 5 (lax exits 0)",
        r0.returncode == 0 and r5.returncode == 5,
        f"lax={r0.returncode} strict={r5.returncode}",
    )

    # single-project strict: a changed file unreachable from tests
    sp = g.tmp / "strictone"
    shutil.rmtree(sp, ignore_errors=True)
    g.write("strictone/pytest.ini", "[pytest]\n")
    g.write("strictone/used.py", "X = 1\n")
    g.write("strictone/test_main.py", "import used\ndef test_x(): assert used.X\n")
    g.write("strictone/orphan.py", "Y = 1\n")  # imported by nothing
    subprocess.run(["git", "init", "-q"], cwd=sp, check=True)
    subprocess.run(["git", "add", "-A"], cwd=sp, check=True)
    subprocess.run(
        ["git", "-c", "user.email=g@g", "-c", "user.name=g", "commit", "-qm", "base"],
        cwd=sp,
        check=True,
    )
    (sp / "orphan.py").write_text("Y = 2\n")
    r_lax = g.run("-n", "2", "--changed", cwd=sp)
    r_str = g.run("-n", "2", "--changed-strict", cwd=sp)
    check(
        "strict: unreachable changed file forces full run",
        "no tests affected" in r_lax.stdout
        and "reaches no tests" in r_str.stderr
        and "1 passed" in r_str.stdout,
        f"lax:{r_lax.stdout[-150:]} strict:{r_str.stderr[-250:]}",
    )

    # [tool.rstest] projects restricts discovery
    g.write("mono/pyproject.toml", '[tool.rstest]\nprojects = ["libs/a"]\n')
    r = g.run("-n", "2", cwd=mono)
    check(
        "mono: projects globs filter",
        "monorepo: 1 projects" in r.stdout and r.returncode == 0,
        r.stdout[:300],
    )

    print("== warnings ==")
    g.write("warn/test_warn.py", WARN)
    r = g.run("warn", "-n", "2")
    check("warnings summary section", "warnings summary" in r.stdout and "UserWarning" in r.stdout)
    check("warnings in counts", "warnings in" in r.stdout, r.stdout[-120:])

    print("== doctor ==")
    g.write("doc/test_doc.py", DOCTOR)
    r = g.run("doc", "-n", "2", "--doctor")
    check("doctor renders", "rstest doctor" in r.stdout and "SLOWEST FILES" in r.stdout)
    check("doctor wait-bound", "WAIT-BOUND" in r.stdout, r.stdout[-400:])
    dj = g.tmp / "doctor.json"
    g.run("doc", "-n", "2", "--doctor-json", str(dj))
    d = json.loads(dj.read_text(encoding="utf-8"))
    check(
        "doctor json schema",
        d.get("schema") == 2
        and d.get("wait_bound")
        and any("test_sleepy" in t["nodeid"] for t in d["wait_bound"]["tests"]),
        str(d)[:200],
    )
    dm = g.tmp / "doctor.md"
    summ = g.tmp / "summary.md"
    g.run(
        "doc", "-n", "2", "--doctor-md", str(dm),
        env_extra={"GITHUB_STEP_SUMMARY": str(summ)},
    )
    md = dm.read_text(encoding="utf-8")
    check(
        "doctor markdown",
        md.startswith("## rstest doctor")
        and "**Wait-bound:**" in md
        and "### Slowest files" in md
        and "test_sleepy" in md,
        md[:300],
    )
    check(
        "doctor auto-appends job summary",
        summ.exists() and "## rstest doctor" in summ.read_text(encoding="utf-8"),
    )

    # --doctor-fail-on: turn the doctor signal into a CI gate. The DOCTOR
    # suite is ~all wait (test_sleepy), so wait_pct is high.
    r = g.run("doc", "-n", "2", "--doctor-fail-on", "wait_pct>50")
    check(
        "doctor-fail-on: breach fails the run (exit 1)",
        # Failure block goes to STDERR so --output json/tap stay pure.
        r.returncode == 1 and "doctor gate failures" in r.stderr and "wait_pct" in r.stderr,
        f"rc={r.returncode} " + r.stderr[-300:],
    )
    # A gate breach under --output json must NOT corrupt the pure NDJSON stream:
    # the failure block is stderr-only.
    r = g.run("doc", "-n", "2", "--output", "json", "--doctor-fail-on", "wait_pct>1")
    ok, _objs = parse_ndjson(r.stdout)
    check(
        "doctor-fail-on: breach keeps --output json pure (stderr only)",
        r.returncode == 1 and ok and "doctor gate failures" not in r.stdout
        and "doctor gate failures" in r.stderr,
        f"rc={r.returncode} ndjson_ok={ok} " + r.stdout[-200:],
    )
    # A threshold the run clears: gate passes, exit stays 0.
    r = g.run("doc", "-n", "2", "--doctor-fail-on", "wall_seconds>1000")
    check(
        "doctor-fail-on: within threshold passes (exit 0)",
        r.returncode == 0 and "condition(s) passed" in r.stderr,
        f"rc={r.returncode} " + r.stderr[-200:],
    )
    # A metric whose section didn't apply to this run (single-worker has no
    # parallel efficiency) is skipped, never failed.
    r = g.run("doc", "-n", "0", "--doctor-fail-on", "parallel_efficiency<1")
    check(
        "doctor-fail-on: absent metric skipped, not failed",
        r.returncode == 0 and "not measured" in r.stderr,
        f"rc={r.returncode} " + r.stderr[-200:],
    )
    # A typo'd metric aborts up front (before the run) rather than silently
    # never firing — the exact dead-gate bug this feature exists to kill.
    r = g.run("doc", "-n", "2", "--doctor-fail-on", "bogus<1")
    check(
        "doctor-fail-on: bad metric aborts loudly",
        r.returncode != 0 and "unknown metric" in r.stderr,
        f"rc={r.returncode} " + r.stderr[-200:],
    )

    print("== auto worker capping ==")
    for i in range(6):
        g.write(f"kovstyle/mod{i}_test.py", "def test_a(): assert True\n")
    r = g.run(cwd=g.tmp / "kovstyle")
    check(
        "auto scales on *_test.py suites",
        "workers (parallel" in r.stdout.splitlines()[0],
        r.stdout[:100],
    )
    g.write("tiny/test_one.py", "def test_only(): assert True\n")
    r = g.run(cwd=g.tmp / "tiny")
    check(
        "auto caps tiny suite to single worker",
        "single worker" in r.stdout.splitlines()[0],
        r.stdout[:100],
    )

    print("== coverage ==")
    g.write("cov/mypkg/__init__.py", "def used(x):\n    return x * 2\n\n\ndef unused(x):\n    return x - 1\n")
    g.write("cov/test_cov.py", "from mypkg import used\n\n\ndef test_used():\n    assert used(2) == 4\n")
    covdir = g.tmp / "cov"
    r = g.run("test_cov.py", "-n", "2", "--cov=mypkg", "--cov-report=term", cwd=covdir,
              env_extra={"PYTHONPATH": str(covdir)})
    check("coverage report under pool", "mypkg" in r.stdout and "__init__.py" in r.stdout and "%" in r.stdout, r.stdout[-300:])
    r = g.run("test_cov.py", "-n", "2", "--cov=mypkg", "--cov-fail-under=99", cwd=covdir,
              env_extra={"PYTHONPATH": str(covdir)})
    check("coverage fail-under exits 1", r.returncode == 1 and "FAIL Required" in r.stdout, r.stdout[-200:])

    print("== smart selection ==")
    sp = g.tmp / "selproj"
    g.write("selproj/pkg/__init__.py", "")
    g.write("selproj/pkg/a.py", "def alpha():\n    return 1\n")
    g.write("selproj/pkg/b.py", "def beta():\n    return 2\n")
    g.write("selproj/pkg/c.py", "from .a import alpha\n\n\ndef gamma():\n    return alpha() + 1\n")
    g.write("selproj/tests/test_a.py", "from pkg.a import alpha\n\ndef test_alpha(): assert alpha() == 1\n")
    g.write("selproj/tests/test_b.py", "from pkg.b import beta\n\ndef test_beta(): assert beta() == 2\n")
    g.write("selproj/tests/test_c.py", "from pkg.c import gamma\n\ndef test_gamma(): assert gamma() == 2\n")
    g.write("selproj/pyproject.toml", '[tool.pytest.ini_options]\ntestpaths = ["tests"]\n')
    subprocess.run(["git", "init", "-q"], cwd=sp, check=True)
    subprocess.run(["git", "add", "-A"], cwd=sp, check=True)
    subprocess.run(
        ["git", "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "init"],
        cwd=sp, check=True,
    )
    with open(sp / "pkg" / "a.py", "a") as f:
        f.write("# touched\n")
    r = g.run("--changed", "-v", cwd=sp, env_extra={"PYTHONPATH": str(sp)})
    check(
        "selection: direct + transitive, excludes unrelated",
        "2 affected test file(s)" in r.stderr
        and "test_alpha" in r.stdout
        and "test_gamma" in r.stdout
        and "test_beta" not in r.stdout,
        r.stderr[-200:] + r.stdout[-200:],
    )
    g.write("selproj/pytest.ini", "[pytest]\n")
    r = g.run("--changed", cwd=sp, env_extra={"PYTHONPATH": str(sp)})
    check("selection: config change -> full run", "falling back to full run" in r.stderr, r.stderr[-200:])
    (sp / "pytest.ini").unlink()
    subprocess.run(["git", "add", "-A"], cwd=sp, check=True)
    subprocess.run(
        ["git", "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "w"],
        cwd=sp, check=True,
    )
    r = g.run("--changed", cwd=sp, env_extra={"PYTHONPATH": str(sp)})
    check("selection: clean tree -> nothing", r.returncode == 0 and "no tests affected" in r.stdout, r.stdout[-200:])
    g.write("selproj/tests/conftest.py", "import pytest\n")
    r = g.run("--changed", "-v", cwd=sp, env_extra={"PYTHONPATH": str(sp)})
    check(
        "selection: conftest -> whole subtree",
        "3 affected test file(s)" in r.stderr and "test_beta" in r.stdout,
        r.stderr[-200:],
    )
    (sp / "tests" / "conftest.py").unlink()
    g.write("selproj/tests/test_new.py", "def test_fresh(): assert True\n")
    r = g.run("--changed", "-v", cwd=sp, env_extra={"PYTHONPATH": str(sp)})
    check(
        "selection: untracked test file selected",
        "test_fresh" in r.stdout and "test_beta" not in r.stdout,
        r.stdout[-200:],
    )
    (sp / "tests" / "test_new.py").unlink()

    print("== shuffle ==")
    for i in range(6):
        g.write(f"shuf/test_s{i}.py", f"def test_s{i}(): assert True\n")
    r = g.run("shuf", "-n", "2", "--shuffle=42")
    check(
        "shuffle: explicit seed echoed, run green",
        r.returncode == 0
        and "shuffle seed 42" in r.stderr
        and "--shuffle=42" in r.stderr
        and "6 passed" in r.stdout,
        r.stderr[-200:] + r.stdout[-100:],
    )
    r = g.run("shuf", "-n", "2", "--shuffle")
    check(
        "shuffle: random seed printed with reproduce hint",
        r.returncode == 0 and "reproduce with --shuffle=" in r.stderr,
        r.stderr[-200:],
    )
    r = g.run("shuf", "-n", "0", "--shuffle")
    check(
        "shuffle: refused in single-worker mode",
        r.returncode != 0 and "parallel pool" in r.stderr,
        f"rc={r.returncode} " + r.stderr[-200:],
    )

    print("== duration regression gate ==")
    g.write(
        "dreg/test_d.py",
        "import os, time\n\n"
        "def test_variable():\n"
        "    time.sleep(float(os.environ.get('DREG_SLEEP', '0.1')))\n\n"
        "def test_stable():\n"
        "    time.sleep(0.05)\n",
    )
    ddir = g.tmp / "dreg"
    r = g.run(".", "-n", "2", "--durations-regress", "2.0", cwd=ddir,
              env_extra={"DREG_SLEEP": "0.1"})
    check(
        "durations-regress: cold baseline skips, run green",
        r.returncode == 0 and "no duration baseline yet" in r.stderr,
        f"rc={r.returncode} " + r.stderr[-200:],
    )
    r = g.run(".", "-n", "2", "--durations-regress", "2.0", cwd=ddir,
              env_extra={"DREG_SLEEP": "0.1"})
    check(
        "durations-regress: warm baseline, no regressions",
        r.returncode == 0 and "no regressions" in r.stderr,
        f"rc={r.returncode} " + r.stderr[-200:],
    )
    r = g.run(".", "-n", "2", "--durations-regress", "2.0", cwd=ddir,
              env_extra={"DREG_SLEEP": "1.2"})
    check(
        "durations-regress: regression flagged, exit 1, stable test quiet",
        r.returncode == 1
        and "duration regressions" in r.stdout
        and "test_variable" in r.stdout
        and "test_stable" not in r.stdout.split("duration regressions")[1]
        and "1 duration regression" in r.stderr,
        f"rc={r.returncode} " + r.stdout[-300:] + r.stderr[-150:],
    )

    # PR-aware --changed: with GITHUB_BASE_REF set, bare --changed diffs vs
    # the merge-base with origin/<base> — a clean checkout of a PR commit
    # still selects the PR's files (vs HEAD it would select nothing).
    base_sha = subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=sp, capture_output=True, text=True, check=True
    ).stdout.strip()
    subprocess.run(
        ["git", "update-ref", "refs/remotes/origin/mainline", base_sha], cwd=sp, check=True
    )
    with open(sp / "pkg" / "a.py", "a") as f:
        f.write("# pr change\n")
    subprocess.run(["git", "add", "-A"], cwd=sp, check=True)
    subprocess.run(
        ["git", "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "pr"],
        cwd=sp, check=True,
    )
    r = g.run(
        "--changed", "-v", cwd=sp,
        env_extra={"PYTHONPATH": str(sp), "GITHUB_BASE_REF": "mainline"},
    )
    check(
        "selection: GITHUB_BASE_REF auto-targets PR base",
        "auto-targets PR base origin/mainline" in r.stderr
        and "2 affected test file(s)" in r.stderr
        and "test_alpha" in r.stdout
        and "test_beta" not in r.stdout,
        r.stderr[-300:],
    )
    r = g.run(
        "--changed", cwd=sp,
        env_extra={"PYTHONPATH": str(sp), "GITHUB_BASE_REF": "nosuchbranch"},
    )
    check(
        "selection: missing PR base ref errors, no silent skip",
        r.returncode != 0 and "fetch the base branch" in r.stderr,
        f"rc={r.returncode} " + r.stderr[-300:],
    )
    r = g.run(
        "--changed=HEAD~1", cwd=sp,
        env_extra={"PYTHONPATH": str(sp), "GITHUB_BASE_REF": "mainline"},
    )
    check(
        "selection: explicit rev wins over GITHUB_BASE_REF",
        "auto-targets" not in r.stderr and "2 affected test file(s)" in r.stderr,
        r.stderr[-300:],
    )
    # Buildkite exposes the base as a branch name, resolved the same way.
    r = g.run(
        "--changed", "-v", cwd=sp,
        env_extra={"PYTHONPATH": str(sp), "BUILDKITE_PULL_REQUEST_BASE_BRANCH": "mainline"},
    )
    check(
        "selection: Buildkite base branch auto-targets PR base",
        "auto-targets PR base origin/mainline" in r.stderr and "test_alpha" in r.stdout,
        r.stderr[-300:],
    )
    # GitLab provides the exact diff-base SHA — used directly, no merge-base.
    r = g.run(
        "--changed", "-v", cwd=sp,
        env_extra={"PYTHONPATH": str(sp), "CI_MERGE_REQUEST_DIFF_BASE_SHA": base_sha},
    )
    check(
        "selection: GitLab diff-base SHA auto-targets MR base",
        "auto-targets MR base" in r.stderr and "test_alpha" in r.stdout,
        r.stderr[-300:],
    )
    # An unresolvable GitLab base SHA errors — never a silent full skip.
    r = g.run(
        "--changed", cwd=sp,
        env_extra={"PYTHONPATH": str(sp), "CI_MERGE_REQUEST_DIFF_BASE_SHA": "0" * 40},
    )
    check(
        "selection: missing GitLab base SHA errors, no silent skip",
        r.returncode != 0 and "not in the local clone" in r.stderr,
        f"rc={r.returncode} " + r.stderr[-300:],
    )

    print("== [tool.rstest] config ==")
    g.write("toolcfg/pyproject.toml", "[tool.rstest]\nnumprocesses = 2\nreruns = 1\n")
    g.write("toolcfg/test_cfg.py", FLAKY)
    tmarker = g.tmp / "toolcfg_marker"
    if tmarker.exists():
        tmarker.unlink()
    r = g.run("test_cfg.py", cwd=g.tmp / "toolcfg", env_extra={"FLAKY_MARKER": str(tmarker)})
    check(
        "tool.rstest defaults applied",
        r.returncode == 0 and "2 workers" in r.stdout.splitlines()[0] and "1 flaky" in r.stdout,
        r.stdout[:120] + r.stdout[-160:],
    )
    r = g.run("test_cfg.py", "-n", "0", cwd=g.tmp / "toolcfg", env_extra={"FLAKY_MARKER": str(tmarker)})
    check("CLI overrides tool.rstest", "single worker" in r.stdout.splitlines()[0], r.stdout[:120])
    check(
        "tail-batch rerun works (EndSession model)",
        True,  # asserted by 'tool.rstest defaults applied': 1 item, rerun delivered post-drain
    )

    print("== flaky reruns ==")
    g.write("flaky/test_flaky.py", FLAKY)
    fdir = g.tmp / "flaky"
    marker = g.tmp / "flaky_marker"
    if marker.exists():
        marker.unlink()
    r = g.run("test_flaky.py", "-n", "2", "--reruns", "2", cwd=fdir,
              env_extra={"FLAKY_MARKER": str(marker)})
    check("flaky passes with reruns", r.returncode == 0 and "1 flaky" in r.stdout, r.stdout[-200:])
    check("flaky section listed", "passed after rerun" in r.stdout)
    marker.unlink()

    # Single-worker reruns: --reruns at -n 1 / -n 0 must fire (a degenerate
    # one-worker pool drives the rerun loop) instead of being silently inert.
    swm = g.tmp / "sw_reruns_marker"
    swm.unlink(missing_ok=True)
    r = g.run("test_flaky.py", "-n", "1", "--reruns", "2", cwd=fdir,
              env_extra={"FLAKY_MARKER": str(swm)})
    check(
        "single-worker reruns fire at -n 1",
        r.returncode == 0 and "1 flaky" in r.stdout
        and "single worker (rerun pool" in r.stdout.splitlines()[0]
        # the byte-exact -> pool switch is announced on stderr, not just the banner
        and "not byte-exact" in r.stderr,
        f"rc={r.returncode} " + r.stdout.splitlines()[0] + " || " + r.stderr[-200:],
    )
    swm.unlink(missing_ok=True)
    r = g.run("test_flaky.py", "-n", "0", "--reruns", "2", cwd=fdir,
              env_extra={"FLAKY_MARKER": str(swm)})
    check(
        "single-worker reruns fire at -n 0",
        r.returncode == 0 and "1 flaky" in r.stdout,
        f"rc={r.returncode} " + r.stdout[-200:],
    )
    # No --reruns at -n 1 stays byte-exact single session: the flake fails.
    swm.unlink(missing_ok=True)
    r = g.run("test_flaky.py", "-n", "1", cwd=fdir,
              env_extra={"FLAKY_MARKER": str(swm)})
    check(
        "no-reruns at -n 1 stays byte-exact (flake fails)",
        r.returncode == 1 and "1 failed" in r.stdout
        and "pytest-exact mode" in r.stdout.splitlines()[0],
        f"rc={r.returncode} " + r.stdout.splitlines()[0] + " || " + r.stdout[-200:],
    )
    # Passthrough (-s) can't be pooled: reruns stay inert, warned.
    swm.unlink(missing_ok=True)
    r = g.run("test_flaky.py", "-n", "1", "--reruns", "2", "-s", cwd=fdir,
              env_extra={"FLAKY_MARKER": str(swm)})
    check(
        "reruns inert under -s, warned",
        r.returncode == 1 and "ignored under -s" in r.stderr,
        f"rc={r.returncode} " + r.stderr[-200:],
    )
    marker.unlink(missing_ok=True)
    r = g.run("test_flaky.py", "-n", "2", cwd=fdir,
              env_extra={"FLAKY_MARKER": str(marker)})
    check("flaky fails without reruns", r.returncode == 1 and "1 failed" in r.stdout, r.stdout[-200:])

    print("== flaky-aware reruns (--reruns-only-known-flaky) ==")
    # Gate reruns on prior flaky history so a deterministic mass-failure
    # doesn't burn the budget. Fixture: test_flaky_once fails its first
    # attempt then passes; nodeid is `test_flaky.py::test_flaky_once`.
    fnode = "test_flaky.py::test_flaky_once"
    fcache = g.tmp / "flaky" / ".rstest_cache" / "flakes.json"
    fcache.parent.mkdir(parents=True, exist_ok=True)

    def faware(hist, marker_name, *extra):
        # hist: dict written to flakes.json (or None to remove it).
        m = g.tmp / marker_name
        m.unlink(missing_ok=True)
        if hist is None:
            fcache.unlink(missing_ok=True)
        else:
            fcache.write_text(json.dumps(hist), encoding="utf-8")
        r = g.run("test_flaky.py", "-n", "2", "--reruns", "2",
                  "--reruns-only-known-flaky", *extra, cwd=fdir,
                  env_extra={"FLAKY_MARKER": str(m)})
        m.unlink(missing_ok=True)
        return r

    # In history with flaky>0 -> rerun-eligible -> recovers. Use a current
    # epoch so retention aging (load() drops entries past the window) keeps it.
    now_epoch = int(time.time())
    r = faware({fnode: {"flaky": 2, "failed": 0, "last_epoch": now_epoch}}, "fa_known")
    check("flaky-aware: known-flaky test is reran and recovers",
          r.returncode == 0 and "1 flaky" in r.stdout, r.stdout[-200:])
    # No history -> not known-flaky -> not reran -> fails.
    r = faware(None, "fa_unknown")
    check("flaky-aware: unknown test not reran, fails",
          r.returncode == 1 and "1 failed" in r.stdout
          and "passed after rerun" not in r.stdout,
          r.stdout[-200:])
    # Hard-failure-only history (flaky==0) -> still not known-flaky: a
    # deterministic mass-failure recorded as `failed` never burns the budget.
    r = faware({fnode: {"flaky": 0, "failed": 9, "last_epoch": now_epoch}}, "fa_failedonly")
    check("flaky-aware: failed-only history does not count as known-flaky",
          r.returncode == 1 and "1 failed" in r.stdout, r.stdout[-200:])
    # Baseline sanity: same unknown test WITHOUT the flag reruns and recovers.
    m = g.tmp / "fa_baseline"
    m.unlink(missing_ok=True)
    fcache.unlink(missing_ok=True)
    r = g.run("test_flaky.py", "-n", "2", "--reruns", "2", cwd=fdir,
              env_extra={"FLAKY_MARKER": str(m)})
    m.unlink(missing_ok=True)
    check("flaky-aware: without the flag, unknown flake still recovers",
          r.returncode == 0 and "1 flaky" in r.stdout, r.stdout[-200:])

    # An explicit @pytest.mark.flaky declaration bypasses the gate even with
    # no history: the author already declared it flaky.
    g.write(
        "faware_mark/test_m.py",
        "import os, pathlib, pytest\n"
        "@pytest.mark.flaky(reruns=2)\n"
        "def test_marked():\n"
        "    m = pathlib.Path(os.environ['MK'])\n"
        "    if not m.exists():\n"
        "        m.write_text('x'); assert False\n"
        "    assert True\n",
    )
    mdir = g.tmp / "faware_mark"
    mk = g.tmp / "fa_mark"
    mk.unlink(missing_ok=True)
    # --reruns 2 so the gate is actually active (known_flaky loads); with no
    # history an unmarked test would be blocked, so recovery here proves the
    # @mark.flaky declaration bypasses the gate.
    r = g.run("test_m.py", "-n", "2", "--reruns", "2", "--reruns-only-known-flaky",
              cwd=mdir, env_extra={"MK": str(mk)})
    mk.unlink(missing_ok=True)
    check("flaky-aware: @mark.flaky bypasses the gate (no history)",
          r.returncode == 0 and "1 flaky" in r.stdout, r.stdout[-200:])

    g.write("crashflaky/test_cf.py", CRASHFLAKY)
    cmarker = g.tmp / "cf_marker"
    if cmarker.exists():
        cmarker.unlink()
    r = g.run("test_cf.py", "-n", "2", "--reruns", "1", cwd=g.tmp / "crashflaky",
              env_extra={"FLAKY_MARKER": str(cmarker)})
    check(
        "crashed test retried within budget",
        r.returncode == 0 and "1 flaky" in r.stdout and "2 passed" in r.stdout,
        r.stdout[-200:],
    )
    marker.unlink(missing_ok=True)
    fx = g.tmp / "flaky_junit.xml"
    g.run("test_flaky.py", "-n", "2", "--reruns", "2", "--junitxml", str(fx),
          cwd=fdir, env_extra={"FLAKY_MARKER": str(marker)})
    check(
        "flaky flagged in junit property",
        'property name="flaky"' in fx.read_text(encoding="utf-8"),
        fx.read_text(encoding="utf-8")[-300:],
    )
    # --output github: a flaky-passed test surfaces as a ::warning
    # annotation (the run is green, the flake is visible on the PR).
    marker.unlink(missing_ok=True)
    r = g.run("test_flaky.py", "-n", "2", "--reruns", "2", "--output", "github",
              cwd=fdir, env_extra={"FLAKY_MARKER": str(marker)})
    warns = [ln for ln in r.stdout.splitlines() if ln.startswith("::warning ")]
    check(
        "github: flaky-passed emits ::warning",
        r.returncode == 0
        and len(warns) == 1
        and "flaky" in warns[0]
        and "rerun" in warns[0]
        and "test_flaky.py" in warns[0]
        and not any(ln.startswith("::error ") for ln in r.stdout.splitlines()),
        r.stdout[-300:],
    )

    print("== quarantine ==")
    g.write(
        "quar/test_q.py",
        "def test_ok(): assert True\n\n"
        "def test_known_flake(): assert False, 'known flake'\n\n"
        "def test_real_bug(): assert 1 == 2\n",
    )
    qdir = g.tmp / "quar"
    g.write("quar/quarantine.txt", "# known flakes\ntest_q.py::test_known_flake\n")
    r = g.run(".", "-n", "2", "--quarantine", "quarantine.txt", cwd=qdir)
    check(
        "quarantine: listed failure demoted, unlisted still fails",
        r.returncode == 1
        and "1 failed, 1 passed, 1 quarantined" in r.stdout
        and "QUARANTINED test_q.py::test_known_flake" in r.stdout
        and "FAILED" in r.stdout
        and "QUARANTINED test_q.py::test_real_bug" not in r.stdout,
        f"rc={r.returncode} " + r.stdout[-400:],
    )
    g.write("quar/quarantine.txt", "test_q.py::*\n")
    qx = g.tmp / "quar_junit.xml"
    r = g.run(".", "-n", "2", "--quarantine", "quarantine.txt",
              "--junitxml", str(qx), cwd=qdir)
    jx = qx.read_text(encoding="utf-8")
    check(
        "quarantine: glob demotes all -> exit 0, junit green + flagged",
        r.returncode == 0
        and "2 quarantined" in r.stdout
        and 'failures="0"' in jx
        and jx.count('property name="quarantined"') == 2,
        f"rc={r.returncode} " + r.stdout[-200:],
    )
    flog = json.loads((qdir / ".rstest_cache" / "flakes.json").read_text(encoding="utf-8"))
    check(
        "flake history: failures recorded across runs",
        flog.get("test_q.py::test_real_bug", {}).get("failed", 0) >= 2,
        str(flog)[:300],
    )
    r = g.run(".", "-n", "2", "--quarantine", "quarantine.txt", cwd=qdir)
    check(
        "quarantine: history annotation in section",
        "failed 2x before" in r.stdout or "failed 3x before" in r.stdout,
        r.stdout[-400:],
    )

    print("== loadscope / loadgroup ==")
    g.write("scopes/test_sc_a.py", SCOPE_A)
    g.write("scopes/test_sc_b.py", SCOPE_B)
    g.write("scopes/test_sc_c.py", SCOPE_C)
    slog = g.tmp / "scope_log"
    for mode, label in (("loadscope", "class"), ("loadgroup", "group")):
        clear_e2e_log(slog)
        r = g.run("-n", "3", "--dist", mode, cwd=g.tmp / "scopes",
                  env_extra={"SLOG": str(slog)})
        rows = read_e2e_rows(slog)
        import collections
        by = collections.defaultdict(set)
        for x in rows:
            by[x["t"].split(".")[0]].add(x["w"])
        if mode == "loadscope":
            check(
                "loadscope: classes cohesive",
                len(by["alpha"]) == 1 and len(by["beta"]) == 1 and "12 passed" in r.stdout,
                str(dict(by)),
            )
        else:
            check(
                "loadgroup: cross-file group cohesive",
                len(by["grp"]) == 1 and "12 passed" in r.stdout,
                str(dict(by)),
            )

    print("== flaky marks / only-rerun ==")
    g.write("marks/test_marks.py", MARKS)
    mk = g.tmp / "marks_marker"
    cnt = g.tmp / "marks_count"
    for f in (mk, cnt):
        if f.exists():
            f.unlink()
    r = g.run("test_marks.py", "-n", "2", cwd=g.tmp / "marks",
              env_extra={"MK": str(mk), "CNT": str(cnt)})
    check(
        "flaky mark reruns without --reruns",
        "1 flaky" in r.stdout and "1 failed" in r.stdout,
        r.stdout[-200:],
    )
    check(
        "unmarked test not rerun by mark",
        cnt.read_text() == "1",
        cnt.read_text(),
    )
    mk.unlink()
    cnt.unlink()
    r = g.run("test_marks.py", "-n", "2", "--reruns", "2", "--only-rerun", "transient",
              cwd=g.tmp / "marks", env_extra={"MK": str(mk), "CNT": str(cnt)})
    check(
        "only-rerun gates non-matching failures",
        "1 flaky" in r.stdout and cnt.read_text() == "1",
        f"count={cnt.read_text()} " + r.stdout[-160:],
    )

    print("== worker-timeout watchdog ==")
    g.write("hang/test_hang.py", HANG)
    r = g.run("test_hang.py", "-n", "2", "--worker-timeout", "3",
              cwd=g.tmp / "hang", timeout=60)
    check(
        "hung test killed and attributed",
        r.returncode == 1
        and "exceeded --worker-timeout" in r.stdout
        and "2 passed" in r.stdout,
        r.stdout[-300:],
    )

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

    lines: "queue.Queue[str]" = queue.Queue()

    def _pump():
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

    print(f"\n{PASS} ok, {len(FAIL)} failed")
    if FAIL:
        print("FAILED:", ", ".join(FAIL))
        sys.exit(1)


LAZY_CONFTEST = """
import pytest

@pytest.fixture(scope="session")
def sess_counter(request):
    request.config._inits = getattr(request.config, "_inits", 0) + 1
    return request.config._inits
"""

LAZY_SESSION_A = """
def test_a1(sess_counter):
    assert sess_counter == 1

def test_a2(sess_counter):
    assert sess_counter == 1
"""

LAZY_SESSION_B = """
def test_b1(sess_counter):
    assert sess_counter == 1
"""

MP_SPAWN = """
import multiprocessing as mp

def _sq(x):
    return x * x

def test_spawn_pool():
    ctx = mp.get_context("spawn")
    with ctx.Pool(2) as pool:
        assert pool.map(_sq, [1, 2, 3]) == [1, 4, 9]

def test_spawn_process():
    ctx = mp.get_context("spawn")
    q = ctx.Queue()
    p = ctx.Process(target=q.put, args=(42,))
    p.start()
    assert q.get(timeout=30) == 42
    p.join()
"""

DISCO = """
import pytest


def test_one():
    assert True


def test_two():
    assert True


@pytest.mark.serial
def test_ser():
    assert True


@pytest.mark.parametrize("x", [1, 2])
def test_p(x):
    assert x
"""

BASIC = """
def test_passes():
    assert 1 + 1 == 2

def test_fails():
    assert 1 + 1 == 3, "math broke"

def test_error():
    raise RuntimeError("boom")

def test_also_passes():
    assert "abc".upper() == "ABC"
"""

# Hard-crash a worker mid-test, cross-platform: POSIX SIGKILL (uncatchable,
# instant), os._exit elsewhere (no SIGKILL on Windows). Either way the worker
# pipe closes abruptly -> the runner sees EOF and reports a crash.
HARD_CRASH = """
import os, signal


def _hard_crash():
    sig = getattr(signal, "SIGKILL", None)
    if sig is not None:
        os.kill(os.getpid(), sig)
    os._exit(137)
"""

CRASH = HARD_CRASH + """
def test_before_a(): assert True
def test_before_b(): assert True

def test_killer():
    _hard_crash()

def test_after_a(): assert True
def test_after_b(): assert True
def test_after_c(): assert True
"""

CRASHLOOP = HARD_CRASH + """
def test_k1(): _hard_crash()
def test_k2(): _hard_crash()
def test_k3(): _hard_crash()
def test_k4(): _hard_crash()
def test_k5(): _hard_crash()
def test_k6(): _hard_crash()
def test_ok(): assert True
"""

SERIAL = """
import json, os, time
import pytest

def _log(name):
    start = time.monotonic()
    time.sleep(0.15)
    # Per-worker file: cross-process append is not atomic on Windows, so a
    # shared log tears into blank/partial lines. One file per worker = one
    # writer per file = no contention.
    path = os.environ["RSTEST_E2E_LOG"] + "." + (os.environ.get("RSTEST_WORKER_ID") or "main")
    with open(path, "a") as f:
        f.write(json.dumps({
            "name": name,
            "worker": os.environ.get("RSTEST_WORKER_ID"),
            "start": start,
            "end": time.monotonic(),
        }) + "\\n")

def test_par_a(): _log("par_a")
def test_par_b(): _log("par_b")
def test_par_c(): _log("par_c")
def test_par_d(): _log("par_d")
def test_par_e(): _log("par_e")
def test_par_f(): _log("par_f")

@pytest.mark.serial
def test_serial_one(): _log("serial_one")

@pytest.mark.serial
def test_serial_two(): _log("serial_two")
"""

SECTIONS = """
def test_prints_and_fails():
    print("DEBUG: the database said no")
    assert False
"""

MAXFAIL = """
import time

def test_fail_fast():
    assert False

def test_s1(): time.sleep(0.3)
def test_s2(): time.sleep(0.3)
def test_s3(): time.sleep(0.3)
def test_s4(): time.sleep(0.3)
def test_s5(): time.sleep(0.3)
def test_s6(): time.sleep(0.3)
def test_s7(): time.sleep(0.3)
def test_s8(): time.sleep(0.3)
"""

LF = """
def test_ok_one(): assert True
def test_ok_two(): assert True
def test_flaky_not(): assert False
"""

NODECRASH_CONFTEST = '\nimport os\nimport uuid\n\nimport pytest\n\n\ndef pytest_addhooks(pluginmanager):\n    class XdistSpecs:\n        @pytest.hookspec\n        def pytest_configure_node(self, node): ...\n\n        @pytest.hookspec\n        def pytest_testnodedown(self, node, error): ...\n\n    pluginmanager.add_hookspecs(XdistSpecs)\n\n\ndef _log(line):\n    path = os.environ.get("NODE_HOOK_LOG")\n    if path:\n        with open(path + "." + str(os.getpid()), "a") as f:\n            f.write(line + "\\n")\n\n\nclass XDistHooks:\n    def pytest_configure_node(self, node):\n        ident = "res_%s_%s" % (node.gateway.id, uuid.uuid4().hex[:6])\n        node.workerinput["resource_ident"] = ident\n        _log("up:" + ident)\n\n    def pytest_testnodedown(self, node, error):\n        _log("down:" + node.workerinput["resource_ident"])\n\n\ndef pytest_configure(config):\n    config.pluginmanager.register(XDistHooks())\n'

NODECRASH_TEST = HARD_CRASH + '\nimport time\n\n\ndef test_a(): time.sleep(0.05)\ndef test_b(): time.sleep(0.05)\n\n\ndef test_killer():\n    _hard_crash()\n\n\ndef test_c(): time.sleep(0.05)\ndef test_d(): time.sleep(0.05)\ndef test_e(): time.sleep(0.05)\n'

NODEHOOKS_CONFTEST = '\nimport os\n\nimport pytest\n\n\ndef pytest_addhooks(pluginmanager):\n    # Real suites get these specs from pytest-xdist; declare them the\n    # same way so this fixture is hermetic.\n    class XdistSpecs:\n        @pytest.hookspec\n        def pytest_configure_node(self, node): ...\n\n        @pytest.hookspec\n        def pytest_testnodeready(self, node): ...\n\n        @pytest.hookspec\n        def pytest_testnodedown(self, node, error): ...\n\n    pluginmanager.add_hookspecs(XdistSpecs)\n\n\ndef _log(line):\n    path = os.environ.get("NODE_HOOK_LOG")\n    if path:\n        with open(path + "." + str(os.getpid()), "a") as f:\n            f.write(line + "\\n")\n\n\nclass XDistHooks:\n    # the sqlalchemy pattern: master fills workerinput per node\n    def pytest_configure_node(self, node):\n        node.workerinput["follower_ident"] = "follower_" + node.gateway.id\n\n    def pytest_testnodeready(self, node):\n        _log("ready:" + node.gateway.id)\n\n    def pytest_testnodedown(self, node, error):\n        _log("down:" + node.workerinput["follower_ident"])\n\n\ndef pytest_configure(config):\n    config.pluginmanager.register(XDistHooks())\n    # read it back IMMEDIATELY (sqlalchemy does exactly this): only a\n    # synchronous configure_node call at registration time satisfies it\n    if hasattr(config, "workerinput"):\n        config._follower_ident = config.workerinput["follower_ident"]\n'

NODEHOOKS_TEST = '\ndef test_ident_present(request):\n    assert request.config._follower_ident.startswith("follower_gw")\n\n\ndef test_workerinput_kept(request):\n    assert request.config.workerinput["follower_ident"] == request.config._follower_ident\n'

# pytest-html (via pytest-metadata) ships the one-arg form
# `pytest_testnodedown(node)`. xdist's hookspec is (node, error); rstest must
# call it without `error=` or it TypeErrors and the HTML report is lost.
NODEONEARG_CONFTEST = '''
import os

import pytest


def pytest_addhooks(pluginmanager):
    class XdistSpecs:
        @pytest.hookspec
        def pytest_configure_node(self, node): ...

        @pytest.hookspec
        def pytest_testnodedown(self, node, error): ...

    pluginmanager.add_hookspecs(XdistSpecs)


def _log(line):
    path = os.environ.get("NODE_HOOK_LOG")
    if path:
        with open(path + "." + str(os.getpid()), "a") as f:
            f.write(line + "\\n")


class OneArgHooks:
    def pytest_configure_node(self, node):
        node.workerinput["oa_ident"] = "oa_" + node.gateway.id

    # ONE arg, no `error` — the pytest-html / pytest-metadata signature.
    def pytest_testnodedown(self, node):
        _log("down:" + node.workerinput["oa_ident"])


def pytest_configure(config):
    config.pluginmanager.register(OneArgHooks())
'''

DURATIONS_FIXTURE = """
import time

def test_sleepy(): time.sleep(0.3)
def test_quick(): pass
"""

DOCTEST_MOD = "def add(a, b):\n    '''\n    >>> add(2, 3)\n    5\n    '''\n    return a + b\n\ndef sub(a, b):\n    '''\n    >>> sub(5, 3)\n    1\n    '''\n    return a - b\n"

WARN = """
import warnings

def test_warns():
    warnings.warn("noisy thing", UserWarning)

def test_clean():
    assert True
"""

FLAKY = """
import os
import pathlib


def test_flaky_once():
    marker = pathlib.Path(os.environ["FLAKY_MARKER"])
    if not marker.exists():
        marker.write_text("attempted")
        assert False, "first attempt fails"
    assert True


def test_stable():
    assert True
"""

CRASHFLAKY = HARD_CRASH + """
import pathlib


def test_crashes_once():
    marker = pathlib.Path(os.environ["FLAKY_MARKER"])
    if not marker.exists():
        marker.write_text("crashed")
        _hard_crash()
    assert True


def test_other():
    assert True
"""

SCOPE_A = """
import json
import os


def _log(name):
    w = os.environ["RSTEST_WORKER_ID"]
    with open(os.environ["SLOG"] + "." + str(os.getpid()), "a") as f:
        f.write(json.dumps({"t": name, "w": w}) + "\\n")


class TestAlpha:
    def test_a1(self): _log("alpha.a1")
    def test_a2(self): _log("alpha.a2")
    def test_a3(self): _log("alpha.a3")


class TestBeta:
    def test_b1(self): _log("beta.b1")
    def test_b2(self): _log("beta.b2")
    def test_b3(self): _log("beta.b3")
"""

SCOPE_B = """
import json
import os

import pytest


def _log(name):
    w = os.environ["RSTEST_WORKER_ID"]
    with open(os.environ["SLOG"] + "." + str(os.getpid()), "a") as f:
        f.write(json.dumps({"t": name, "w": w}) + "\\n")


@pytest.mark.xdist_group("dbpool")
def test_g1(): _log("grp.g1")


def test_free1(): _log("free.1")
def test_free2(): _log("free.2")


@pytest.mark.xdist_group("dbpool")
def test_g2(): _log("grp.g2")
"""

SCOPE_C = """
import json
import os

import pytest


def _log(name):
    w = os.environ["RSTEST_WORKER_ID"]
    with open(os.environ["SLOG"] + "." + str(os.getpid()), "a") as f:
        f.write(json.dumps({"t": name, "w": w}) + "\\n")


@pytest.mark.xdist_group("dbpool")
def test_g3(): _log("grp.g3")


def test_free3(): _log("free.3")
"""

MARKS = """
import os
import pathlib

import pytest


@pytest.mark.flaky(reruns=2)
def test_marked_flaky():
    marker = pathlib.Path(os.environ["MK"])
    if not marker.exists():
        marker.write_text("x")
        assert False, "transient blip"
    assert True


def test_unmarked_fails():
    cnt = pathlib.Path(os.environ["CNT"])
    n = int(cnt.read_text()) if cnt.exists() else 0
    cnt.write_text(str(n + 1))
    assert False, "permanent failure"


def test_ok():
    assert True
"""

HANG = """
import time


def test_quick_a():
    assert True


def test_hangs_forever():
    time.sleep(600)


def test_quick_b():
    assert True
"""

DOCTOR = """
import time

def test_sleepy():
    time.sleep(1.2)

def test_quick():
    assert True
"""


if __name__ == "__main__":
    main()
