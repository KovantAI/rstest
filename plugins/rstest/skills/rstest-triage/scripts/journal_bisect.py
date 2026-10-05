#!/usr/bin/env python3
"""Find a polluter inside one worker's recorded order from an rstest replay journal.

`rstest bisect` searches the victim's predecessors in *collection* order. On CI
the polluter often ran before the victim only because the scheduler put it
there, while it collects *after* the victim, so bisect reports "not
order-dependent". This script uses the order the journal recorded instead.

Usage:
    journal_bisect.py JOURNAL NODEID [--rstest PATH] [--list] [-- PYTEST_ARGS...]

    --rstest PATH  rstest binary (default: .venv/venv/$VIRTUAL_ENV, then PATH)

    --list   only print the worker's predecessors and the reproduce command
             (no test runs)

Without --list it runs `rstest -n 0` on shrinking prefixes of the worker's
predecessor list (keeping their recorded order) and prints the minimal set
that still makes the victim fail. Assumes the victim passes on its own; check
that first with `rstest -n 0 NODEID`.
"""

import json
import os
import re
import shlex
import shutil
import subprocess
import sys


def find_rstest():
    """The project's venv first (where `uv pip install rstest` puts it), then PATH."""
    for venv in (os.environ.get("VIRTUAL_ENV"), ".venv", "venv"):
        if not venv:
            continue
        for rel in ("bin/rstest", "Scripts/rstest.exe"):
            cand = os.path.join(venv, rel)
            if os.path.isfile(cand) and os.access(cand, os.X_OK):
                return cand
    found = shutil.which("rstest")
    if found:
        return found
    sys.exit("error: rstest not found in .venv, venv, $VIRTUAL_ENV or PATH; pass --rstest PATH")


def load_predecessors(journal_path, nodeid):
    with open(journal_path) as f:
        journal = json.load(f)
    for worker, tests in enumerate(journal.get("assignment", [])):
        if nodeid in tests:
            return worker, tests[: tests.index(nodeid)]
    sys.exit(f"error: {nodeid} is not in any worker's list in {journal_path}")


def command(rstest, tests, nodeid, extra):
    return [rstest, "-n", "0", "-q", "-p", "no:randomly", *extra, *tests, nodeid]


def victim_fails(rstest, tests, nodeid, extra):
    proc = subprocess.run(command(rstest, tests, nodeid, extra), capture_output=True, text=True)
    if proc.returncode not in (0, 1):
        sys.exit(
            f"error: rstest exited {proc.returncode}\n{proc.stdout[-2000:]}{proc.stderr[-2000:]}"
        )
    # Only the victim's own outcome counts; a predecessor failing is not a repro.
    # A setup/teardown failure reports as ERROR, and a nodeid may contain spaces
    # (`test_x[a b]`), so match the whole id up to the " - reason" separator.
    victim = re.compile(rf"^(?:FAILED|ERROR) {re.escape(nodeid)}(?: - |$)", re.MULTILINE)
    return victim.search(proc.stdout) is not None


def ddmin(rstest, tests, nodeid, extra):
    """Classic ddmin over an ordered list; subsets keep the recorded order."""
    n = 2
    runs = 0
    while len(tests) >= 2 and runs < 80:
        size = max(1, len(tests) // n)
        chunks = [tests[i : i + size] for i in range(0, len(tests), size)]
        reduced = False
        for i, chunk in enumerate(chunks):
            runs += 1
            if victim_fails(rstest, chunk, nodeid, extra):
                tests, n, reduced = chunk, 2, True
                break
            complement = [t for j, c in enumerate(chunks) if j != i for t in c]
            runs += 1
            if victim_fails(rstest, complement, nodeid, extra):
                tests, n, reduced = complement, max(n - 1, 2), True
                break
        if not reduced:
            if n >= len(tests):
                break
            n = min(len(tests), n * 2)
    return tests, runs


def main():
    argv = sys.argv[1:]
    extra = []
    if "--" in argv:
        i = argv.index("--")
        argv, extra = argv[:i], argv[i + 1 :]
    rstest = None
    if "--rstest" in argv:
        i = argv.index("--rstest")
        rstest = argv[i + 1]
        del argv[i : i + 2]
    list_only = "--list" in argv
    argv = [a for a in argv if a != "--list"]
    if len(argv) != 2:
        sys.exit(__doc__)
    journal_path, nodeid = argv
    rstest = rstest or find_rstest()

    worker, preds = load_predecessors(journal_path, nodeid)
    print(f"worker gw{worker}: {len(preds)} test(s) ran before {nodeid}")
    full = command(rstest, preds, nodeid, extra)
    if list_only:
        print(shlex.join(full))
        return 0
    if not victim_fails(rstest, preds, nodeid, extra):
        print(
            "the recorded worker order does NOT reproduce the failure at -n 0: "
            "likely concurrency or load between workers, not state pollution"
        )
        return 1
    culprits, runs = ddmin(rstest, preds, nodeid, extra)
    print(f"minimal polluter set ({runs} runs):")
    for t in culprits:
        print(f"  {t}")
    print("reproduce:")
    print("  " + shlex.join(command(rstest, culprits, nodeid, extra)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
