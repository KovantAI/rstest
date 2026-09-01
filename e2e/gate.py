#!/usr/bin/env python3
"""rstest test gate: end-to-end assertions for every shipped behavior.

Hermetic: builds its own venv (worker runtime deps) and fixture suites.
Usage: python3 e2e/gate.py [--binary target/release/rstest]

Exit 0 = all gates green. Designed to be the single CI entry point.
"""

import argparse
import glob
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import xml.etree.ElementTree as ET
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
E2E = REPO / "e2e"
FIXTURES = E2E / "fixtures"

WINDOWS = os.name == "nt"


def fx(name: str) -> str:
    """Load a fixture suite from e2e/fixtures/ (real .py files, not inlined
    string blobs). The gate writes these into per-test tmp dirs via g.write."""
    return (FIXTURES / name).read_text(encoding="utf-8")


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
            # rstest emits UTF-8 glyphs (✓ ✗ ─); pin the decode to UTF-8 so
            # the locale encoding (cp1252 on Windows) does not mangle them
            # and make `"✓" in r.stdout` spuriously False.
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
    if sys.version_info >= (3, 10):  # noqa: UP036 — launcher may run under older python
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
    fail json.loads - exactly the regression this guards against."""
    objs, ok = [], True
    for ln in text.splitlines():
        if not ln.strip():
            continue
        try:
            objs.append(json.loads(ln))
        except Exception:
            ok = False
    return ok, objs


def gate_basics(g, args, binary):
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


def gate_collection_error_semantics(g, args, binary):
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


def gate_output_styles(g, args, binary):
    print("== output styles ==")
    g.write("basic/test_basic.py", BASIC)  # self-contained (also written by gate_basics)
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
    # inline only - the batched "--- FAILED ---" block must NOT also print
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

    # --output azure: human log PLUS an Azure Pipelines `##vso[task.logissue]`
    # command per failing test (rendered as inline issues on the PR).
    r = g.run("basic/test_basic.py", "-n", "2", "--output", "azure")
    az = [ln for ln in r.stdout.splitlines() if ln.startswith("##vso[task.logissue ")]
    az_ok = (
        "2 passed" in r.stdout
        and "2 failed" in r.stdout  # human summary intact
        and sum("type=error" in ln for ln in az) == 2  # one per failed test
        and all("sourcepath=" in ln for ln in az if "type=error" in ln)
        and any("test_basic.py" in ln for ln in az)
    )
    check("output azure: human log + logissue per failure", az_ok, r.stdout[-400:])

    # --output json: stdout is PURE NDJSON - no banner, every line parses,
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

    # --output tap: pure TAP stream - version header, one point per test,
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


def gate_multiprocessing_spawn_children(g, args, binary):
    print("== multiprocessing-spawn children ==")
    # spawn-mode children re-import the worker's __main__ as __mp_main__
    # (runpy, no package context): the worker entry must import absolutely,
    # guard main(), and keep its sys.path bootstrap idempotent. anyio's
    # to_process exercises the same protocol.
    g.write("mpspawn/test_mpspawn.py", MP_SPAWN)
    r = g.run("mpspawn", "-n", "2")
    check("mp-spawn under pool", "2 passed" in r.stdout, r.stdout[-300:])


def gate_crash_handling(g, args, binary):
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


def gate_report_json_contract(g, args, binary):
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


def gate_collect_only_discovery_json(g, args, binary):
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


def gate_pytest_randomly_real_plugin(g, args, binary):
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
    check(
        "randomly: consumes synthesized seed, no crash at -n 2",
        "4 passed" in r.stdout,
        r.stdout[-400:],
    )
    r = gr.run("rnd", "-n", "2", env_extra={"RSTEST_RUN_UID": uid})
    check("randomly: reproducible seed with pinned uid", "4 passed" in r.stdout, r.stdout[-400:])


def gate_pytest_rerunfailures_xdist_no_sock_port_(g, args, binary):
    print("== pytest-rerunfailures + xdist (no sock_port KeyError) ==")
    # rerunfailures+xdist reads workerinput["sock_port"], a key only an xdist
    # master sets; with no master rstest KeyError'd at -n>=2. rstest now drops
    # the plugin in pytest_cmdline_main and owns reruns. xdist reproduces it.
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


def gate_pytest_retry_xdist_server_port_self_prov(g, args, binary):
    print("== pytest-retry + xdist (server_port self-provision) ==")
    # pytest-retry gates master vs worker on xdist + numprocesses; rstest keeps
    # numprocesses visible so each worker self-provisions a ReportServer (master
    # branch) and never reads the sourceless workerinput["server_port"].
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
        # branch - the direct evidence that the KeyError path is never taken.
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


def gate_interpreter_probe_cache_heals_after_deps(g, args, binary):
    print("== interpreter probe cache (heals after deps installed) ==")
    # Regression: a NEGATIVE probe (interpreter present but worker shim not
    # importable, e.g. msgpack missing) must NOT be cached. The cache keys on
    # binary mtime, unchanged by pip install, so a cached false would persist.
    bare = g.tmp / "bareenv"
    shutil.rmtree(bare, ignore_errors=True)
    subprocess.run([find_python(), "-m", "venv", str(bare)], check=True)  # no deps -> no msgpack
    barepy = venv_bin(bare, "python")
    probe_cache = g.tmp / "probecache"
    shutil.rmtree(probe_cache, ignore_errors=True)
    g.write("probe/test_p.py", "def test_ok(): assert True\n")
    # 1) msgpack absent: the interpreter runs but can't host a worker, so the
    #    probe is negative and (with the fix) is not written to the cache.
    r = g.run("probe", "--python", str(barepy), env_extra={"RSTEST_CACHE_DIR": str(probe_cache)})
    check(
        "probe: msgpack-less interpreter rejected",
        r.returncode != 0 and ("worker shim" in r.stderr or "no usable" in r.stderr.lower()),
        f"rc={r.returncode} " + r.stderr[-200:],
    )
    # 2) install the worker deps into the SAME interpreter (binary mtime
    #    unchanged), then rerun with the SAME cache dir: a stale cached negative
    #    would still reject it; the fix re-probes negatives so it now succeeds.
    subprocess.run([str(venv_bin(bare, "pip")), "install", "-q", *BASE_DEPS], check=True)
    r = g.run("probe", "--python", str(barepy), env_extra={"RSTEST_CACHE_DIR": str(probe_cache)})
    check(
        "probe: heals after deps installed (negative not cached)",
        r.returncode == 0 and "1 passed" in r.stdout,
        f"rc={r.returncode} " + r.stderr[-200:] + " || " + r.stdout[-200:],
    )


def gate_lazy_collection(g, args, binary):
    print("== lazy collection ==")
    # D5 single-point collection: same fixtures, same outcomes, no
    # initial collection pass in any worker.
    r = g.run("basic/test_basic.py", "-n", "2", "--collect", "lazy")
    check("lazy: parallel counts", "2 failed, 2 passed" in r.stdout, r.stdout[-200:])
    check("lazy: exit 1", r.returncode == 1)
    r = g.run("basic", "-n", "2", "--collect", "lazy", "-k", "passes")
    check(
        "lazy: -k filters per file",
        "2 passed" in r.stdout and "failed" not in r.stdout,
        r.stdout[-200:],
    )
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
    r = g.run(
        "test_flaky.py",
        "-n",
        "2",
        "--collect",
        "lazy",
        "--reruns",
        "2",
        cwd=fdir,
        env_extra={"FLAKY_MARKER": str(marker)},
    )
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
        s["start"] < o["end"] and o["start"] < s["end"] for s in lser for o in rows if o is not s
    )
    check(
        "lazy: serial exclusive",
        not overlap and len({s["worker"] for s in lser}) == 1,
    )


def gate_serial_mark(g, args, binary):
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
        s["start"] < o["end"] and o["start"] < s["end"] for s in serial for o in rows if o is not s
    )
    check("serial exclusive", not overlap and len({s["worker"] for s in serial}) == 1)
    check(
        "serial after parallel",
        min(s["start"] for s in serial) >= max(p["end"] for p in par),
    )


def gate_failure_output(g, args, binary):
    print("== failure output ==")
    g.write("sections/test_sections.py", SECTIONS)
    r = g.run("sections", "-n", "2")
    check(
        "captured stdout section",
        "Captured stdout call" in r.stdout and "the database said no" in r.stdout,
    )


def gate_x_maxfail(g, args, binary):
    print("== -x / --maxfail ==")
    g.write("maxfail/test_maxfail.py", MAXFAIL)
    r = g.run("maxfail", "-n", "2", "-x", timeout=60)
    full = g.run("maxfail", "-n", "2", timeout=60)
    ran_x = int(r.stdout.split(" passed")[0].rsplit(" ", 1)[-1]) if " passed" in r.stdout else 0
    check("-x stops early", "1 failed" in r.stdout and ran_x < 8, r.stdout[-120:])
    check("full run unaffected", "8 passed" in full.stdout, full.stdout[-120:])


def gate_lf(g, args, binary):
    print("== --lf ==")
    lf = g.tmp / "lf"
    shutil.rmtree(lf / ".pytest_cache", ignore_errors=True)
    g.write("lf/test_lf.py", LF)
    g.run("test_lf.py", "-n", "2", cwd=lf)
    r = g.run("test_lf.py", "-n", "2", "--lf", cwd=lf)
    check(
        "--lf reruns only failures",
        "1 failed" in r.stdout and "passed" not in r.stdout,
        r.stdout[-200:],
    )


def gate_junitxml(g, args, binary):
    print("== junitxml ==")
    xml_path = g.tmp / "junit.xml"
    g.run("maxfail", "-n", "2", "--junitxml", str(xml_path), timeout=60)
    ts = ET.parse(xml_path).getroot().find("testsuite")
    check(
        "junit counts",
        ts is not None and ts.get("tests") == "9" and ts.get("failures") == "1",
        str(dict(ts.attrib) if ts is not None else None),
    )


def gate_shard_k_n(g, args, binary):
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
        g.run(
            ".", "-n", "2", "--shard", f"{k}/{n}", "--junitxml", str(xp), *extra, cwd=str(shard_cwd)
        )
        root = ET.parse(xp).getroot()
        return {(tc.get("classname"), tc.get("name")) for tc in root.iter("testcase")}

    s1, s2 = shard_ids(1, 2), shard_ids(2, 2)
    check("shard: buckets disjoint", s1.isdisjoint(s2), f"overlap={s1 & s2}")
    check("shard: buckets cover the suite", len(s1 | s2) == 8, f"union={len(s1 | s2)}")
    check("shard: no empty bucket", bool(s1) and bool(s2), f"sizes={len(s1)},{len(s2)}")
    r = g.run("shardsuite", "-n", "2", "--shard", "5/4")
    check("shard: K>N rejected", r.returncode != 0 and "1..=4" in r.stderr, r.stderr[-160:])
    r = g.run("shardsuite", "-n", "2", "--shard", "1/2", "--shuffle")
    check(
        "shard: +shuffle rejected",
        r.returncode != 0 and "not supported with --shuffle" in r.stderr,
        r.stderr[-160:],
    )
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
    split = list(set(f1) & set(f2))
    check("shard loadfile: no file split across shards", not split, f"split files={split}")
    all_files = set(f1) | set(f2)
    check("shard loadfile: buckets cover both files", len(all_files) == 2, f"files={all_files}")


def gate_dist_each(g, args, binary):
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
    check(
        "each: --reruns rejected",
        r.returncode != 0 and "not supported" in r.stderr,
        r.stderr[-200:],
    )


def gate_dist_validation(g, args, binary):
    print("== --dist validation ==")
    # An invalid --dist value must be rejected the same way on every path;
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


def gate_testnodedown_for_crashed_workers(g, args, binary):
    print("== testnodedown for crashed workers ==")
    g.write("nodecrash/conftest.py", NODECRASH_CONFTEST)
    g.write("nodecrash/test_crashy.py", NODECRASH_TEST)
    crash_log = g.tmp / "node_crash.log"
    clear_hook_log(crash_log)
    r = g.run("nodecrash", "-n", "2", env_extra={"NODE_HOOK_LOG": str(crash_log)})
    text = read_hook_log(crash_log)
    ups = {line.split(":", 1)[1] for line in text.splitlines() if line.startswith("up:")}
    downs = {line.split(":", 1)[1] for line in text.splitlines() if line.startswith("down:")}
    check(
        "every provisioned ident torn down (incl. crashed worker's)",
        ups and ups == downs,
        f"ups={sorted(ups)} downs={sorted(downs)}\n" + r.stdout[-200:],
    )
    check("crash still attributed", "crashed while running" in r.stdout, r.stdout[-300:])


def gate_xdist_master_side_hooks(g, args, binary):
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


def gate_one_arg_pytest_testnodedown(g, args, binary):
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


def gate_durations(g, args, binary):
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


def gate_doctest_modules(g, args, binary):
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


def gate_monorepo(g, args, binary):
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
    git_init_commit(cm, "base")
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
    git_init_commit(cs, "base")
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
    git(cs, "add", "-A")
    git_commit(cs, "x")
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
    git_init_commit(sp, "base")
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


def gate_warnings(g, args, binary):
    print("== warnings ==")
    g.write("warn/test_warn.py", WARN)
    r = g.run("warn", "-n", "2")
    check("warnings summary section", "warnings summary" in r.stdout and "UserWarning" in r.stdout)
    check("warnings in counts", "warnings in" in r.stdout, r.stdout[-120:])


def gate_doctor(g, args, binary):
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
        "doc",
        "-n",
        "2",
        "--doctor-md",
        str(dm),
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
        r.returncode == 1
        and ok
        and "doctor gate failures" not in r.stdout
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
    # never firing - the exact dead-gate bug this feature exists to kill.
    r = g.run("doc", "-n", "2", "--doctor-fail-on", "bogus<1")
    check(
        "doctor-fail-on: bad metric aborts loudly",
        r.returncode != 0 and "unknown metric" in r.stderr,
        f"rc={r.returncode} " + r.stderr[-200:],
    )


def gate_auto_worker_capping(g, args, binary):
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


def gate_coverage(g, args, binary):
    print("== coverage ==")
    g.write(
        "cov/mypkg/__init__.py",
        "def used(x):\n    return x * 2\n\n\ndef unused(x):\n    return x - 1\n",
    )
    g.write(
        "cov/test_cov.py", "from mypkg import used\n\n\ndef test_used():\n    assert used(2) == 4\n"
    )
    covdir = g.tmp / "cov"
    r = g.run(
        "test_cov.py",
        "-n",
        "2",
        "--cov=mypkg",
        "--cov-report=term",
        cwd=covdir,
        env_extra={"PYTHONPATH": str(covdir)},
    )
    check(
        "coverage report under pool",
        "mypkg" in r.stdout and "__init__.py" in r.stdout and "%" in r.stdout,
        r.stdout[-300:],
    )
    r = g.run(
        "test_cov.py",
        "-n",
        "2",
        "--cov=mypkg",
        "--cov-fail-under=99",
        cwd=covdir,
        env_extra={"PYTHONPATH": str(covdir)},
    )
    check(
        "coverage fail-under exits 1",
        r.returncode == 1 and "FAIL Required" in r.stdout,
        r.stdout[-200:],
    )


def gate_coverage_contexts_line_test_index_cov_co(g, args, binary):
    print("== coverage contexts + line->test index (--cov-context) ==")
    # Per-test contexts must survive the PARALLEL merge (tests land on different
    # workers, yet each covered line keeps its context), and --cov-context must
    # emit the line->test index that coverage-based --changed consumes.
    ctxdir = g.tmp / "covctx"
    g.write(
        "covctx/mymod.py",
        "def used_by_a():\n    return 1\n"
        "def used_by_b():\n    return 2\n"
        "def used_by_both():\n    return 3\n",
    )
    g.write(
        "covctx/test_a.py",
        "import mymod\n"
        "def test_a():\n    assert mymod.used_by_a() == 1\n"
        "    assert mymod.used_by_both() == 3\n",
    )
    g.write(
        "covctx/test_b.py",
        "import mymod\n"
        "def test_b():\n    assert mymod.used_by_b() == 2\n"
        "    assert mymod.used_by_both() == 3\n",
    )
    shutil.rmtree(ctxdir / ".rstest_cache", ignore_errors=True)
    r = g.run(
        "test_a.py",
        "test_b.py",
        "-n",
        "2",
        "--cov=mymod",
        "--cov-context=test",
        "--cov-report=",
        cwd=ctxdir,
        env_extra={"PYTHONPATH": str(ctxdir)},
    )
    idx_path = ctxdir / ".rstest_cache" / "coverage_index.json"
    check(
        "cov-context: line->test index written",
        r.returncode == 0 and idx_path.exists(),
        f"rc={r.returncode} " + r.stdout[-200:],
    )
    if idx_path.exists():
        idx = json.loads(idx_path.read_text())
        fm = idx.get("files", {}).get("mymod.py", {})
        # schema 2 nests the line map under "lines" and stamps a source "hash".
        lm = fm.get("lines", {})
        # used_by_a body (line 2) only test_a; used_by_b (line 4) only test_b;
        # used_by_both (line 6) BOTH - proving cross-worker context merge.
        check(
            "cov-context: schema + per-test line mapping",
            idx.get("schema") == 2
            and isinstance(fm.get("hash"), str)
            and len(fm["hash"]) == 64
            and lm.get("2") == ["test_a.py::test_a"]
            and lm.get("4") == ["test_b.py::test_b"]
            and lm.get("6") == ["test_a.py::test_a", "test_b.py::test_b"],
            json.dumps(fm),
        )
    # Without --cov-context, no index is written (feature is opt-in via the flag).
    shutil.rmtree(ctxdir / ".rstest_cache", ignore_errors=True)
    r = g.run(
        "test_a.py",
        "test_b.py",
        "-n",
        "2",
        "--cov=mymod",
        "--cov-report=",
        cwd=ctxdir,
        env_extra={"PYTHONPATH": str(ctxdir)},
    )
    check(
        "cov-context: no index without the flag",
        r.returncode == 0 and not idx_path.exists(),
        f"rc={r.returncode}",
    )


def gate_smart_selection(g, args, binary):
    print("== smart selection ==")
    sp = g.tmp / "selproj"
    g.write("selproj/pkg/__init__.py", "")
    g.write("selproj/pkg/a.py", "def alpha():\n    return 1\n")
    g.write("selproj/pkg/b.py", "def beta():\n    return 2\n")
    g.write("selproj/pkg/c.py", "from .a import alpha\n\n\ndef gamma():\n    return alpha() + 1\n")
    g.write(
        "selproj/tests/test_a.py",
        "from pkg.a import alpha\n\ndef test_alpha(): assert alpha() == 1\n",
    )
    g.write(
        "selproj/tests/test_b.py", "from pkg.b import beta\n\ndef test_beta(): assert beta() == 2\n"
    )
    g.write(
        "selproj/tests/test_c.py",
        "from pkg.c import gamma\n\ndef test_gamma(): assert gamma() == 2\n",
    )
    g.write("selproj/pyproject.toml", '[tool.pytest.ini_options]\ntestpaths = ["tests"]\n')
    git_init_commit(sp, "init")
    with open(sp / "pkg" / "a.py", "a") as f:
        f.write("# touched\n")
    r = g.run("--changed", "-v", cwd=sp, env_extra={"PYTHONPATH": str(sp)})
    check(
        "selection: direct + transitive, excludes unrelated",
        "2 affected test target(s)" in r.stderr
        and "test_alpha" in r.stdout
        and "test_gamma" in r.stdout
        and "test_beta" not in r.stdout,
        r.stderr[-200:] + r.stdout[-200:],
    )
    g.write("selproj/pytest.ini", "[pytest]\n")
    r = g.run("--changed", cwd=sp, env_extra={"PYTHONPATH": str(sp)})
    check(
        "selection: config change -> full run",
        "falling back to full run" in r.stderr,
        r.stderr[-200:],
    )
    (sp / "pytest.ini").unlink()
    git(sp, "add", "-A")
    git_commit(sp, "w")
    r = g.run("--changed", cwd=sp, env_extra={"PYTHONPATH": str(sp)})
    check(
        "selection: clean tree -> nothing",
        r.returncode == 0 and "no tests affected" in r.stdout,
        r.stdout[-200:],
    )
    g.write("selproj/tests/conftest.py", "import pytest\n")
    r = g.run("--changed", "-v", cwd=sp, env_extra={"PYTHONPATH": str(sp)})
    check(
        "selection: conftest -> whole subtree",
        "3 affected test target(s)" in r.stderr and "test_beta" in r.stdout,
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

    # PR-aware --changed: with GITHUB_BASE_REF set, bare --changed diffs vs
    # the merge-base with origin/<base> - a clean checkout of a PR commit
    # still selects the PR's files (vs HEAD it would select nothing).
    base_sha = subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=sp, capture_output=True, text=True, check=True
    ).stdout.strip()
    subprocess.run(
        ["git", "update-ref", "refs/remotes/origin/mainline", base_sha], cwd=sp, check=True
    )
    with open(sp / "pkg" / "a.py", "a") as f:
        f.write("# pr change\n")
    git(sp, "add", "-A")
    git_commit(sp, "pr")
    r = g.run(
        "--changed",
        "-v",
        cwd=sp,
        env_extra={"PYTHONPATH": str(sp), "GITHUB_BASE_REF": "mainline"},
    )
    check(
        "selection: GITHUB_BASE_REF auto-targets PR base",
        "auto-targets PR base origin/mainline" in r.stderr
        and "2 affected test target(s)" in r.stderr
        and "test_alpha" in r.stdout
        and "test_beta" not in r.stdout,
        r.stderr[-300:],
    )
    r = g.run(
        "--changed",
        cwd=sp,
        env_extra={"PYTHONPATH": str(sp), "GITHUB_BASE_REF": "nosuchbranch"},
    )
    check(
        "selection: missing PR base ref errors, no silent skip",
        r.returncode != 0 and "fetch the base branch" in r.stderr,
        f"rc={r.returncode} " + r.stderr[-300:],
    )
    r = g.run(
        "--changed=HEAD~1",
        cwd=sp,
        env_extra={"PYTHONPATH": str(sp), "GITHUB_BASE_REF": "mainline"},
    )
    check(
        "selection: explicit rev wins over GITHUB_BASE_REF",
        "auto-targets" not in r.stderr and "2 affected test target(s)" in r.stderr,
        r.stderr[-300:],
    )
    # Buildkite exposes the base as a branch name, resolved the same way.
    r = g.run(
        "--changed",
        "-v",
        cwd=sp,
        env_extra={"PYTHONPATH": str(sp), "BUILDKITE_PULL_REQUEST_BASE_BRANCH": "mainline"},
    )
    check(
        "selection: Buildkite base branch auto-targets PR base",
        "auto-targets PR base origin/mainline" in r.stderr and "test_alpha" in r.stdout,
        r.stderr[-300:],
    )
    # GitLab provides the exact diff-base SHA - used directly, no merge-base.
    r = g.run(
        "--changed",
        "-v",
        cwd=sp,
        env_extra={"PYTHONPATH": str(sp), "CI_MERGE_REQUEST_DIFF_BASE_SHA": base_sha},
    )
    check(
        "selection: GitLab diff-base SHA auto-targets MR base",
        "auto-targets MR base" in r.stderr and "test_alpha" in r.stdout,
        r.stderr[-300:],
    )
    # An unresolvable GitLab base SHA errors - never a silent full skip.
    r = g.run(
        "--changed",
        cwd=sp,
        env_extra={"PYTHONPATH": str(sp), "CI_MERGE_REQUEST_DIFF_BASE_SHA": "0" * 40},
    )
    check(
        "selection: missing GitLab base SHA errors, no silent skip",
        r.returncode != 0 and "not in the local clone" in r.stderr,
        f"rc={r.returncode} " + r.stderr[-300:],
    )


def gate_coverage_based_selection_changed_uses_th(g, args, binary):
    print("== coverage-based selection (--changed uses the cov index) ==")
    # Warm a line->test index, then prove --changed narrows to only the tests
    # whose recorded coverage hit the changed lines - tighter than the
    # import-graph (which would run every test importing the module).
    cs = g.tmp / "covsel"
    g.write(
        "covsel/mymod.py",
        "def used_by_a():\n    return 1\n"
        "def used_by_b():\n    return 2\n"
        "def used_by_both():\n    return 3\n",
    )
    g.write(
        "covsel/test_a.py",
        "import mymod\n"
        "def test_a():\n    assert mymod.used_by_a() == 1\n"
        "    assert mymod.used_by_both() == 3\n",
    )
    g.write(
        "covsel/test_b.py",
        "import mymod\n"
        "def test_b():\n    assert mymod.used_by_b() == 2\n"
        "    assert mymod.used_by_both() == 3\n",
    )
    g.write("covsel/pyproject.toml", "[tool.pytest.ini_options]\n")
    git_init_commit(cs, "init")

    def cov_changed_targets(edit_fn):
        # reset, apply edit to the working tree, list the selected nodeids
        git(cs, "checkout", "-q", ".")
        edit_fn()
        r = g.run("-n", "2", "--changed", "--co", "-q", cwd=cs, env_extra={"PYTHONPATH": str(cs)})
        got = sorted(set(re.findall(r"test_[ab]\.py::test_[ab]", r.stdout)))
        return got, r

    def edit_line(path, old, new):
        p = cs / path
        p.write_text(p.read_text().replace(old, new), encoding="utf-8")

    # Warm the index (writes covsel/.rstest_cache/coverage_index.json).
    r = g.run(
        "-n",
        "2",
        "--cov=mymod",
        "--cov-context=test",
        "--cov-report=",
        cwd=cs,
        env_extra={"PYTHONPATH": str(cs)},
    )
    check(
        "cov-select: index warmed",
        (cs / ".rstest_cache" / "coverage_index.json").exists(),
        r.stdout[-200:] + r.stderr[-200:],
    )

    got, r = cov_changed_targets(
        lambda: edit_line("mymod.py", "    return 1\n", "    return 111\n")
    )
    check(
        "cov-select: edit used_by_a -> only test_a",
        got == ["test_a.py::test_a"],
        f"{got} || {r.stderr[-150:]}",
    )

    got, _ = cov_changed_targets(lambda: edit_line("mymod.py", "    return 2\n", "    return 22\n"))
    check("cov-select: edit used_by_b -> only test_b", got == ["test_b.py::test_b"], str(got))

    got, _ = cov_changed_targets(lambda: edit_line("mymod.py", "    return 3\n", "    return 33\n"))
    check(
        "cov-select: edit shared line -> both tests",
        got == ["test_a.py::test_a", "test_b.py::test_b"],
        str(got),
    )

    # Rail: a pure insertion (new function) has no old-side coverage -> falls
    # back to import-graph, which runs every test importing the module.
    got, _ = cov_changed_targets(
        lambda: (cs / "mymod.py").write_text(
            (cs / "mymod.py").read_text() + "\ndef brand_new():\n    return 9\n", encoding="utf-8"
        )
    )
    check(
        "cov-select: new code falls back to import-graph (both)",
        got == ["test_a.py::test_a", "test_b.py::test_b"],
        str(got),
    )

    # Rail: a changed test file always runs its own tests.
    got, _ = cov_changed_targets(lambda: edit_line("test_a.py", "== 1\n", "== 1  # x\n"))
    check("cov-select: changed test file runs itself", got == ["test_a.py::test_a"], str(got))

    # Rail: a `def` line runs at import time under the empty context, so it is
    # never in the index. Trusting the empty lookup would select ZERO tests;
    # instead the file must fall back to import-graph (both tests).
    got, _ = cov_changed_targets(
        lambda: edit_line("mymod.py", "def used_by_a():\n", "def used_by_a(x=1):\n")
    )
    check(
        "cov-select: edited def line falls back to import-graph (both)",
        got == ["test_a.py::test_a", "test_b.py::test_b"],
        str(got),
    )

    # Rail: cold cache (no index) is identical to import-graph selection.
    shutil.rmtree(cs / ".rstest_cache", ignore_errors=True)
    got, _ = cov_changed_targets(
        lambda: edit_line("mymod.py", "    return 1\n", "    return 111\n")
    )
    check(
        "cov-select: cold cache -> import-graph (both)",
        got == ["test_a.py::test_a", "test_b.py::test_b"],
        str(got),
    )

    # Rail: a warm index may hold a nodeid for a test renamed since it was built.
    # Passing the stale nodeid aborts the run, and dropping it skips a test still
    # covering the changed line, so the file demotes to import-graph instead.
    git(cs, "checkout", "-q", ".")
    git(cs, "clean", "-fdq")
    g.run(
        "-n",
        "2",
        "--cov=mymod",
        "--cov-context=test",
        "--cov-report=",
        cwd=cs,
        env_extra={"PYTHONPATH": str(cs)},
    )  # warm index (knows test_a.py::test_a)
    git(cs, "mv", "test_a.py", "test_renamed.py")
    edit_line("mymod.py", "    return 1\n", "    return 111\n")  # line only test_a covered
    r = g.run("-n", "2", "--changed", "--co", "-q", cwd=cs, env_extra={"PYTHONPATH": str(cs)})
    got = sorted(set(re.findall(r"test_\w+\.py::test_\w+", r.stdout)))
    check(
        "cov-select: renamed test (stale nodeid) -> no crash, runs via fallback",
        r.returncode == 0 and "test_renamed.py::test_a" in got and "not found" not in r.stdout,
        f"rc={r.returncode} {got} {r.stderr[-150:]}",
    )
    git(cs, "reset", "-q", "--hard", "HEAD")
    git(cs, "clean", "-fdq")

    # Rail: an index warmed before a line-shifting commit must not be trusted at
    # stale lines. A prepend shifts used_by_a's body; the per-file SHA-256 no
    # longer matches HEAD, so the file drifts to import-graph, not a stale lookup.
    git(cs, "checkout", "-q", ".")
    git(cs, "clean", "-fdq")
    g.run(
        "-n",
        "2",
        "--cov=mymod",
        "--cov-context=test",
        "--cov-report=",
        cwd=cs,
        env_extra={"PYTHONPATH": str(cs)},
    )  # warm at current HEAD
    (cs / "mymod.py").write_text(
        "def zzz():\n    return 0\n" + (cs / "mymod.py").read_text(), encoding="utf-8"
    )
    subprocess.run(
        [
            "git",
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qam",
            "prepend shifts lines",
        ],
        cwd=cs,
        check=True,
    )
    edit_line("mymod.py", "    return 1\n", "    return 111\n")  # used_by_a body, now shifted
    r = g.run("-n", "2", "--changed", "--co", "-q", cwd=cs, env_extra={"PYTHONPATH": str(cs)})
    got = sorted(set(re.findall(r"test_[ab]\.py::test_[ab]", r.stdout)))
    check(
        "cov-select: line-shift drift -> import-graph fallback, not stale lookup",
        got == ["test_a.py::test_a", "test_b.py::test_b"],
        f"{got} {r.stderr[-150:]}",
    )
    git(cs, "reset", "-q", "--hard", "HEAD~1")
    git(cs, "clean", "-fdq")

    print("== changed selection: files with no line-diff ==")
    # `git diff -U0` emits no hunk for deletions, binaries, and renames, so
    # parse_diff_hunks drops them; a --name-only union recovers them for the
    # fallback. A deleted test file must be skipped, not handed to pytest.
    git(cs, "reset", "-q", "--hard", "HEAD")
    git(cs, "clean", "-fdq")
    # Warm the index so the deleted-test case exercises the warm direct_tests
    # branch (the path actually wired for coverage selection).
    g.run(
        "-n",
        "2",
        "--cov=mymod",
        "--cov-context=test",
        "--cov-report=",
        cwd=cs,
        env_extra={"PYTHONPATH": str(cs)},
    )

    # Deleted TEST file: no file on disk -> dropped, not selected. Nothing else
    # changed -> nothing to run, and crucially NO missing-path error.
    (cs / "test_a.py").unlink()
    r = g.run("--changed", cwd=cs, env_extra={"PYTHONPATH": str(cs)})
    check(
        "changed: deleted test file skipped, no missing-path error",
        r.returncode == 0
        and "no tests affected" in r.stdout
        and "not found" not in (r.stdout + r.stderr)
        and "No such file" not in (r.stdout + r.stderr),
        f"rc={r.returncode} " + (r.stdout + r.stderr)[-250:],
    )
    git(cs, "checkout", "-q", ".")

    # Deleted SOURCE file under --changed-strict: -U0 shows +++ /dev/null (no
    # hunk). Pre-fix it was dropped and falsely SKIPPED everything; the
    # --name-only union routes it to the strict rail, forcing a full run.
    (cs / "mymod.py").unlink()
    r = g.run("--changed-strict", cwd=cs, env_extra={"PYTHONPATH": str(cs)})
    check(
        "changed-strict: deleted source file forces full run (not a false skip)",
        "falling back to full run" in r.stderr and "no tests affected" not in r.stdout,
        r.stderr[-250:] + " || " + r.stdout[-150:],
    )
    git(cs, "checkout", "-q", ".")

    # Changed BINARY (non-Python) file: -U0 emits no @@ hunk, so parse_diff_hunks
    # drops it; the --name-only union recovers it and rule1 forces a full run.
    (cs / "blob.bin").write_bytes(b"\x00\x01\x02rstest\x00")
    git(cs, "add", "-A")
    git_commit(cs, "add binary")
    (cs / "blob.bin").write_bytes(b"\x00\x01\x02rstest\xffCHANGED\x00")
    r = g.run("--changed", cwd=cs, env_extra={"PYTHONPATH": str(cs)})
    check(
        "changed: modified binary file -> full run (name-only recovers it)",
        "falling back to full run" in r.stderr and "non-Python" in r.stderr,
        r.stderr[-250:],
    )
    git(cs, "reset", "-q", "--hard", "HEAD~1")
    git(cs, "clean", "-fdq")


def gate_coverage_selection_under_autocrlf_crlf_w(g, args, binary):
    print("== coverage selection under autocrlf (CRLF worktree, LF blob) ==")
    # Regression: autocrlf stores the blob LF while the worktree is CRLF. The
    # index hash (worktree, Python) and drift check (blob, Rust) must agree once
    # both normalize newlines, else every indexed file "drifts" on Windows.
    cr = g.tmp / "crlf"
    cr.mkdir(parents=True, exist_ok=True)

    def wr_crlf(rel, text):
        (cr / rel).write_bytes(text.replace("\n", "\r\n").encode("utf-8"))

    wr_crlf(
        "mymod.py",
        "def used_by_a():\n    return 1\n"
        "def used_by_b():\n    return 2\n"
        "def used_by_both():\n    return 3\n",
    )
    wr_crlf(
        "test_a.py",
        "import mymod\n"
        "def test_a():\n    assert mymod.used_by_a() == 1\n"
        "    assert mymod.used_by_both() == 3\n",
    )
    wr_crlf(
        "test_b.py",
        "import mymod\n"
        "def test_b():\n    assert mymod.used_by_b() == 2\n"
        "    assert mymod.used_by_both() == 3\n",
    )
    (cr / "pyproject.toml").write_bytes(b"[tool.pytest.ini_options]\n")
    git(cr, "init", "-q")
    git(cr, "config", "core.autocrlf", "true")
    subprocess.run(["git", "add", "-A"], cwd=cr, check=True, capture_output=True)  # blobs -> LF
    git_commit(cr, "init")
    blob = subprocess.run(["git", "show", "HEAD:mymod.py"], cwd=cr, capture_output=True).stdout
    check(
        "autocrlf: blob normalized to LF, worktree stays CRLF",
        b"\r\n" not in blob and b"\r\n" in (cr / "mymod.py").read_bytes(),
        f"blob_has_crlf={b'/r/n' in blob}",
    )
    # Warm the index (Python hashes the CRLF worktree, normalized to LF).
    g.run(
        "-n",
        "2",
        "--cov=mymod",
        "--cov-context=test",
        "--cov-report=",
        cwd=cr,
        env_extra={"PYTHONPATH": str(cr)},
    )
    # Edit only used_by_a's body. The stored hash (normalized CRLF) still equals
    # the base blob hash (normalized LF), so the index is trusted and narrows to
    # test_a. Pre-fix the CRLF-vs-LF mismatch drifted every file, running both.
    wr_crlf(
        "mymod.py",
        "def used_by_a():\n    return 111\n"
        "def used_by_b():\n    return 2\n"
        "def used_by_both():\n    return 3\n",
    )
    r = g.run("-n", "2", "--changed", "--co", "-q", cwd=cr, env_extra={"PYTHONPATH": str(cr)})
    got = sorted(set(re.findall(r"test_[ab]\.py::test_[ab]", r.stdout)))
    check(
        "autocrlf: index trusted across CRLF/LF -> narrows to test_a",
        got == ["test_a.py::test_a"],
        f"{got} || {r.stderr[-150:]}",
    )


def gate_shuffle(g, args, binary):
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


def gate_duration_regression_gate(g, args, binary):
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
    r = g.run(
        ".", "-n", "2", "--durations-regress", "2.0", cwd=ddir, env_extra={"DREG_SLEEP": "0.1"}
    )
    check(
        "durations-regress: cold baseline skips, run green",
        r.returncode == 0 and "no duration baseline yet" in r.stderr,
        f"rc={r.returncode} " + r.stderr[-200:],
    )
    r = g.run(
        ".", "-n", "2", "--durations-regress", "2.0", cwd=ddir, env_extra={"DREG_SLEEP": "0.1"}
    )
    check(
        "durations-regress: warm baseline, no regressions",
        r.returncode == 0 and "no regressions" in r.stderr,
        f"rc={r.returncode} " + r.stderr[-200:],
    )
    r = g.run(
        ".", "-n", "2", "--durations-regress", "2.0", cwd=ddir, env_extra={"DREG_SLEEP": "1.2"}
    )
    check(
        "durations-regress: regression flagged, exit 1, stable test quiet",
        r.returncode == 1
        and "duration regressions" in r.stdout
        and "test_variable" in r.stdout
        and "test_stable" not in r.stdout.split("duration regressions")[1]
        and "1 duration regression" in r.stderr,
        f"rc={r.returncode} " + r.stdout[-300:] + r.stderr[-150:],
    )


def gate_shared_cache_backend(g, args, binary):
    print("== shared cache backend ==")
    # Push this run's segment to a remote dir; a fresh project pulls the union.
    remote = g.tmp / "shared-remote"
    shutil.rmtree(remote, ignore_errors=True)
    g.write(
        "scproj_a/test_s.py",
        "import time\ndef test_slow(): time.sleep(0.05)\ndef test_a(): assert True\n",
    )
    sca = g.tmp / "scproj_a"
    r = g.run("test_s.py", "-n", "2", "--cache-remote", str(remote), "--cache-push", cwd=sca)
    segdir = remote / "segments"
    segs = list(segdir.glob("seg-*.json")) if segdir.exists() else []
    check(
        "shared-cache: push writes exactly one segment",
        r.returncode == 0 and len(segs) == 1 and "pushed segment" in r.stderr,
        f"rc={r.returncode} segs={segs} {r.stderr[-150:]}",
    )
    # Fresh project with no local cache: pull populates it from the remote.
    g.write("scproj_b/test_s.py", "def test_a(): assert True\n")
    scb = g.tmp / "scproj_b"
    r = g.run("test_s.py", "-n", "2", "--cache-remote", str(remote), "--cache-pull", cwd=scb)
    check(
        "shared-cache: pull populates local durations",
        r.returncode == 0
        and (scb / ".rstest_cache" / "durations.json").exists()
        and "pulled" in r.stderr,
        r.stderr[-200:],
    )
    # require-baseline against a cold remote is a hard error, not a silent skip.
    # RSTEST_CACHE points at an empty dir so a prior local cache can't satisfy it.
    cold = g.tmp / "cold-remote"
    r = g.run(
        "test_s.py",
        "-n",
        "2",
        "--cache-remote",
        str(cold),
        "--cache-pull",
        "--require-baseline",
        "--durations-regress",
        "1.5",
        cwd=scb,
        env_extra={"RSTEST_CACHE": str(g.tmp / "nolocal-cache")},
    )
    check(
        "shared-cache: require-baseline errors on a cold remote",
        r.returncode != 0 and "require-baseline" in r.stderr,
        f"rc={r.returncode} " + r.stderr[-200:],
    )
    # Compact folds the segment into a base and prunes segments.
    r = g.run("--cache-remote", str(remote), "--cache-compact", cwd=sca)
    leftover = list(segdir.glob("seg-*.json")) if segdir.exists() else []
    check(
        "shared-cache: compact folds to base, prunes segments",
        r.returncode == 0
        and (remote / "base.json").exists()
        and not leftover
        and "compacted" in r.stderr,
        f"rc={r.returncode} left={leftover}",
    )

    # Coverage index rides the shared cache: two shards each push their PARTIAL
    # coverage slice; a pull unions them into a full line->test index. This is
    # the sharded-partial-index limitation the shared cache exists to fix.
    covremote = g.tmp / "shared-cov-remote"
    shutil.rmtree(covremote, ignore_errors=True)
    scc = g.tmp / "scproj_cov"
    g.write(
        "scproj_cov/mymod.py", "def used_by_a():\n    return 1\ndef used_by_b():\n    return 2\n"
    )
    g.write("scproj_cov/test_a.py", "import mymod\ndef test_a(): assert mymod.used_by_a() == 1\n")
    g.write("scproj_cov/test_b.py", "import mymod\ndef test_b(): assert mymod.used_by_b() == 2\n")
    # Shard 1 covers used_by_a, shard 2 covers used_by_b; each pushes its slice.
    covenv = {"PYTHONPATH": str(scc)}
    for k in (1, 2):
        g.run(
            "-n",
            "2",
            "--shard",
            f"{k}/2",
            "--cov=mymod",
            "--cov-context=test",
            "--cov-report=",
            "--cache-remote",
            str(covremote),
            "--cache-push",
            cwd=scc,
            env_extra=covenv,
        )
    covsegs = sorted((covremote / "segments").glob("seg-*.json"))
    seg_blobs = [json.loads(p.read_text()) for p in covsegs]
    covered_files = {f for b in seg_blobs for f in b.get("cov_index", {}).get("files", {})}
    check(
        "shared-cache: coverage segments carry a cov_index slice",
        len(covsegs) == 2 and any("mymod.py" in f for f in covered_files),
        f"segs={len(covsegs)} files={covered_files}",
    )
    # A fresh clone pulls the UNION: both used_by_a and used_by_b lines mapped.
    sccb = g.tmp / "scproj_cov_b"
    shutil.copytree(scc, sccb)
    shutil.rmtree(sccb / ".rstest_cache", ignore_errors=True)
    r = g.run(
        "-n",
        "2",
        "--cache-remote",
        str(covremote),
        "--cache-pull",
        "--co",
        "-q",
        cwd=sccb,
        env_extra={"PYTHONPATH": str(sccb)},
    )
    pulled_idx = sccb / ".rstest_cache" / "coverage_index.json"
    idx = json.loads(pulled_idx.read_text()) if pulled_idx.exists() else {}
    mymod_key = next((k for k in idx.get("files", {}) if k.endswith("mymod.py")), None)
    nodeids = set()
    if mymod_key:
        for ids in idx["files"][mymod_key]["lines"].values():
            nodeids.update(ids)
    check(
        "shared-cache: pull unions shard coverage slices into one index",
        idx.get("schema") == 2 and {"test_a.py::test_a", "test_b.py::test_b"} <= nodeids,
        f"rc={r.returncode} key={mymod_key} nodeids={sorted(nodeids)}",
    )

    # sccb still holds the pulled MERGED coverage index from above. A --cache-push
    # run that produces NO fresh index of its own — here NO --cov at all, the path
    # that skips covtool entirely — must NOT re-publish that pulled index.
    covr3 = g.tmp / "shared-cov-remote3"
    shutil.rmtree(covr3, ignore_errors=True)
    assert (sccb / ".rstest_cache" / "coverage_index.json").exists()  # precondition
    g.run(
        "-n",
        "2",  # no --cov: covtool never runs, so only the push-time drop guards it
        "--cache-remote",
        str(covr3),
        "--cache-push",
        cwd=sccb,
        env_extra={"PYTHONPATH": str(sccb)},
    )
    r3segs = sorted((covr3 / "segments").glob("seg-*.json"))
    r3blobs = [json.loads(p.read_text()) for p in r3segs]
    republished = any(b.get("cov_index", {}).get("files") for b in r3blobs)
    check(
        "shared-cache: push without a fresh index re-publishes no coverage",
        len(r3segs) >= 1 and not republished,
        f"segs={len(r3segs)} republished={republished}",
    )

    # --cache-pull OVERLAYS remote onto the local cache: local-only entries that
    # were never pushed survive the pull rather than being clobbered.
    scpl = g.tmp / "scproj_pull_local"
    shutil.rmtree(scpl, ignore_errors=True)
    (scpl / ".rstest_cache").mkdir(parents=True)
    (scpl / ".rstest_cache" / "durations.json").write_text(
        '{"local_only.py::t_local": 4.2}', encoding="utf-8"
    )
    g.write("scproj_pull_local/test_s.py", "def test_a(): assert True\n")
    r = g.run("test_s.py", "-n", "2", "--cache-remote", str(remote), "--cache-pull", cwd=scpl)
    merged_local = json.loads((scpl / ".rstest_cache" / "durations.json").read_text())
    check(
        "shared-cache: pull preserves local-only durations (overlay, not clobber)",
        "local_only.py::t_local" in merged_local and len(merged_local) > 1,
        f"rc={r.returncode} keys={sorted(merged_local)}",
    )

    # --cache-compact is run-less; combining it with a run-time cache flag would
    # silently skip the run and the flag, exiting green. Must be rejected.
    r = g.run("--cache-remote", str(remote), "--cache-compact", "--cache-push", cwd=sca)
    check(
        "shared-cache: --cache-compact + --cache-push is rejected",
        r.returncode != 0 and "run-less" in r.stderr,
        f"rc={r.returncode} {r.stderr[-160:]}",
    )

    # Shared-cache flags in monorepo mode would silently no-op (push unreachable,
    # pull warms the wrong root cache). Must fail loud instead, and the rejection
    # must come BEFORE the pull runs (no 'pulled' line first). `mono` fixture is
    # the multi-project tree built in the monorepo section above.
    r = g.run("-n", "2", "--cache-remote", str(remote), "--cache-pull", cwd=g.tmp / "mono")
    check(
        "shared-cache: cache flags rejected in monorepo mode before pull runs",
        r.returncode != 0 and "monorepo" in r.stderr and "pulled" not in r.stderr,
        f"rc={r.returncode} {r.stderr[-200:]}",
    )

    # --cache-remote FLAG with no pull/push/compact does nothing; warn.
    r = g.run("test_s.py", "-n", "2", "--cache-remote", str(remote), cwd=sca)
    check(
        "shared-cache: --cache-remote flag without an action warns",
        r.returncode == 0 and "not being used" in r.stderr,
        f"rc={r.returncode} {r.stderr[-160:]}",
    )
    # But an ambient RSTEST_CACHE_REMOTE env (no flag, no action) must NOT nag.
    r = g.run("test_s.py", "-n", "2", cwd=sca, env_extra={"RSTEST_CACHE_REMOTE": str(remote)})
    check(
        "shared-cache: ambient RSTEST_CACHE_REMOTE env does not warn",
        r.returncode == 0 and "not being used" not in r.stderr,
        f"rc={r.returncode} {r.stderr[-160:]}",
    )


def gate_tool_rstest_config(g, args, binary):
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
    r = g.run(
        "test_cfg.py", "-n", "0", cwd=g.tmp / "toolcfg", env_extra={"FLAKY_MARKER": str(tmarker)}
    )
    check("CLI overrides tool.rstest", "single worker" in r.stdout.splitlines()[0], r.stdout[:120])
    check(
        "tail-batch rerun works (EndSession model)",
        True,  # asserted by 'tool.rstest defaults applied': 1 item, rerun delivered post-drain
    )


def gate_flaky_reruns(g, args, binary):
    print("== flaky reruns ==")
    g.write("flaky/test_flaky.py", FLAKY)
    fdir = g.tmp / "flaky"
    marker = g.tmp / "flaky_marker"
    if marker.exists():
        marker.unlink()
    r = g.run(
        "test_flaky.py",
        "-n",
        "2",
        "--reruns",
        "2",
        cwd=fdir,
        env_extra={"FLAKY_MARKER": str(marker)},
    )
    check("flaky passes with reruns", r.returncode == 0 and "1 flaky" in r.stdout, r.stdout[-200:])
    check("flaky section listed", "passed after rerun" in r.stdout)
    marker.unlink()

    # buildkite_flaky_annotate: with BUILDKITE set and a flaky-passed test, rstest
    # builds a flaky annotation and hands it to `buildkite-agent annotate`; absent
    # that binary (CI/gate runners), it best-effort-skips with a stderr notice.
    r = g.run(
        "test_flaky.py",
        "-n",
        "2",
        "--reruns",
        "2",
        "--output",
        "buildkite",
        cwd=fdir,
        env_extra={"FLAKY_MARKER": str(marker), "BUILDKITE": "1"},
    )
    check(
        "buildkite: flaky annotation attempted (best-effort skip without agent)",
        r.returncode == 0 and "1 flaky" in r.stdout and "Buildkite flaky annotation" in r.stderr,
        f"rc={r.returncode} " + r.stdout[-200:] + " || " + r.stderr[-200:],
    )
    marker.unlink(missing_ok=True)

    # Single-worker reruns: --reruns at -n 1 / -n 0 must fire (a degenerate
    # one-worker pool drives the rerun loop) instead of being silently inert.
    swm = g.tmp / "sw_reruns_marker"
    swm.unlink(missing_ok=True)
    r = g.run(
        "test_flaky.py", "-n", "1", "--reruns", "2", cwd=fdir, env_extra={"FLAKY_MARKER": str(swm)}
    )
    check(
        "single-worker reruns fire at -n 1",
        r.returncode == 0
        and "1 flaky" in r.stdout
        and "single worker (rerun pool" in r.stdout.splitlines()[0]
        # the byte-exact -> pool switch is announced on stderr, not just the banner
        and "not byte-exact" in r.stderr,
        f"rc={r.returncode} " + r.stdout.splitlines()[0] + " || " + r.stderr[-200:],
    )
    swm.unlink(missing_ok=True)
    r = g.run(
        "test_flaky.py", "-n", "0", "--reruns", "2", cwd=fdir, env_extra={"FLAKY_MARKER": str(swm)}
    )
    check(
        "single-worker reruns fire at -n 0",
        r.returncode == 0 and "1 flaky" in r.stdout,
        f"rc={r.returncode} " + r.stdout[-200:],
    )
    # No --reruns at -n 1 stays byte-exact single session: the flake fails.
    swm.unlink(missing_ok=True)
    r = g.run("test_flaky.py", "-n", "1", cwd=fdir, env_extra={"FLAKY_MARKER": str(swm)})
    check(
        "no-reruns at -n 1 stays byte-exact (flake fails)",
        r.returncode == 1
        and "1 failed" in r.stdout
        and "pytest-exact mode" in r.stdout.splitlines()[0],
        f"rc={r.returncode} " + r.stdout.splitlines()[0] + " || " + r.stdout[-200:],
    )
    # Passthrough (-s) can't be pooled: reruns stay inert, warned.
    swm.unlink(missing_ok=True)
    r = g.run(
        "test_flaky.py",
        "-n",
        "1",
        "--reruns",
        "2",
        "-s",
        cwd=fdir,
        env_extra={"FLAKY_MARKER": str(swm)},
    )
    check(
        "reruns inert under -s, warned",
        r.returncode == 1 and "ignored under -s" in r.stderr,
        f"rc={r.returncode} " + r.stderr[-200:],
    )
    marker.unlink(missing_ok=True)
    r = g.run("test_flaky.py", "-n", "2", cwd=fdir, env_extra={"FLAKY_MARKER": str(marker)})
    check(
        "flaky fails without reruns", r.returncode == 1 and "1 failed" in r.stdout, r.stdout[-200:]
    )


def gate_flaky_aware_reruns_reruns_only_known_fla(g, args, binary):
    print("== flaky-aware reruns (--reruns-only-known-flaky) ==")
    g.write("flaky/test_flaky.py", FLAKY)  # self-contained (also written by gate_flaky_reruns)
    fdir = g.tmp / "flaky"
    marker = g.tmp / "flaky_marker"
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
        r = g.run(
            "test_flaky.py",
            "-n",
            "2",
            "--reruns",
            "2",
            "--reruns-only-known-flaky",
            *extra,
            cwd=fdir,
            env_extra={"FLAKY_MARKER": str(m)},
        )
        m.unlink(missing_ok=True)
        return r

    # In history with flaky>0 -> rerun-eligible -> recovers. Use a current
    # epoch so retention aging (load() drops entries past the window) keeps it.
    now_epoch = int(time.time())
    r = faware({fnode: {"flaky": 2, "failed": 0, "last_epoch": now_epoch}}, "fa_known")
    check(
        "flaky-aware: known-flaky test is reran and recovers",
        r.returncode == 0 and "1 flaky" in r.stdout,
        r.stdout[-200:],
    )
    # No history -> not known-flaky -> not reran -> fails.
    r = faware(None, "fa_unknown")
    check(
        "flaky-aware: unknown test not reran, fails",
        r.returncode == 1 and "1 failed" in r.stdout and "passed after rerun" not in r.stdout,
        r.stdout[-200:],
    )
    # Hard-failure-only history (flaky==0) -> still not known-flaky: a
    # deterministic mass-failure recorded as `failed` never burns the budget.
    r = faware({fnode: {"flaky": 0, "failed": 9, "last_epoch": now_epoch}}, "fa_failedonly")
    check(
        "flaky-aware: failed-only history does not count as known-flaky",
        r.returncode == 1 and "1 failed" in r.stdout,
        r.stdout[-200:],
    )
    # Baseline sanity: same unknown test WITHOUT the flag reruns and recovers.
    m = g.tmp / "fa_baseline"
    m.unlink(missing_ok=True)
    fcache.unlink(missing_ok=True)
    r = g.run(
        "test_flaky.py", "-n", "2", "--reruns", "2", cwd=fdir, env_extra={"FLAKY_MARKER": str(m)}
    )
    m.unlink(missing_ok=True)
    check(
        "flaky-aware: without the flag, unknown flake still recovers",
        r.returncode == 0 and "1 flaky" in r.stdout,
        r.stdout[-200:],
    )

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
    r = g.run(
        "test_m.py",
        "-n",
        "2",
        "--reruns",
        "2",
        "--reruns-only-known-flaky",
        cwd=mdir,
        env_extra={"MK": str(mk)},
    )
    mk.unlink(missing_ok=True)
    check(
        "flaky-aware: @mark.flaky bypasses the gate (no history)",
        r.returncode == 0 and "1 flaky" in r.stdout,
        r.stdout[-200:],
    )

    # Cold-start loop (finding #4 / known defect): the docs once claimed a
    # brand-new flake "fails that run, is recorded, and is rescued on
    # subsequent runs". It is NOT. The gate suppresses the rerun that would
    # record `flaky > 0`, so a gated run only ever records the failure as
    # `failed` (flaky == 0) -> the test stays unknown -> the NEXT gated run
    # gates it again. The corrected docs describe the real two-mode workflow.
    #
    # This test PINS that current behavior (run 2 still fails), so gate.py
    # stays green today. It is the tripwire for the fix: when a
    # learn-without-rerun mechanism lands and cold-start self-heals, run 2
    # will start passing and the run-2 assertion below will flip to FAIL —
    # at which point update it to assert recovery (rc == 0, "1 flaky").
    cs_marker = "fa_coldstart"

    def coldstart_run():
        m = g.tmp / cs_marker
        m.unlink(missing_ok=True)  # fresh: fixture fails its first attempt
        r = g.run(
            "test_flaky.py",
            "-n",
            "2",
            "--reruns",
            "2",
            "--reruns-only-known-flaky",
            cwd=fdir,
            env_extra={"FLAKY_MARKER": str(m)},
        )
        m.unlink(missing_ok=True)
        return r

    fcache.unlink(missing_ok=True)  # no seeded history: truly cold
    r1 = coldstart_run()
    # Run 1: unknown -> gated -> fails, and the failure IS recorded so we know
    # the miss is the gate, not a missing write.
    hist_after = json.loads(fcache.read_text()) if fcache.exists() else {}
    rec = hist_after.get(fnode, {})
    check(
        "flaky-aware cold-start: run 1 gated-fails and records the failure",
        r1.returncode == 1 and rec.get("failed", 0) > 0 and rec.get("flaky", 0) == 0,
        f"rc={r1.returncode} rec={rec} {r1.stdout[-160:]}",
    )
    # Run 2: same flag, history now carries the run-1 failure (flaky == 0).
    # A self-healing feature WOULD rescue it here; today it does not. Assert
    # the current (defective) behavior so CI stays green. When cold-start is
    # fixed this flips red -> update to assert rc == 0 and "1 flaky".
    r2 = coldstart_run()
    check(
        "flaky-aware cold-start: run 2 still gated-fails (pins finding #4; flip when fixed)",
        r2.returncode == 1 and "1 failed" in r2.stdout and "passed after rerun" not in r2.stdout,
        r2.stdout[-200:],
    )
    fcache.unlink(missing_ok=True)

    g.write("crashflaky/test_cf.py", CRASHFLAKY)
    cmarker = g.tmp / "cf_marker"
    if cmarker.exists():
        cmarker.unlink()
    r = g.run(
        "test_cf.py",
        "-n",
        "2",
        "--reruns",
        "1",
        cwd=g.tmp / "crashflaky",
        env_extra={"FLAKY_MARKER": str(cmarker)},
    )
    check(
        "crashed test retried within budget",
        r.returncode == 0 and "1 flaky" in r.stdout and "2 passed" in r.stdout,
        r.stdout[-200:],
    )
    marker.unlink(missing_ok=True)
    fx = g.tmp / "flaky_junit.xml"
    g.run(
        "test_flaky.py",
        "-n",
        "2",
        "--reruns",
        "2",
        "--junitxml",
        str(fx),
        cwd=fdir,
        env_extra={"FLAKY_MARKER": str(marker)},
    )
    check(
        "flaky flagged in junit property",
        'property name="flaky"' in fx.read_text(encoding="utf-8"),
        fx.read_text(encoding="utf-8")[-300:],
    )
    # --output github: a flaky-passed test surfaces as a ::warning
    # annotation (the run is green, the flake is visible on the PR).
    marker.unlink(missing_ok=True)
    r = g.run(
        "test_flaky.py",
        "-n",
        "2",
        "--reruns",
        "2",
        "--output",
        "github",
        cwd=fdir,
        env_extra={"FLAKY_MARKER": str(marker)},
    )
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


def gate_quarantine(g, args, binary):
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
    r = g.run(".", "-n", "2", "--quarantine", "quarantine.txt", "--junitxml", str(qx), cwd=qdir)
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


def gate_loadscope_loadgroup(g, args, binary):
    print("== loadscope / loadgroup ==")
    g.write("scopes/test_sc_a.py", SCOPE_A)
    g.write("scopes/test_sc_b.py", SCOPE_B)
    g.write("scopes/test_sc_c.py", SCOPE_C)
    slog = g.tmp / "scope_log"
    for mode, _label in (("loadscope", "class"), ("loadgroup", "group")):
        clear_e2e_log(slog)
        r = g.run("-n", "3", "--dist", mode, cwd=g.tmp / "scopes", env_extra={"SLOG": str(slog)})
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


def gate_flaky_marks_only_rerun(g, args, binary):
    print("== flaky marks / only-rerun ==")
    g.write("marks/test_marks.py", MARKS)
    mk = g.tmp / "marks_marker"
    cnt = g.tmp / "marks_count"
    for f in (mk, cnt):
        if f.exists():
            f.unlink()
    r = g.run(
        "test_marks.py", "-n", "2", cwd=g.tmp / "marks", env_extra={"MK": str(mk), "CNT": str(cnt)}
    )
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
    r = g.run(
        "test_marks.py",
        "-n",
        "2",
        "--reruns",
        "2",
        "--only-rerun",
        "transient",
        cwd=g.tmp / "marks",
        env_extra={"MK": str(mk), "CNT": str(cnt)},
    )
    check(
        "only-rerun gates non-matching failures",
        "1 flaky" in r.stdout and cnt.read_text() == "1",
        f"count={cnt.read_text()} " + r.stdout[-160:],
    )


def gate_worker_timeout_watchdog(g, args, binary):
    print("== worker-timeout watchdog ==")
    g.write("hang/test_hang.py", HANG)
    r = g.run("test_hang.py", "-n", "2", "--worker-timeout", "3", cwd=g.tmp / "hang", timeout=60)
    check(
        "hung test killed and attributed",
        r.returncode == 1 and "exceeded --worker-timeout" in r.stdout and "2 passed" in r.stdout,
        r.stdout[-300:],
    )


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


def git(cwd, *args):
    """Run a git subcommand in cwd, raising on failure."""
    subprocess.run(["git", *args], cwd=cwd, check=True)


def git_commit(cwd, msg="base"):
    """Commit staged changes with a fixed throwaway identity (no gate check
    inspects the author)."""
    git(cwd, "-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", msg)


def git_init_commit(cwd, msg="base"):
    """init + add -A + commit: the standard fixture-repo bootstrap."""
    git(cwd, "init", "-q")
    git(cwd, "add", "-A")
    git_commit(cwd, msg)


def main():
    ap = argparse.ArgumentParser()
    default_binary = REPO / "target" / "release" / ("rstest.exe" if WINDOWS else "rstest")
    ap.add_argument("--binary", default=str(default_binary))
    ap.add_argument("--venv", default=str(REPO / ".gate-venv"))
    ap.add_argument(
        "--only",
        default="",
        help="run only sections whose name contains this substring (dev iteration; "
        "sections run in order, so a section that reuses an earlier one's fixtures "
        "may need a broader filter). Use --list to see names.",
    )
    ap.add_argument("--list", action="store_true", help="list section names and exit")
    args = ap.parse_args()

    sections = (
        gate_basics,
        gate_collection_error_semantics,
        gate_output_styles,
        gate_multiprocessing_spawn_children,
        gate_crash_handling,
        gate_report_json_contract,
        gate_collect_only_discovery_json,
        gate_pytest_randomly_real_plugin,
        gate_pytest_rerunfailures_xdist_no_sock_port_,
        gate_pytest_retry_xdist_server_port_self_prov,
        gate_interpreter_probe_cache_heals_after_deps,
        gate_lazy_collection,
        gate_serial_mark,
        gate_failure_output,
        gate_x_maxfail,
        gate_lf,
        gate_junitxml,
        gate_shard_k_n,
        gate_dist_each,
        gate_dist_validation,
        gate_testnodedown_for_crashed_workers,
        gate_xdist_master_side_hooks,
        gate_one_arg_pytest_testnodedown,
        gate_durations,
        gate_doctest_modules,
        gate_monorepo,
        gate_warnings,
        gate_doctor,
        gate_auto_worker_capping,
        gate_coverage,
        gate_coverage_contexts_line_test_index_cov_co,
        gate_smart_selection,
        gate_coverage_based_selection_changed_uses_th,
        gate_coverage_selection_under_autocrlf_crlf_w,
        gate_shuffle,
        gate_duration_regression_gate,
        gate_shared_cache_backend,
        gate_tool_rstest_config,
        gate_flaky_reruns,
        gate_flaky_aware_reruns_reruns_only_known_fla,
        gate_quarantine,
        gate_loadscope_loadgroup,
        gate_flaky_marks_only_rerun,
        gate_worker_timeout_watchdog,
        gate_try,
        gate_migrate_check,
        gate_watch_mode,
    )
    names = [s.__name__.removeprefix("gate_") for s in sections]
    if args.list:
        print("\n".join(names))
        return
    selected = [s for s in sections if not args.only or args.only in s.__name__]
    if not selected:
        sys.exit(f"--only {args.only!r} matched no section; --list to see names")
    if args.only:
        print(f"running {len(selected)}/{len(sections)} sections matching {args.only!r}")

    binary = Path(args.binary).resolve()
    assert binary.exists(), f"binary missing: {binary} (cargo build --release first)"
    make_venv(Path(args.venv))
    g = Gate(binary, Path(args.venv).resolve())

    for _section in selected:
        _section(g, args, binary)

    print(f"\n{PASS} ok, {len(FAIL)} failed")
    if FAIL:
        print("FAILED:", ", ".join(FAIL))
        sys.exit(1)


# Fixture suites live as real files under e2e/fixtures/, loaded by name.
BASIC = fx("basic.py")
CRASH = fx("crash.py")
CRASHFLAKY = fx("crashflaky.py")
CRASHLOOP = fx("crashloop.py")
DISCO = fx("disco.py")
DOCTEST_MOD = fx("doctest_mod.py")
DOCTOR = fx("doctor.py")
DURATIONS_FIXTURE = fx("durations_fixture.py")
FLAKY = fx("flaky.py")
HANG = fx("hang.py")
LAZY_CONFTEST = fx("lazy_conftest.py")
LAZY_SESSION_A = fx("lazy_session_a.py")
LAZY_SESSION_B = fx("lazy_session_b.py")
LF = fx("lf.py")
MARKS = fx("marks.py")
MAXFAIL = fx("maxfail.py")
MP_SPAWN = fx("mp_spawn.py")
NODECRASH_CONFTEST = fx("nodecrash_conftest.py")
NODECRASH_TEST = fx("nodecrash_test.py")
NODEHOOKS_CONFTEST = fx("nodehooks_conftest.py")
NODEHOOKS_TEST = fx("nodehooks_test.py")
NODEONEARG_CONFTEST = fx("nodeonearg_conftest.py")
SCOPE_A = fx("scope_a.py")
SCOPE_B = fx("scope_b.py")
SCOPE_C = fx("scope_c.py")
SECTIONS = fx("sections.py")
SERIAL = fx("serial.py")
WARN = fx("warn.py")


if __name__ == "__main__":
    main()
