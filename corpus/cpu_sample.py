#!/usr/bin/env python3
"""Sample the CPU use of a test run's coordinating process and its workers.

Evidence for the "controller-bound" reading in docs/reference/benchmarks.md:
runs one command and, every `--interval` seconds, reads the CPU time of the
command's own process (pytest-xdist's controller, or the rstest driver) and
of every descendant (the workers). Each sample's CPU% is the CPU-time delta
over the wall delta, so 100% is one core fully busy.

    uv run --no-project --with psutil python corpus/cpu_sample.py \\
        --out corpus/bench-results/<date>-<suite>-cpu.json --label xdist-n8 \\
        -- <venv>/bin/python -m pytest -q -n 8 ...

Run it from the directory and with the environment the command needs (the
bench's suite env: VIRTUAL_ENV, PATH, PYTHONHASHSEED=0). Several runs append
to the same --out file. Needs psutil.
"""

import argparse
import contextlib
import json
import platform
import statistics
import subprocess
import sys
import time
from pathlib import Path
from typing import Any

import psutil


def _cpu(p):
    t = p.cpu_times()
    return t.user + t.system


def sample(cmd, interval):
    proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    root = psutil.Process(proc.pid)
    last = {}  # pid -> cpu seconds at the previous sample
    t_prev = t0 = time.monotonic()
    rows = []
    while proc.poll() is None:
        time.sleep(interval)
        now = time.monotonic()
        dt = now - t_prev
        t_prev = now
        try:
            kids = root.children(recursive=True)
            root_cpu = _cpu(root)
        except psutil.Error:
            break
        root_pct = (root_cpu - last[root.pid]) / dt * 100 if root.pid in last else None
        last[root.pid] = root_cpu
        kid_pcts = []
        for k in kids:
            with contextlib.suppress(psutil.Error):  # exited between listing and reading
                c = _cpu(k)
                if k.pid in last:
                    kid_pcts.append((c - last[k.pid]) / dt * 100)
                last[k.pid] = c
        if root_pct is not None:
            rows.append(
                {
                    "t": round(now - t0, 2),
                    "root_pct": round(root_pct, 1),
                    "workers": len(kid_pcts),
                    "workers_mean_pct": round(statistics.fmean(kid_pcts), 1) if kid_pcts else None,
                }
            )
    rc = proc.wait()
    return rc, time.monotonic() - t0, rows


def summarize(rows, wall):
    """Per-sample medians, plus CPU-seconds integrated over the samples:
    `workers_busy_share` is worker CPU-seconds over (worker count x wall), the
    share of the run the average worker spent on a core."""
    root = [r["root_pct"] for r in rows]
    busy = [r["workers_mean_pct"] for r in rows if r["workers_mean_pct"] is not None]
    if not root:
        return {}
    root_s = workers_s = 0.0
    prev = rows[0]["t"]  # each sample covers the span since the previous one
    for r in rows[1:]:
        dt, prev = r["t"] - prev, r["t"]
        root_s += r["root_pct"] / 100 * dt
        workers_s += (r["workers_mean_pct"] or 0) / 100 * r["workers"] * dt
    n = statistics.median(r["workers"] for r in rows)
    return {
        "samples": len(root),
        "root_pct_median": round(statistics.median(root), 1),
        "root_share_at_or_above_90pct": round(sum(x >= 90 for x in root) / len(root), 3),
        "workers_mean_pct_median": round(statistics.median(busy), 1) if busy else None,
        "root_cpu_s": round(root_s, 1),
        "workers_cpu_s": round(workers_s, 1),
        "workers": n,
        "workers_busy_share": round(workers_s / (n * wall), 2) if n and wall else None,
    }


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    ap.add_argument("--out", required=True)
    ap.add_argument("--label", required=True)
    ap.add_argument("--interval", type=float, default=0.5)
    ap.add_argument("cmd", nargs=argparse.REMAINDER)
    args = ap.parse_args()
    cmd = args.cmd[1:] if args.cmd[:1] == ["--"] else args.cmd
    rc, wall, rows = sample(cmd, args.interval)
    out = Path(args.out)
    doc: dict[str, Any] = json.loads(out.read_text()) if out.exists() else {"runs": []}
    doc.setdefault("platform", platform.platform())
    doc.setdefault("interval_s", args.interval)
    run = {"label": args.label, "cmd": cmd, "rc": rc, "wall": round(wall, 1)}
    run.update(summary=summarize(rows, wall), series=rows)
    doc["runs"].append(run)
    out.write_text(json.dumps(doc, indent=1))
    print(json.dumps({k: v for k, v in run.items() if k != "series"}, indent=1))
    return 0


if __name__ == "__main__":
    sys.exit(main())
