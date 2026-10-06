#!/usr/bin/env python3
"""Measure pytest baseline vs rstest cold vs rstest warm on this example suite.

Wall-clock only, from the caller's perspective (what CI actually pays). Prints a
Markdown table. Not a microbenchmark — it runs the real commands end to end.

Usage:
    python measure.py                      # uses `pytest` and `rstest` on PATH
    RSTEST=/path/to/rstest PYTEST="python -m pytest" \\
    RSTEST_PYTHON=/path/to/venv/python python measure.py -n 4 --repeat 2
"""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
TESTS = HERE / "tests"
CACHE = HERE / ".rstest_cache"


def wall(cmd: list[str], cwd: Path) -> float:
    t0 = time.perf_counter()
    proc = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True)
    dt = time.perf_counter() - t0
    if proc.returncode not in (0, 1):  # 1 = test failures; anything else is a harness error
        raise SystemExit(
            f"command failed ({proc.returncode}): {' '.join(cmd)}\n{proc.stderr[-2000:]}"
        )
    return dt


def best_of(cmd: list[str], cwd: Path, repeat: int, *, drop_cache_each: bool) -> float:
    times = []
    for _ in range(repeat):
        if drop_cache_each:
            shutil.rmtree(CACHE, ignore_errors=True)
        times.append(wall(cmd, cwd))
    return min(times)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("-n", "--numprocesses", default="4")
    ap.add_argument("--repeat", type=int, default=2)
    args = ap.parse_args()

    pytest_cmd = os.environ.get("PYTEST", "pytest").split()
    rstest = os.environ.get("RSTEST", "rstest")
    rstest_python = os.environ.get("RSTEST_PYTHON")
    rstest_base = [rstest, "-n", args.numprocesses, "-q"]
    if rstest_python:
        rstest_base += ["--python", rstest_python]

    # 1. pytest baseline (serial) — the number CI pays today.
    baseline = best_of([*pytest_cmd, "-q", str(TESTS)], HERE, args.repeat, drop_cache_each=False)

    # 2. rstest COLD — no duration cache: dropped before every run.
    cold = best_of(rstest_base, HERE, args.repeat, drop_cache_each=True)

    # 3. rstest WARM — cache from a prior run present (populate once, then measure).
    shutil.rmtree(CACHE, ignore_errors=True)
    wall(rstest_base, HERE)  # populate the duration cache
    warm = best_of(rstest_base, HERE, args.repeat, drop_cache_each=False)

    n = args.numprocesses
    print(f"\n### rstest example bench (-n {n}, best of {args.repeat})\n")
    print("| config | wall | vs pytest |")
    print("|---|---|---|")
    print(f"| pytest (serial) | {baseline:.1f}s | 1.0x |")
    print(f"| rstest cold (`-n {n}`, no cache) | {cold:.1f}s | {baseline / cold:.1f}x |")
    print(f"| rstest warm (`-n {n}`, cached durations) | {warm:.1f}s | {baseline / warm:.1f}x |")
    print()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
