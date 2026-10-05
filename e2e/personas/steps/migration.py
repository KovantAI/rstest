"""Steps for features/migration.feature: the pytest / pytest-xdist migrator."""

import json
import os
import re
import shlex
import subprocess
import xml.etree.ElementTree as ET

from _harness import REPO, venv_bin
from pytest_bdd import given, parsers, then, when

from steps.common import q

# Fixture-side logger: one JSON line per event, one file per process
# (cross-process appends to one file tear).
_LOG = (
    "import json, os, time\n\n"
    "def _log(**kw):\n"
    "    kw.setdefault('w', os.environ.get('RSTEST_WORKER_ID') or 'main')\n"
    "    kw.setdefault('pid', os.getpid())\n"
    "    kw.setdefault('t', time.time())\n"
    "    with open(os.environ['MG_LOG'] + '.' + str(os.getpid()), 'a') as f:\n"
    "        f.write(json.dumps(kw) + '\\n')\n"
)


def _last(text):
    lines = [ln for ln in text.strip().splitlines() if ln.strip()]
    return lines[-1] if lines else ""


def _counts(line):
    """Parse a pytest summary line ('== 1 failed, 2 passed in 0.1s ==') into
    {'failed': 1, 'passed': 2}. Plural nouns are folded to singular so a
    wording-only difference (EV-15) does not mask parity."""
    body = re.sub(r"\s+in [\d.]+s.*$", "", line.strip().strip("=").strip())
    out = {}
    for part in body.split(","):
        m = re.match(r"\s*(\d+) (.+?)\s*$", part)
        if m:
            key = re.sub(r"\b(error|warning|subtest)s\b", r"\1", m.group(2))
            out[key] = int(m.group(1))
    return out


def _junit(path):
    """{classname::name: sorted outcome tags} from a JUnit file; None if the
    file is missing, unparsable or empty (a usage error writes none)."""
    try:
        root = ET.parse(path).getroot()
    except (OSError, ET.ParseError):
        return None
    res = {}
    for tc in root.iter("testcase"):
        tags = sorted(c.tag for c in tc if c.tag in ("failure", "error", "skipped"))
        res[f"{tc.get('classname')}::{tc.get('name')}"] = tags or ["passed"]
    return res or None


def _pytest(world, *args, env_extra=None):
    """The oracle: plain pytest from the worker venv."""
    env = dict(os.environ)
    for k in ("PYTEST_ADDOPTS", "PYTEST_CURRENT_TEST", "PYTEST_VERSION"):
        env.pop(k, None)
    # pytest writes a pipe in the locale encoding (cp1252 on Windows).
    env["PYTHONIOENCODING"] = "utf-8"
    env.update(env_extra or {})
    return subprocess.run(
        [str(venv_bin(world.gate.venv, "python")), "-m", "pytest", "-p", "no:cacheprovider", *args],
        cwd=world.project,
        env=env,
        capture_output=True,
        text=True,
        encoding="utf-8",
        timeout=120,
    )


def _quoted(text):
    return re.findall(r'"([^"]*)"', text)


# -- Given -------------------------------------------------------------------


@given(parsers.re(rf"the fixture event logger in {q('path')}"))
def _event_logger(world, path):
    world.write(path, _LOG)


@given(parsers.re(rf"the fixture event logger in {q('path')}, followed by:"))
def _event_logger_and(world, path, docstring):
    world.write(path, _LOG + "\n" + docstring + "\n")


# -- When: the pytest oracle -------------------------------------------------


@when("I run plain pytest")
def _plain_pytest(world):
    world.notes["pytest"] = _pytest(world)


@when(parsers.re(rf"I run plain pytest with {q('args')}"))
def _plain_pytest_args(world, args):
    world.notes["pytest"] = _pytest(world, *args.split())


@when("I run plain pytest with JUnit output")
def _plain_pytest_junit(world):
    path = world.gate.tmp / "pytest.xml"
    world.notes["pytest"] = _pytest(world, "--junitxml", str(path))
    world.notes["pytest_junit"] = _junit(path)


@when(parsers.re(r"I collect with plain pytest under PYTHONHASHSEED (?P<seeds>[\d, and]+)"))
def _collect_hashseeds(world, seeds):
    orders = []
    for seed in re.findall(r"\d+", seeds):
        p = _pytest(world, "--collect-only", "-q", env_extra={"PYTHONHASHSEED": seed})
        orders.append(tuple(ln for ln in p.stdout.splitlines() if "::" in ln))
    world.notes["orders"] = orders


# -- When: rstest ------------------------------------------------------------


@when(
    parsers.re(
        rf"I run {q('command')} with "
        r"(?P<outputs>a JSON report and JUnit output|a JSON report|JUnit output)"
    )
)
def _run_with_reports(world, command, outputs):
    extra = []
    if "JSON" in outputs:
        world.notes["report_json"] = world.gate.tmp / "report.json"
        extra += ["--report-json", str(world.notes["report_json"])]
    if "JUnit" in outputs:
        world.notes["junit"] = world.gate.tmp / "rstest.xml"
        world.notes["junit"].unlink(missing_ok=True)
        extra += ["--junitxml", str(world.notes["junit"])]
    world.run(" ".join([command, *map(shlex.quote, extra)]))


@when(parsers.re(rf"I run {q('command')} with PYTHONHASHSEED unset"))
def _run_no_hashseed(world, command):
    world.run(command, env_drop=("PYTHONHASHSEED",))


def _dirs(root):
    return {p.relative_to(root).as_posix() for p in root.rglob("*") if p.is_dir()}


@when(parsers.re(rf"I run {q('command')} (?P<times>\d+) times, noting the directories after each"))
def _run_times(world, command, times):
    world.notes["runs"] = []
    for _ in range(int(times)):
        r = world.run(command)
        world.notes["runs"].append({"result": r, "dirs": _dirs(world.project)})


@when(parsers.re(rf"I run {q('command')} (?P<times>\d+) times with PYTHONHASHSEED unset"))
def _run_times_no_hashseed(world, command, times):
    world.notes["runs"] = [
        {"result": world.run(command, env_drop=("PYTHONHASHSEED",))} for _ in range(int(times))
    ]


def _run_logged(world, command):
    base = world.gate.tmp / "events.log"
    for p in base.parent.glob(base.name + ".*"):
        p.unlink()
    r = world.run(command, env_extra={"MG_LOG": str(base)})
    rows = []
    for p in base.parent.glob(base.name + ".*"):
        for ln in p.read_text(encoding="utf-8").splitlines():
            if ln.strip():
                rows.append(json.loads(ln))
    return {"result": r, "events": sorted(rows, key=lambda x: x["t"])}


@when(parsers.re(rf"I run {q('command')} collecting the fixture event log"))
def _run_with_log(world, command):
    world.notes["events"] = _run_logged(world, command)["events"]


@when(parsers.re(rf"I run {q('command')} (?P<times>\d+) times collecting the fixture event log"))
def _run_times_with_log(world, command, times):
    world.notes["runs"] = [_run_logged(world, command) for _ in range(int(times))]


@when(parsers.re(rf"I run {q('command')} with the findings JSON"))
def _run_xdist_removal(world, command):
    path = world.gate.tmp / "findings.json"
    world.run(f"{command} --xdist-removal-json {shlex.quote(str(path))}")
    doc = json.loads(path.read_text(encoding="utf-8")) if path.is_file() else {"findings": []}
    world.notes["findings"] = doc["findings"]


@when(parsers.re(r"I read what (?P<a>\S+\.md) and (?P<b>\S+\.md) say about modifyitems reordering"))
def _read_reorder_claims(world, a, b):
    claims = {}
    for rel in (a, b):
        text = " ".join((REPO / rel).read_text(encoding="utf-8").split())
        claims[rel] = bool(
            re.search(r"[Rr]eordering[^.]{0,80}modifyitems[^.]{0,40} is ignored", text)
            or re.search(r"modifyitems[`*]* reordering is ignored", text)
        )
    world.notes["claims"] = claims


# -- Then: the pytest oracle -------------------------------------------------


def _oracle_tail(world):
    p = world.notes["pytest"]
    return f"pytest rc={p.returncode}\n{(p.stdout + p.stderr)[-400:]}"


@then(parsers.re(r"plain pytest exited with code (?P<code>\d+)"))
def _pytest_exit(world, code):
    assert world.notes["pytest"].returncode == int(code), _oracle_tail(world)


@then(parsers.re(rf"the last line of plain pytest's stdout contains {q('text')}"))
def _pytest_last_contains(world, text):
    assert text in _last(world.notes["pytest"].stdout), _oracle_tail(world)


@then(parsers.re(r"plain pytest's summary counts (?P<n>\d+) passed"))
def _pytest_passed(world, n):
    pc = _counts(_last(world.notes["pytest"].stdout))
    assert pc.get("passed", 0) == int(n), _oracle_tail(world)


@then("the collected order is not the same every time, and each lists 8 tests")
def _orders_differ(world):
    orders = world.notes["orders"]
    assert len(set(orders)) > 1 and all(len(o) == 8 for o in orders), (
        f"distinct orders={len(set(orders))} sizes={[len(o) for o in orders]}"
    )


# -- Then: parity with the oracle --------------------------------------------


def _parity(world):
    p, r = world.notes["pytest"], world.result
    pc, rc = _counts(_last(p.stdout)), _counts(_last(r.stdout))
    return p, r, pc, rc, f"pytest rc={p.returncode} {pc} | rstest rc={r.returncode} {rc}"


@then("rstest matches plain pytest: exit code, summary counts and per-test outcomes")
def _matches_pytest_junit(world):
    p, r, pc, rc, detail = _parity(world)
    pj, rj = world.notes["pytest_junit"], _junit(world.notes["junit"])
    diff = {k: (pj or {}).get(k) for k in set(pj or {}) ^ set(rj or {})}
    both = set(pj or {}) & set(rj or {})
    diff.update({k: (pj[k], rj[k]) for k in both if pj[k] != rj[k]})
    assert r.returncode == p.returncode and rc == pc and rj == pj, f"{detail} | diff={diff}"


@then("rstest matches plain pytest: exit code and summary counts")
def _matches_pytest(world):
    p, r, pc, rc, detail = _parity(world)
    assert r.returncode == p.returncode and rc == pc, f"{detail}\n{world.output[-200:]}"


@then("the summary counts match plain pytest's")
def _counts_match(world):
    _, _, pc, rc, detail = _parity(world)
    assert rc == pc, detail


# -- Then: report artifacts --------------------------------------------------


@then(parsers.re(r"the JSON report's meta\.counts\.failed is at least (?P<n>\d+)"))
def _report_failed(world, n):
    try:
        doc = json.loads(world.notes["report_json"].read_text(encoding="utf-8"))
        failed = doc["meta"]["counts"]["failed"]
    except (OSError, ValueError, KeyError):
        failed = None
    assert failed is not None and failed >= int(n), f"failed={failed}\n{world.tail()}"


@then(parsers.re(rf"the JUnit report has a <failure> for {q('test')}"))
def _junit_failure(world, test):
    junit = _junit(world.notes["junit"]) or {}
    assert "failure" in junit.get(test, []), str(junit)


# -- Then: repeated runs -----------------------------------------------------


def _summary(runs):
    return [(x["result"].returncode, _last(x["result"].stdout)) for x in runs]


@then(parsers.re(rf"every run succeeded with {q('text')} in the last line of stdout"))
def _every_run_passed(world, text):
    runs = world.notes["runs"]
    assert all(x["result"].returncode == 0 and text in _last(x["result"].stdout) for x in runs), (
        str(_summary(runs))
    )


@then(parsers.re(r"after every run (?P<paths>.+) were directories"))
def _every_run_dirs(world, paths):
    want = set(_quoted(paths))
    runs = world.notes["runs"]
    assert all(want <= x["dirs"] for x in runs), str(
        [sorted(d for d in x["dirs"] if d.count("/") <= 1) for x in runs]
    )


@then(parsers.re(rf"every run failed with {q('text')} not in stdout"))
def _every_run_flagged(world, text):
    runs = world.notes["runs"]
    tries = [(x["result"].returncode, text not in x["result"].stdout) for x in runs]
    assert all(rc != 0 and flagged for rc, flagged in tries), str(tries)


# -- Then: xdist-removal-check findings --------------------------------------


def _imports(world):
    return [f for f in world.notes["findings"] if f.get("kind") == "import"]


@then(parsers.re(r"there are exactly (?P<n>\d+) import findings, on distinct locations"))
def _import_count(world, n):
    found = _imports(world)
    locations = {f["location"] for f in found}
    assert len(found) == int(n) and len(locations) == int(n), (
        f"rc={world.result.returncode} {sorted(locations)}"
    )


def _import_at(world, location):
    return {f["location"]: f for f in _imports(world)}.get(location, {})


@then(parsers.re(rf"the fix for the import finding at {q('location')} contains {q('text')}"))
def _import_fix_contains(world, location, text):
    fix = _import_at(world, location).get("fix", "")
    assert text in fix, fix


@then(
    parsers.re(
        rf"the import finding at {q('location')} has a fix that does not contain {q('text')}"
    )
)
def _import_fix_lacks(world, location, text):
    fix = _import_at(world, location).get("fix", "")
    assert fix and text not in fix, fix


@then(
    parsers.re(
        rf"exactly one finding is located in addopts, its text contains {q('needle')} "
        r"and it has a fix"
    )
)
def _addopts_finding(world, needle):
    hits = [f for f in world.notes["findings"] if "addopts" in f.get("location", "")]
    assert len(hits) == 1 and needle in hits[0].get("text", "") and bool(hits[0].get("fix")), (
        f"rc={world.result.returncode} " + str(hits)[:300]
    )


# -- Then: flags and docs ----------------------------------------------------


@then(
    parsers.re(
        rf"the run succeeds with {q('passed')} in stdout, or exits 4 with {q('hint')} "
        r"in the output"
    )
)
def _reaches_plugin_or_hints(world, passed, hint):
    r = world.result
    ok = (r.returncode == 0 and passed in r.stdout) or (r.returncode == 4 and hint in world.output)
    assert ok, world.tail(300)


def _help_options(world):
    return set(re.findall(r"^\s+(?:-\w, )?(--[a-z][a-z0-9-]*)", world.result.stdout, re.M))


@then(parsers.re(r"the help lists the options (?P<flags>.+)"))
def _help_lists(world, flags):
    owned = _help_options(world)
    assert set(_quoted(flags)) <= owned, str(sorted(owned))[:300]


@then(
    parsers.re(
        r"each of these options that the help lists is in the first column of (?P<doc>\S+\.md):"
    )
)
def _docs_list_flags(world, doc, datatable):
    owned = _help_options(world)
    table = (REPO / doc).read_text(encoding="utf-8")
    # First column of each table row only: the prose cells mention flags too.
    first = [ln.split("|")[1] for ln in table.splitlines() if ln.startswith("|")]
    listed = {f for cell in first for f in re.findall(r"`(--[a-z][a-z0-9-]*)", cell)}
    rows = [(flag, plugin) for flag, plugin in datatable[1:] if flag in owned]
    missing = [f"{flag} ({plugin})" for flag, plugin in rows if flag not in listed]
    assert not missing, f"missing={missing} listed={sorted(listed)}"


@then("both docs make the same claim about whether it is ignored")
def _docs_agree(world):
    claims = world.notes["claims"]
    assert len(set(claims.values())) == 1, str(claims)


# -- Then: fixture event log -------------------------------------------------


@then(
    parsers.re(
        r"(?P<n>\d+) events came from (?P<w>\d+) distinct workers, "
        r"all with one uid equal to its env"
    )
)
def _uid_events(world, n, w):
    rows = world.notes["events"]
    uids = {(x["uid"], x["env"]) for x in rows}
    workers = sorted({x["w"] for x in rows})
    assert (
        len(rows) == int(n)
        and len(workers) == int(w)
        and len(uids) == 1
        and all(u == e for u, e in uids)
    ), f"{uids} workers={workers}"


def _group_runs(world):
    out = []
    for x in world.notes["runs"]:
        rows = x["events"]
        db = sorted({e["w"] for e in rows if e["name"] == "db"})
        net = sorted({e["w"] for e in rows if e["name"] == "net"})
        span = max(e["end"] for e in rows) - min(e["start"] for e in rows) if rows else 99
        out.append((x["result"].returncode, len(rows), db, net, round(span, 2)))
    return out


@then(
    parsers.re(
        rf"every run succeeded with (?P<n>\d+) events, the {q('a')} and the {q('b')} events "
        r"each on one worker"
    )
)
def _groups_cohesive(world, n, a, b):
    assert (a, b) == ("db", "net"), "group names are fixed by the fixture"
    runs = _group_runs(world)
    assert all(
        rc == 0 and k == int(n) and len(d) == 1 and len(t) == 1 for rc, k, d, t, _ in runs
    ), str(runs)


@then(parsers.re(rf"in every run the {q('a')} and {q('b')} groups ran on different workers"))
def _groups_apart(world, a, b):
    assert (a, b) == ("db", "net"), "group names are fixed by the fixture"
    runs = _group_runs(world)
    assert all(d != t for _, _, d, t, _ in runs), str(runs)


@then(parsers.re(rf"in every run the {q('a')} and {q('b')} groups overlapped in time"))
def _groups_overlap(world, a, b):
    """Each group's window runs from its first start to its last end; the two
    windows must intersect. A wall-clock bound flakes on slow runners."""
    windows = []
    for x in world.notes["runs"]:
        rows = x["events"]
        win = {
            g: (min(e["start"] for e in ev), max(e["end"] for e in ev))
            for g in (a, b)
            if (ev := [e for e in rows if e["name"] == g])
        }
        windows.append(win)
    overlap = [len(w) == 2 and max(w[a][0], w[b][0]) < min(w[a][1], w[b][1]) for w in windows]
    assert all(overlap), str(windows)


@then(parsers.re(rf"both {q('ev')} events came from one worker"))
def _serial_one_worker(world, ev):
    ser = [x for x in world.notes["events"] if x["ev"] == ev]
    assert len(ser) == 2 and len({x["w"] for x in ser}) == 1, str(ser)


@then(
    parsers.re(
        rf"no {q('ev')} event falls between the serial events, "
        r"and the serial worker has exactly one"
    )
)
def _serial_reuses_session(world, ev):
    rows = world.notes["events"]
    ser = [x for x in rows if x["ev"] == "serial"]
    ups = [x for x in rows if x["ev"] == ev]
    between = [x for x in ups if ser and ser[0]["t"] <= x["t"] <= ser[-1]["t"]]
    on_ser_worker = [x for x in ups if ser and x["w"] == ser[0]["w"]]
    assert not between and len(on_ser_worker) == 1, (
        f"{ev} between serials={len(between)} on serial worker={len(on_ser_worker)}"
    )


@then(
    parsers.re(
        r"(?P<n>\d+) events were logged, each worker's \"i\" values descend, "
        r"and (?P<first>\d+) is among the first two"
    )
)
def _reversed_dispatch(world, n, first):
    rows = world.notes["events"]
    per_worker = {}
    for x in rows:
        per_worker.setdefault(x["w"], []).append(x["i"])
    assert (
        len(rows) == int(n)
        and all(seq == sorted(seq, reverse=True) for seq in per_worker.values())
        and int(first) in [x["i"] for x in rows[:2]]
    ), str(per_worker)
