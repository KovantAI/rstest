#!/usr/bin/env python3
"""Measure `rstest --watch` rerun latency on a one-test project.

Writes a throwaway project (one test file, one trivial test), starts
`rstest --watch -n N`, and after the first run edits the test file `--cycles`
times, timing three spans per edit from rstest's own output:

  detect  — save until rstest prints `[watch] ... changed; rerunning`
            (file-event delivery plus the save-burst debounce);
  run     — that line until the rerun's `passed` summary line (worker
            spawn, collection, the test itself, reporting);
  total   — save to result, the sum of the two.

Output lines are timestamped as they arrive on a pipe, so the spans include
pipe delivery (well under a millisecond). Reports the median and min-max per
worker count and writes them as JSON.

    python3 corpus/watch_cycle.py --out corpus/bench-results/<date>-watch-cycle.json

`--python` picks the worker interpreter (needs pytest and the rstest worker
package: the repo's `uv sync` venv by default).
"""

import argparse
import datetime
import json
import platform
import queue
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
READY = "[watch] waiting for changes"
CHANGED = "changed; rerunning"
TEST = "def test_one():\n    assert {} == {}\n"


def _reader(stream, q):
    for line in stream:
        q.put((time.monotonic(), line.rstrip("\n")))
    q.put((time.monotonic(), None))


def _wait_for(q, needle, timeout=60):
    deadline = time.monotonic() + timeout
    while True:
        t, line = q.get(timeout=max(0.1, deadline - time.monotonic()))
        if line is None:
            raise RuntimeError(f"rstest exited while waiting for {needle!r}")
        if needle in line:
            return t


def measure(rstest, python, workers, cycles, settle):
    with tempfile.TemporaryDirectory() as tmp:
        proj = Path(tmp)
        test = proj / "test_one.py"
        test.write_text(TEST.format(0, 0))
        proc = subprocess.Popen(
            [rstest, "--watch", "-n", str(workers), "--python", python],
            cwd=proj,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
        q = queue.Queue()
        threading.Thread(target=_reader, args=(proc.stdout, q), daemon=True).start()
        rows = []
        try:
            _wait_for(q, READY)
            for i in range(1, cycles + 1):
                time.sleep(settle)  # let the watcher go idle between saves
                t0 = time.monotonic()
                test.write_text(TEST.format(i, i))
                t_detect = _wait_for(q, CHANGED)
                t_result = _wait_for(q, "passed")
                _wait_for(q, READY)
                rows.append(
                    {
                        "detect_ms": round((t_detect - t0) * 1000, 1),
                        "run_ms": round((t_result - t_detect) * 1000, 1),
                        "total_ms": round((t_result - t0) * 1000, 1),
                    }
                )
        finally:
            proc.kill()
            proc.wait()
    out = {"workers": workers, "cycles": rows}
    for key in ("detect_ms", "run_ms", "total_ms"):
        vals = [r[key] for r in rows]
        out[key] = {
            "median": round(statistics.median(vals), 1),
            "min": min(vals),
            "max": max(vals),
        }
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    ap.add_argument("--rstest", default=str(REPO / "target" / "release" / "rstest"))
    ap.add_argument("--python", default=str(REPO / ".venv" / "bin" / "python"))
    ap.add_argument("--workers", default="0,2", help="comma-separated -n values")
    ap.add_argument("--cycles", type=int, default=10, help="timed edits per worker count")
    ap.add_argument("--settle", type=float, default=1.0, help="idle seconds before each edit")
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    def _out(cmd):
        return subprocess.run(cmd, capture_output=True, text=True).stdout.strip()

    doc = {
        "environment": {
            "date": datetime.date.today().isoformat(),
            "platform": platform.platform(),
            "rstest": _out([args.rstest, "--version"]),
            "rstest_commit": _out(["git", "-C", str(REPO), "rev-parse", "--short", "HEAD"]),
            "worker_python": _out([args.python, "-c", "import sys; print(sys.version.split()[0])"]),
            "pytest": _out([args.python, "-c", "import pytest; print(pytest.__version__)"]),
            "cycles": args.cycles,
            "settle_s": args.settle,
        },
        "points": [],
    }
    for n in [int(x) for x in args.workers.split(",") if x]:
        p = measure(args.rstest, args.python, n, args.cycles, args.settle)
        doc["points"].append(p)
        print(
            f"-n {n}: total {p['total_ms']['median']} ms "
            f"(detect {p['detect_ms']['median']}, run {p['run_ms']['median']})"
        )
    Path(args.out).write_text(json.dumps(doc, indent=1))
    return 0


if __name__ == "__main__":
    sys.exit(main())
