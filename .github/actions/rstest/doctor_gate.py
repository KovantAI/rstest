#!/usr/bin/env python3
"""Threshold gate over rstest's --doctor-json report.

rstest computes the diagnostics (parallel efficiency, wait-bound %, worker
imbalance, …) but has no built-in gate (--doctor-fail-on is a tracked core
TODO). This wraps the existing JSON so CI can fail on a metric crossing a
threshold, e.g.:

    --fail-on 'parallel_efficiency<30, wait_pct>50, imbalance_pct>60'

Each condition is a FAIL predicate: if it holds, the gate fails. A metric that
is absent from the report (a single-worker run has no parallel_efficiency) is
skipped with a note, never a failure.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys

OPS = {
    "<=": lambda a, b: a <= b,
    ">=": lambda a, b: a >= b,
    "==": lambda a, b: a == b,
    "<": lambda a, b: a < b,
    ">": lambda a, b: a > b,
}

# metric alias -> path into the doctor JSON
METRICS = {
    "parallel_efficiency": ("parallel_efficiency", "efficiency_pct"),
    "efficiency_pct": ("parallel_efficiency", "efficiency_pct"),
    "realized_speedup": ("parallel_efficiency", "realized_speedup"),
    "imbalance_pct": ("parallel_efficiency", "imbalance_pct"),
    "imbalance": ("parallel_efficiency", "imbalance_pct"),
    "long_pole_seconds": ("parallel_efficiency", "long_pole_seconds"),
    "wait_pct": ("wait_bound", "wait_pct"),
    "wait_bound": ("wait_bound", "wait_pct"),
    "wall_seconds": ("wall_seconds",),
    "wall": ("wall_seconds",),
    "tests": ("tests",),
}

# longest ops first so '<=' is matched before '<'
_COND = re.compile(r"^\s*([A-Za-z_]+)\s*(<=|>=|==|<|>)\s*([-+0-9.]+)\s*$")


def _lookup(report: dict, path: tuple):
    node = report
    for key in path:
        if not isinstance(node, dict) or key not in node or node[key] is None:
            return None
        node = node[key]
    return node


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--report", required=True)
    ap.add_argument("--fail-on", required=True)
    args = ap.parse_args()

    try:
        with open(args.report, encoding="utf-8") as fh:
            report = json.load(fh)
    except (OSError, json.JSONDecodeError) as e:
        print(f"::error::doctor gate: cannot read '{args.report}': {e}", file=sys.stderr)
        return 1

    rows: list[str] = []
    failures: list[str] = []
    for raw in args.fail_on.split(","):
        raw = raw.strip()
        if not raw:
            continue
        m = _COND.match(raw)
        if not m:
            print(f"::error::doctor gate: cannot parse condition '{raw}' "
                  "(expected e.g. 'parallel_efficiency<30')", file=sys.stderr)
            return 1
        name, op, threshold_s = m.group(1), m.group(2), m.group(3)
        if name not in METRICS:
            print(f"::error::doctor gate: unknown metric '{name}'. "
                  f"Known: {', '.join(sorted(METRICS))}", file=sys.stderr)
            return 1
        threshold = float(threshold_s)
        value = _lookup(report, METRICS[name])
        if value is None:
            rows.append(f"- `{raw}`: metric absent — skipped")
            continue
        breached = OPS[op](value, threshold)
        status = "FAIL" if breached else "ok"
        rows.append(f"- `{raw}`: {name}={value:g} → **{status}**")
        if breached:
            failures.append(f"{name}={value:g} {op} {threshold:g}")

    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as fh:
            fh.write("### rstest doctor-metric gate\n\n")
            fh.write("\n".join(rows) + "\n")
            fh.write(f"\n**Result: {'FAIL' if failures else 'PASS'}**\n")

    print("doctor gate:")
    for r in rows:
        print("  " + r.lstrip("- "))
    if failures:
        print("::error::doctor-metric gate failed: " + "; ".join(failures))
        return 1
    print("doctor gate: PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
