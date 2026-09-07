"""e2e gate sections: reporting."""

import json
import os
import xml.etree.ElementTree as ET

from _harness import BASIC, DISCO, DOCTOR, DURATIONS_FIXTURE, SECTIONS, WARN, check, parse_ndjson


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


def gate_failure_output(g, args, binary):
    print("== failure output ==")
    g.write("sections/test_sections.py", SECTIONS)
    r = g.run("sections", "-n", "2")
    check(
        "captured stdout section",
        "Captured stdout call" in r.stdout and "the database said no" in r.stdout,
    )


def gate_html_report(g, args, binary):
    print("== html report ==")
    hp = g.tmp / "htmlproj"
    g.write(
        "htmlproj/test_h.py",
        "def test_ok(): assert True\n"
        'def test_bad(): assert 1 == 2, "values <differ> & <script>x</script>"\n',
    )
    out = hp / "report.html"
    # -n 2: the case pytest-html can't do (no writer registered on any worker).
    r = g.run("test_h.py", "-n", "2", "--html", str(out), cwd=hp, env_extra={"PYTHONPATH": str(hp)})
    doc = out.read_text() if out.exists() else ""
    check(
        "html: written at -n 2 with a valid document",
        r.returncode == 1 and out.exists() and doc.startswith("<!doctype html>"),
        f"rc={r.returncode} exists={out.exists()}",
    )
    check(
        "html: summary reflects merged counts",
        "1 passed" in doc and "1 failed" in doc,
        doc[:400],
    )
    check(
        "html: failing nodeid + its traceback are present",
        "test_h.py::test_bad" in doc and "AssertionError" in doc,
        "",
    )
    check(
        "html: untrusted traceback markup is escaped, not live",
        "&lt;differ&gt;" in doc
        and "&lt;script&gt;x&lt;/script&gt;" in doc
        and "<script>x</script>" not in doc,
        "escaping breach",
    )
    check(
        "html: self-contained (embedded data, no external asset refs)",
        'id="data"' in doc and "src=" not in doc and 'href="http' not in doc,
        "",
    )
    # Report write failure must surface as a nonzero exit, not a silent green:
    # the run completes, then the post-run report writer errors and that error
    # propagates out of execute() (run.rs write_run_reports `?`). Point --html
    # into a nonexistent directory so fs::write fails.
    bad = hp / "nope" / "report.html"
    r = g.run("test_h.py", "-n", "2", "--html", str(bad), cwd=hp, env_extra={"PYTHONPATH": str(hp)})
    check(
        "html: unwritable report path fails the run (error propagated, not swallowed)",
        r.returncode != 0 and not bad.exists() and "Error:" in r.stderr,
        f"rc={r.returncode} stderr={r.stderr[-200:]}",
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


def gate_resource_leak_detection(g, args, binary):
    print("== resource leak detection ==")
    lp = g.tmp / "leakproj"
    # test_a is the warm-up (first test, first-touch imports skipped); the
    # leakers come after so their deltas are attributed.
    g.write(
        "leakproj/test_leaks.py",
        "import threading\n"
        "def test_a_warmup(): assert True\n"
        "def test_b_clean(): assert True\n"
        "def test_c_thread_leak():\n"
        "    threading.Thread(target=lambda: __import__('time').sleep(30), daemon=True).start()\n"
        "    assert True\n"
        "def test_d_fd_leak():\n"
        "    test_d_fd_leak.f = open('/dev/null')  # never closed\n"
        "    assert True\n",
    )
    # -n 1 keeps collection order deterministic so the warm-up is test_a.
    r = g.run("test_leaks.py", "-n", "1", "--doctor", cwd=lp, env_extra={"PYTHONPATH": str(lp)})
    check(
        "leak: --doctor names the thread + fd leakers",
        "RESOURCE LEAKS" in r.stdout
        and "test_c_thread_leak" in r.stdout
        and "test_d_fd_leak" in r.stdout,
        r.stdout[-400:],
    )
    r = g.run(
        "test_leaks.py", "-n", "1", "--fail-on-leak", cwd=lp, env_extra={"PYTHONPATH": str(lp)}
    )
    check(
        "leak: --fail-on-leak fails the run (exit 1)",
        r.returncode == 1 and "leaked threads/fds" in r.stderr,
        f"rc={r.returncode} " + r.stderr[-300:],
    )
    # A clean suite: no leaks, exit 0.
    g.write(
        "leakproj/test_ok.py",
        "def test_a(): assert True\ndef test_b(): assert True\ndef test_c(): assert True\n",
    )
    r = g.run("test_ok.py", "-n", "1", "--fail-on-leak", cwd=lp, env_extra={"PYTHONPATH": str(lp)})
    check(
        "leak: clean suite passes the gate (exit 0, warm-up caveat noted)",
        r.returncode == 0 and "no thread/fd leaks" in r.stderr and "warm-up" in r.stderr,
        f"rc={r.returncode} " + r.stderr[-200:],
    )
    # Warm-up skip: the FIRST test each worker runs is not leak-checked, so a
    # test that leaks but happens to run first is not flagged (first-touch
    # imports aren't a per-test leak). test_a leaks + runs first under -n 1.
    g.write(
        "leakproj/test_warmup.py",
        "import threading\n"
        "def test_a_leaks_but_is_warmup():\n"
        "    threading.Thread(target=lambda: __import__('time').sleep(30), daemon=True).start()\n"
        "    assert True\n"
        "def test_b_clean(): assert True\n",
    )
    r = g.run(
        "test_warmup.py", "-n", "1", "--fail-on-leak", cwd=lp, env_extra={"PYTHONPATH": str(lp)}
    )
    check(
        "leak: first test is an unchecked warm-up (its leak not flagged)",
        r.returncode == 0 and "no thread/fd leaks" in r.stderr,
        f"rc={r.returncode} " + r.stderr[-200:],
    )
    # --doctor + --fail-on-leak together: the RESOURCE LEAKS table is rendered
    # once (by doctor), not repeated by the gate; gate still fails the run.
    r = g.run(
        "test_leaks.py",
        "-n",
        "1",
        "--doctor",
        "--fail-on-leak",
        cwd=lp,
        env_extra={"PYTHONPATH": str(lp)},
    )
    check(
        "leak: --doctor + --fail-on-leak fails once, no double table",
        r.returncode == 1
        and "leaked threads/fds" in r.stderr
        and (r.stdout + r.stderr).count("test_c_thread_leak") == 1,
        f"rc={r.returncode} " + (r.stdout + r.stderr)[-400:],
    )
    # Passthrough (-s): no instrumentation, so --fail-on-leak is ignored with a
    # warning rather than passing silently (exit reflects the tests, not a gate).
    r = g.run("test_leaks.py", "-s", "--fail-on-leak", cwd=lp, env_extra={"PYTHONPATH": str(lp)})
    check(
        "leak: --fail-on-leak ignored (with warning) in passthrough mode",
        "has no effect in passthrough mode" in r.stderr and "leaked threads/fds" not in r.stderr,
        f"rc={r.returncode} " + r.stderr[-300:],
    )
