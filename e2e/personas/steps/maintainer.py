"""Steps for features/maintainer.feature: the suite maintainer."""

import json
import operator
import re
import shlex
import subprocess
import time
import uuid

from _harness import REPO, venv_bin
from pytest_bdd import given, parsers, then, when

from steps.common import q

_OPS = {"<": operator.lt, "<=": operator.le, ">": operator.gt, ">=": operator.ge, "==": operator.eq}
_OP = r"(?P<op><=|>=|==|<|>)"
_NUM = r"(?P<value>-?\d+(?:\.\d+)?)"
_MISSING = object()


def _json(path):
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}


def _field(doc, dotted):
    """`a.0.b` walks dicts by key and lists by index; _MISSING when absent."""
    cur = doc
    for part in dotted.split("."):
        if isinstance(cur, list) and part.isdigit() and int(part) < len(cur):
            cur = cur[int(part)]
        elif isinstance(cur, dict) and part in cur:
            cur = cur[part]
        else:
            return _MISSING
    return cur


def _env(spec):
    return dict(kv.split("=", 1) for kv in shlex.split(spec))


def _totals(text):
    return [" ".join(ln.split()) for ln in text.splitlines() if ln.startswith("TOTAL")]


# -- running ------------------------------------------------------------------


@given(parsers.re(rf"I have run {q('command')} with environment {q('env')}"))
@when(parsers.re(rf"I run {q('command')} with environment {q('env')}"))
def _run_with_env(world, command, env):
    world.run(command, env_extra=_env(env))


@when(parsers.re(rf"I run {q('command')} and time it as {q('label')}"))
def _run_timed(world, command, label):
    t0 = time.monotonic()
    world.run(command)
    world.notes.setdefault("wall", {})[label] = time.monotonic() - t0


@when(parsers.re(rf"I run {q('command')} (?P<times>\d+) times, keeping each result"))
def _run_repeatedly(world, command, times):
    world.notes["repeats"] = [world.run(command) for _ in range(int(times))]


@then(parsers.re(rf"every one of those runs exited 1 with {q('text')} in stdout"))
def _repeats_failed(world, text):
    runs = world.notes["repeats"]
    fails = sum(r.returncode == 1 and text in r.stdout for r in runs)
    assert fails == len(runs), f"{fails}/{len(runs)}"


# -- timing -------------------------------------------------------------------


def _speedup(world, slow, fast):
    wall = world.notes["wall"]
    return wall[slow] / max(wall[fast], 1e-6)


@then(
    parsers.re(
        rf"the {q('slow')} run took at least (?P<ratio>[\d.]+) times as long as the {q('fast')} run"
    )
)
def _took_longer(world, slow, ratio, fast):
    actual = _speedup(world, slow, fast)
    assert actual >= float(ratio), f"walls={world.notes['wall']} ratio={actual:.2f}"


@then(
    parsers.re(
        rf"the doctor realized speedup in {q('path')} is within (?P<pct>\d+)% of the "
        rf'"(?P<slow>[^"/]*)"/"(?P<fast>[^"]*)" wall-time ratio'
    )
)
def _realized_speedup(world, path, pct, slow, fast):
    actual = _speedup(world, slow, fast)
    pe = _json(world.project / path).get("parallel_efficiency") or {}
    realized = pe.get("realized_speedup", 0.0)
    assert actual > 0 and abs(realized / actual - 1) <= int(pct) / 100, (
        f"realized={realized:.2f} actual={actual:.2f}"
    )


# -- JSON files ---------------------------------------------------------------


@then(parsers.re(rf"the JSON file {q('path')} has {q('field')} {_OP} {_NUM}"))
def _json_field(world, path, field, op, value):
    got = _field(_json(world.project / path), field)
    assert got is not _MISSING and got is not None, f"{field} missing from {path}"
    assert _OPS[op](got, float(value)), f"{field}={got!r}, expected {op} {value}"


@then(parsers.re(rf"the JSON file {q('path')} has {q('field')} {_OP} {_NUM} or no such field"))
def _json_field_if_present(world, path, field, op, value):
    got = _field(_json(world.project / path), field)
    if got is not _MISSING:
        assert _OPS[op](got, float(value)), f"{field}={got!r}, expected {op} {value}"


# -- stdout / stderr ----------------------------------------------------------


@then(parsers.re(rf"stdout contains {q('a')} or {q('b')}"))
def _stdout_either(world, a, b):
    assert a in world.result.stdout or b in world.result.stdout, world.tail()


@then(parsers.re(rf"the {q('marker')} section of stdout does not contain {q('text')}"))
def _stdout_section_lacks(world, marker, text):
    """A section runs from its header to the next blank line; an absent
    section passes. Later sections (PARALLEL FLOOR, SLOWEST TESTS) can
    legitimately name the same test, so they must not be searched."""
    _, found, rest = world.result.stdout.partition(marker)
    section = rest.split("\n\n", 1)[0] if found else ""
    assert text not in section, world.tail()


@then(parsers.re(rf"the last line of stderr contains {q('text')}"))
def _last_stderr_line(world, text):
    lines = world.result.stderr.splitlines()
    last = lines[-1] if lines else ""
    assert text in last, f"last stderr line: {last!r}"


# -- doctor gates -------------------------------------------------------------


@when(parsers.re(rf"I run {q('command')} with these doctor gates:"))
def _run_doctor_gates(world, command, datatable):
    conds = [row[0] for row in datatable[1:]]
    flags = " ".join(f"--doctor-fail-on {shlex.quote(c)}" for c in conds)
    world.run(f"{command} {flags}")


@then(parsers.re(rf"the doctor gate {q('cond')} fires"))
def _gate_fires(world, cond):
    """A breach line (`  wait_pct = 44.00 > 10.00 (wait_pct>10)`) and never
    `'cond' not measured`."""
    stderr = world.result.stderr
    metric = re.split(r"[<>=!]", cond, maxsplit=1)[0]
    breached = re.search(rf"^\s+{re.escape(metric)} = ", stderr, re.M) is not None
    line = next((ln for ln in stderr.splitlines() if cond in ln), "")[:160]
    assert breached and f"'{cond}' not measured" not in stderr, f"{line!r}\n{world.tail()}"


# -- durations ----------------------------------------------------------------


@then(parsers.re(rf"rstest explain {q('nodeid')} reports a duration of at least (?P<secs>[\d.]+)s"))
def _explain_duration(world, nodeid, secs):
    # Its own run: world.result stays the last test run.
    r = world.gate.run("explain", nodeid, cwd=world.project)
    m = re.search(r"duration\s+([\d.]+)s", r.stdout)
    got = float(m.group(1)) if m else None
    assert got is not None and got >= float(secs), f"explain duration={got}\n{r.stdout[-300:]}"


# -- coverage -----------------------------------------------------------------


def _pytest(world, command):
    argv = shlex.split(command)
    assert argv and argv[0] == "pytest", f"command must start with 'pytest': {command!r}"
    py = str(venv_bin(world.gate.venv, "python"))
    return subprocess.run(
        [py, "-m", *argv], cwd=world.project, capture_output=True, text=True, timeout=120
    )


@when(parsers.re(rf"I run {q('command')} in the worker venv"))
def _run_pytest(world, command):
    world.result = _pytest(world, command)


@given("pytest-cov's coverage TOTAL lines for the project have been recorded")
def _cov_oracle(world):
    o = _pytest(world, "pytest -q -p no:cacheprovider --cov=pkg --cov-report=term")
    world.notes["cov_oracle"] = _totals(o.stdout)


@then(parsers.re(rf"stdout has exactly one coverage TOTAL line, ending in {q('suffix')}"))
def _one_total(world, suffix):
    totals = _totals(world.result.stdout)
    assert len(totals) == 1 and totals[0].endswith(suffix), f"{totals}\n{world.tail(200)}"


@when(parsers.re(rf"I run {q('command')} and note its coverage TOTAL lines"))
def _run_noting_totals(world, command):
    world.run(command)
    world.notes.setdefault("cov_totals", []).extend(_totals(world.output))


@then("the noted coverage TOTAL lines are all pytest-cov's TOTAL line")
def _noted_totals(world):
    seen, oracle = world.notes.get("cov_totals", []), world.notes["cov_oracle"]
    assert seen and set(seen) == set(oracle), f"rstest={sorted(set(seen))} pytest={oracle}"


@then("the coverage TOTAL lines of this run are exactly pytest-cov's")
def _run_totals(world):
    totals, oracle = _totals(world.output), world.notes["cov_oracle"]
    assert totals == oracle, f"rstest={totals} pytest={oracle}"


# -- pool scheduling ----------------------------------------------------------


@when(parsers.re(rf"I run {q('command')} with a fresh MT_LOG_DIR"))
def _run_logging(world, command):
    log = world.gate.tmp / f"log-{uuid.uuid4().hex}"
    log.mkdir()
    world.notes["log"] = log
    world.run(command, env_extra={"MT_LOG_DIR": str(log)})


def _ran(world):
    """test name -> worker id, from the `ran.<name>` files."""
    return {p.name[4:]: p.read_text() for p in world.notes["log"].glob("ran.*")}


@then(parsers.re(r"(?P<n>\d+) tests logged a run in MT_LOG_DIR"))
def _logged_runs(world, n):
    ran = _ran(world)
    assert len(ran) == int(n), f"ran={ran}"


@then(parsers.re(rf"the logged {q('prefix')} tests all ran on one worker"))
def _one_worker(world, prefix):
    workers = {w for k, w in _ran(world).items() if k.startswith(prefix)}
    assert len(workers) == 1, f"ran={_ran(world)}"


@then(parsers.re(rf"the logged session set-ups equal the workers that ran a {q('prefix')} test"))
def _setups_per_worker(world, prefix):
    ups = list(world.notes["log"].glob("up.*"))
    workers = {w for k, w in _ran(world).items() if k.startswith(prefix)}
    assert len(ups) == len(workers), f"setups={len(ups)} workers={sorted(workers)}"


def _report_workers(world, path, name):
    tests = _json(world.project / path).get("tests", {})
    return [t.get("worker") for k, t in tests.items() if name in k]


@then(parsers.re(rf"the report {q('path')} has (?P<n>\d+) {q('name')} tests"))
def _report_has_tests(world, path, n, name):
    workers = _report_workers(world, path, name)
    assert len(workers) == int(n), f"workers={workers}"


@then(parsers.re(rf"the {q('name')} tests in {q('path')} each ran on a different worker"))
def _distinct_workers(world, name, path):
    workers = _report_workers(world, path, name)
    assert len(set(workers)) == len(workers), f"workers={workers}"


# -- reproducing order-dependent failures -------------------------------------


@when(
    parsers.re(
        rf"I try {q('command')} with --shuffle seeds (?P<lo>\d+) to (?P<hi>\d+) "
        rf"until one exits 1 naming {q('text')}"
    )
)
def _hunt_seed(world, command, lo, hi, text):
    world.notes["seed"] = None
    for seed in range(int(lo), int(hi) + 1):
        r = world.run(f"{command} --shuffle={seed}")
        if r.returncode == 1 and text in r.stdout:
            world.notes["seed"] = seed
            return


@then("a shuffle seed hit the failure")
def _seed_found(world):
    assert world.notes["seed"] is not None, "no seed reproduced the failure"


@given(parsers.re(rf"the {q('heading')} section of {q('doc')}"))
def _docs_section(world, heading, doc):
    text = (REPO / doc).read_text(encoding="utf-8")
    world.notes["docs"] = text.split(heading, 1)[-1].split("\n### ", 1)[0]


@then(parsers.re(rf"that docs section contains {q('text')}"))
def _docs_contains(world, text):
    assert text in world.notes["docs"], world.notes["docs"][:200]


@then(parsers.re(rf"that docs section does not contain {q('text')}"))
def _docs_lacks(world, text):
    assert text not in world.notes["docs"], world.notes["docs"][:200]


# -- flaky policy -------------------------------------------------------------


@when(
    parsers.re(
        rf"I run the flaky-policy suite with {q('command')}(?P<mark> and the @flaky mark on)?"
    )
)
def _run_policy(world, command, mark):
    """Each run gets a fresh first-attempt marker and its own report."""
    report = world.gate.tmp / f"r-{uuid.uuid4().hex}.json"
    env = {"MT_FLAKY_MARKER": str(world.gate.tmp / f"m-{uuid.uuid4().hex}")}
    if mark:
        env["MT_MARK"] = "1"
    world.notes["report"] = report
    world.run(f"{command} --report-json {shlex.quote(str(report))}", env_extra=env, timeout=60)


def _counts(world):
    return _json(world.notes["report"]).get("meta", {}).get("counts", {})


@then(parsers.re(rf"the policy report counts are {q('spec')}"))
def _counts_are(world, spec):
    c = _counts(world)
    want = {k: int(v) for k, v in _env(spec).items()}
    got = {k: c.get(k) for k in want}
    assert got == want, f"rc={world.result.returncode} counts={c}"


@then(parsers.re(r"the policy report counts at least (?P<n>\d+) (?P<kind>\w+)"))
def _counts_at_least(world, n, kind):
    c = _counts(world)
    assert c.get(kind, 0) >= int(n), f"rc={world.result.returncode} counts={c}"
