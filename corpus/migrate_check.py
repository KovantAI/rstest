#!/usr/bin/env python3
"""Prototype: the unstable-nodeid detector for `rstest --migrate-check` (M1).

Run a suite's collection TWICE in fresh rstest processes and diff the id sets.
IDs present in one collection but not the other are run-to-run unstable — the
class that makes per-worker collections disagree and forces rstest to bail
("workers collected different test sets"), pinning the suite to -n 0.

This is a reference implementation to validate the detector + classifier
against the corpus before porting to Rust (see DESIGN-migrate-check.md). It is
NOT the shipped feature — it shells out to `rstest --collect-only`.

Usage:
    python3 corpus/migrate_check.py <suite-name> [--runs N]

Classifies each unstable id by why its parametrization is non-deterministic:
    address  — embeds a memory address (0x...): differs every process -> WILL bail
    uuid     — embeds a uuid4: differs every process -> WILL bail
    time     — embeds a timestamp/date: MAY bail (depends on collection timing)
    other    — unstable for an unrecognized reason
"""
import argparse
import json
import os
import re
import subprocess
import sys
from collections import defaultdict
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent
RSTEST = REPO / "target" / "release" / "rstest"

# classifiers: ordered, first match wins. Operate on the parametrize segment.
PATTERNS = [
    ("address", re.compile(r"0x[0-9a-fA-F]{6,}")),
    ("uuid", re.compile(r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-"
                        r"[0-9a-fA-F]{4}-[0-9a-fA-F]{12}")),
    ("time", re.compile(r"\b\d{4}-\d{2}-\d{2}\b|\b\d{2}:\d{2}:\d{2}\b"
                        r"|\bdatetime\b|\d{10,}")),
]

# "WILL bail" classes are per-process-unstable; "MAY bail" depend on timing.
WILL_BAIL = {"address", "uuid"}


def suite_dir(name):
    work = HERE / "work" / name
    src = work / "src"
    return (src if src.is_dir() else work), work


def collect_ids(cwd, venv):
    """One fresh `rstest --collect-only --report-json` -> set of nodeids."""
    out = Path(f"/tmp/migrate-collect-{os.getpid()}-{collect_ids.n}.json")
    collect_ids.n += 1
    env = dict(os.environ, VIRTUAL_ENV=str(venv), PATH=f"{venv}/bin:{os.environ['PATH']}")
    subprocess.run(
        [str(RSTEST), "-n", "0", "--collect-only", "--report-json", str(out), "-q"],
        cwd=cwd, env=env, capture_output=True,
    )
    if not out.exists():
        sys.exit(f"no collection snapshot produced for {cwd}")
    data = json.loads(out.read_text())
    out.unlink(missing_ok=True)
    tests = data.get("tests", data) if isinstance(data, dict) else data
    if isinstance(tests, dict):            # nodeid -> entry
        return set(tests.keys())
    # list of entries (collect-only) or of nodeid strings
    return {t["nodeid"] if isinstance(t, dict) else t for t in tests}


collect_ids.n = 0


def site_of(nodeid):
    """The parametrization site: nodeid with the [..] param stripped."""
    return re.sub(r"\[.*\]$", "", nodeid)


def param_of(nodeid):
    m = re.search(r"\[(.*)\]$", nodeid)
    return m.group(1) if m else ""


def classify(param):
    for name, rx in PATTERNS:
        if rx.search(param):
            return name
    return "other"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("suite")
    ap.add_argument("--runs", type=int, default=2)
    args = ap.parse_args()

    cwd, work = suite_dir(args.suite)
    venv = work / "venv"
    if not venv.is_dir():
        sys.exit(f"no prepared venv at {venv} (run corpus/run.py --prepare-only)")

    runs = [collect_ids(cwd, venv) for _ in range(args.runs)]
    base = runs[0]
    union = set().union(*runs)
    inter = set(base).intersection(*runs[1:])
    unstable = union - inter  # present in some runs, absent in others

    total = len(union)
    print(f"suite: {args.suite}  ({args.runs} collections)")
    print(f"  collected (union): {total}   stable across all runs: {len(inter)}")
    if not unstable:
        print("  UNSTABLE NODEIDS: none — collection is reproducible. "
              "No parallel-dispatch bail from id instability.")
        return 0

    # group unstable ids by site + class
    by_site = defaultdict(lambda: defaultdict(list))
    for nid in unstable:
        by_site[site_of(nid)][classify(param_of(nid))].append(nid)

    will = sum(1 for nid in unstable if classify(param_of(nid)) in WILL_BAIL)
    print(f"  UNSTABLE NODEIDS: {len(unstable)} across {len(by_site)} sites "
          f"({will} per-process => WILL bail at -n auto)\n")
    for site in sorted(by_site):
        classes = by_site[site]
        kinds = ", ".join(f"{k}:{len(v)}" for k, v in sorted(classes.items()))
        verdict = "WILL bail" if any(k in WILL_BAIL for k in classes) else "may bail (timing)"
        print(f"  {site}")
        print(f"    {kinds}   -> {verdict}")
        sample = next(iter(next(iter(classes.values()))))
        print(f"    e.g. [{param_of(sample)[:90]}]")
        kind = next(k for k in classes if k in WILL_BAIL) if any(
            k in WILL_BAIL for k in classes) else next(iter(classes))
        fix = {
            "address": "give this parametrize stable ids= (its default id falls back to repr(), hence the address)",
            "uuid": "give this parametrize stable ids= (don't derive the id from a uuid)",
            "time": "freeze the clock for this parametrize source, or pass explicit ids=",
            "other": "pin this parametrize's ids= to stable labels",
        }[kind]
        print(f"    FIX (upstream): {fix}")
        if verdict == "WILL bail":
            print(f"    STOPGAP (rstest): -n 0\n")
        else:
            print(f"    STOPGAP (rstest): usually runs at -n auto (the id is "
                  f"stable enough within a run); -n 0 only if it bails\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
