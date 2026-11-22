"""How often does `--incremental` skip what it could, after a one-function edit?

For each suite: one green `--cov --cov-context=test` run warms the coverage
index, one `--incremental` run with no edit is the baseline (what always
runs: tests that were not green). Then, per probe, edit one function, run
`--incremental`, read which tests ran (report-json entries without
`"cached": true`), restore the file and the cache, and compare:

  needed   tests whose covered lines include the edited function (from the
           coverage index before the edit; for a test function, the tests
           that ran it, subclasses inheriting it included); none for a
           comment-only edit
  ran      tests executed this run, minus the baseline's always-run set
  excess   ran but not needed: what a finer cache key could still skip
  missed   needed but skipped: a false green, must be 0
  next     with --follow-up: tests the next run (edit kept, nothing new)
           executes, minus the always-run set; ideally 0

Edit kinds:

  body     a statement added at the top of a source function's body
  comment  a comment line added at the same place (no code change)
  test     a statement added to one test function's body

Each suite runs with its own cache (`RSTEST_CACHE`), snapshotted after the
warm-up and restored after every probe, so the corpus checkouts' caches are
untouched. pytest-cov is installed into the suite venv for the measurement and
uninstalled after. Edited files are restored even on error.

    python3 corpus/run.py --prepare-only --only click,itsdangerous
    python3 corpus/incremental_hits.py --only click,itsdangerous --probes 10
    python3 corpus/incremental_hits.py --only click \\
        --out corpus/bench-results/<date>-incremental-hits.json
"""

from __future__ import annotations

import argparse
import ast
import json
import random
import shutil
import signal
import statistics
import sys
import tempfile
from pathlib import Path

import tomllib
from run import HERE, REPO, Suite, log, sh

KINDS = ("body", "comment", "test")
SKIP_DIRS = {".git", "venv", ".venv", "node_modules", "build", "dist", ".tox", ".nox"}


def installed(suite, names):
    """Which of `names` are already installed in the suite venv."""
    r = sh(["uv", "pip", "list", "-p", str(suite.venv), "--format", "json"])
    present = {p["name"].lower() for p in json.loads(r.stdout or "[]")}
    return {n for n in names if n in present}


def run_incremental(suite, cache, extra, snap):
    """One rstest run on `suite` with its cache at `cache`; the report."""
    snap.unlink(missing_ok=True)
    env = dict(suite.env(), RSTEST_CACHE=str(cache))
    argv = [
        str(suite.rstest_bin),
        "-n",
        "4",
        "--report-json",
        str(snap),
        "--cov=.",
        "--cov-context=test",
        "--cov-report=",
        *extra,
        *suite.cfg.get("args", []),
    ]
    r = sh(argv, cwd=suite.cwd(), env=env)
    if not snap.exists():
        raise RuntimeError(f"no report (rc={r.returncode}): {(r.stdout or '')[-400:]}")
    # rstest's own account of what it skipped or why it skipped nothing.
    notes = [ln for ln in (r.stderr or "").splitlines() if "incremental" in ln]
    report = json.loads(snap.read_text())
    if report.get("collect_errors"):
        notes.append(f"collect errors: {report['collect_errors']}")
        return None, notes
    return report["tests"], notes


def ran(tests):
    return {t for t, e in tests.items() if not e.get("cached")}


def index_files(cache):
    """The coverage index: file -> {line: [nodeids]}."""
    raw = json.loads((cache / "coverage_index.json").read_text())
    return {
        f: {int(k): set(v) for k, v in d.get("lines", {}).items()} for f, d in raw["files"].items()
    }


def is_test_file(p: Path):
    return p.name.startswith("test_") or p.name.endswith("_test.py") or p.name == "conftest.py"


def functions(root: Path):
    """(file, def node) for every function in the checkout's Python files."""
    for p in sorted(root.rglob("*.py")):
        if SKIP_DIRS & set(p.relative_to(root).parts):
            continue
        try:
            tree = ast.parse(p.read_text())
        except (SyntaxError, UnicodeDecodeError):
            continue
        lines = p.read_text().splitlines()
        for node in ast.walk(tree):
            if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)) and node.body:
                # A body on the header's own line (`def f(): ...`) has no line
                # of its own to insert a probe above.
                first = node.body[0]
                if lines[first.lineno - 1][: first.col_offset].strip():
                    continue
                yield p, node


def insert_at(node):
    """1-based line to insert before: the first body statement after a
    docstring, above its decorators when it is a decorated def or class (a
    decorated statement's own line is its `def`, after the decorators)."""
    body = node.body
    first = body[0]
    if (
        len(body) > 1
        and isinstance(first, ast.Expr)
        and isinstance(getattr(first, "value", None), ast.Constant)
        and isinstance(first.value.value, str)
    ):
        first = body[1]
    line = min([first.lineno, *(d.lineno for d in getattr(first, "decorator_list", []))])
    return line, first.col_offset


def pick_probes(root, index, rng, n):
    """Up to `n` probes per kind: (kind, file, node, needed nodeids)."""
    src, tests = [], []
    for p, node in functions(root):
        rel = str(p.relative_to(root))
        # Tests whose covered lines fall in this function: for a test function
        # that includes subclasses that inherit it (they run its lines).
        lines = index.get(rel, {})
        head = min([d.lineno for d in node.decorator_list] + [node.lineno])

        def covered(first, last=node.end_lineno, lines=lines):
            return set().union(*(lines.get(ln, set()) for ln in range(first, last + 1)))

        # Probes are picked by any covered line of the def (stable sample);
        # the tests that need to run are the ones that ran its body. A covered
        # header alone means the module was executed (a test re-importing it),
        # except for a docstring-only body, whose call records only the header.
        doc_only = len(node.body) == 1 and ast.get_docstring(node, clean=False) is not None
        covering = covered(head if doc_only else node.body[0].lineno)
        if is_test_file(p):
            if node.name.startswith("test") and covered(node.lineno):
                tests.append((p, node, covering))
            continue
        if covered(node.lineno):
            src.append((p, node, covering))
    probes = []
    for p, node, covering in rng.sample(src, min(n, len(src))):
        probes += [("body", p, node, covering), ("comment", p, node, covering)]
    for p, node, covering in rng.sample(tests, min(n, len(tests))):
        probes.append(("test", p, node, covering))
    return probes


def edit(p, node, kind):
    """Apply the probe's edit; returns the original text."""
    original = p.read_text()
    lines = original.splitlines(keepends=True)
    line, col = insert_at(node)
    pad = " " * col
    new = (
        f"{pad}# rstest incremental probe\n"
        if kind == "comment"
        else f"{pad}_rstest_probe = None\n"
    )
    lines.insert(line - 1, new)
    p.write_text("".join(lines))
    return original


def needed_tests(kind, covering):
    if kind == "comment":
        return set()  # a comment changes no behaviour: nothing needs to run
    return covering


def measure(suite, probes_per_kind, seed, follow_up=False):
    work = Path(tempfile.mkdtemp(prefix=f"inc-{suite.name}-"))
    cache, snapshot, snap = work / "cache", work / "cache.snapshot", work / "report.json"
    root = suite.cwd()
    log(f"  {suite.name}: warming the coverage index")
    green, _ = run_incremental(suite, cache, [], snap)
    # The green set is recorded only by --incremental runs: this one records
    # it (and runs everything), the next is the no-edit baseline.
    run_incremental(suite, cache, ["--incremental"], snap)
    baseline = ran(run_incremental(suite, cache, ["--incremental"], snap)[0])
    total = len(green)
    log(f"  {suite.name}: {total} tests, {len(baseline)} always run with no edit")
    shutil.copytree(cache, snapshot)
    index = index_files(cache)
    probes = pick_probes(root, index, random.Random(seed), probes_per_kind)
    rows = []
    for kind, p, node, covering in probes:
        original = edit(p, node, kind)
        nxt = None
        try:
            tests, notes = run_incremental(suite, cache, ["--incremental"], snap)
            if follow_up and tests is not None:
                # The run after, edit kept: what the first one left skippable.
                nxt = run_incremental(suite, cache, ["--incremental"], snap)[0]
        finally:
            p.write_text(original)
            shutil.rmtree(cache)
            shutil.copytree(snapshot, cache)
        if tests is None:
            # The probe broke collection: not a measurement, report and skip it.
            log(f"    {kind:7} {p.relative_to(root)}::{node.name}: INVALID probe, {notes[-1]}")
            continue
        executed = ran(tests) - baseline
        needed = needed_tests(kind, covering) - baseline
        row = {
            "kind": kind,
            "where": f"{p.relative_to(root)}::{node.name}",
            "total": len(tests),
            "needed": len(needed),
            "ran": len(executed),
            "excess": len(executed - needed),
            "missed": sorted(needed - ran(tests)),
            "notes": notes,
        }
        if nxt is not None:
            row["next"] = len(ran(nxt) - baseline)
        rows.append(row)
        log(
            f"    {kind:7} {row['where']}: ran {row['ran']}, needed {row['needed']}, "
            f"excess {row['excess']}"
            + (f", next run {row['next']}" if "next" in row else "")
            + (f", MISSED {len(row['missed'])}" if row["missed"] else "")
        )
        for n in notes:
            log(f"        {n}")
    shutil.rmtree(work)
    return {"suite": suite.name, "tests": total, "always_run": len(baseline), "probes": rows}


def summarize(result):
    out = []
    for kind in KINDS:
        rows = [r for r in result["probes"] if r["kind"] == kind]
        if not rows:
            continue
        t = result["tests"] - result["always_run"]
        ran_share = [r["ran"] / t for r in rows] if t else [0]
        ideal_share = [r["needed"] / t for r in rows] if t else [0]
        nxt = [r["next"] / t for r in rows if "next" in r and t]
        out.append(
            f"| {result['suite']} | {kind} | {len(rows)} "
            f"| {statistics.median(ideal_share):.1%} | {statistics.median(ran_share):.1%} "
            f"| {sum(r['excess'] for r in rows) / len(rows):.0f} "
            f"| {sum(len(r['missed']) for r in rows)} |"
            + (f" {statistics.median(nxt):.1%} |" if nxt else "")
        )
    return out


def main():
    # SIGTERM (a stopped run) goes through the `finally` blocks, so an edited
    # file and the venv's pytest-cov are restored.
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(1))
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--only", required=True, help="comma-separated corpus suites (prepared)")
    ap.add_argument("--probes", type=int, default=10, help="functions per edit kind (default 10)")
    ap.add_argument("--seed", type=int, default=0)
    ap.add_argument("--out", help="write the per-probe rows as JSON here")
    ap.add_argument("--binary", help="rstest binary (default: target/release/rstest)")
    ap.add_argument(
        "--follow-up",
        action="store_true",
        help="after each probe, run once more with the edit kept (should rerun nothing)",
    )
    args = ap.parse_args()
    cfg = tomllib.loads((HERE / "suites.toml").read_text())
    binary = Path(args.binary) if args.binary else REPO / "target" / "release" / "rstest"
    results = []
    for name in args.only.split(","):
        suite = Suite(name, cfg[name], None, binary)
        had = installed(suite, ("pytest-cov", "coverage"))
        log(f"{name}: installing pytest-cov for the measurement")
        suite.pip("pytest-cov")
        try:
            results.append(measure(suite, args.probes, args.seed, args.follow_up))
        finally:
            added = [p for p in ("pytest-cov", "coverage") if p not in had]
            if added:
                sh(["uv", "pip", "uninstall", "-p", str(suite.venv), *added])
    print(
        "\n| suite | edit | probes | must run (median) | ran (median) "
        "| excess tests (mean) | missed |" + (" next run (median) |" if args.follow_up else "")
    )
    print("|---|---|---:|---:|---:|---:|---:|" + ("---:|" if args.follow_up else ""))
    for r in results:
        print("\n".join(summarize(r)))
    print("\nmust run / ran: share of the suite (always-run tests excluded). missed must be 0.")
    if args.out:
        Path(args.out).write_text(json.dumps(results, indent=1) + "\n")


if __name__ == "__main__":
    main()
