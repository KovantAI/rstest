#!/usr/bin/env python3
"""Render the plugin-version table in docs/guides/plugin-stack.md from real
corpus data.

The corpus installs plugins unpinned, so the versions it runs against vendored
pytest move over time. corpus/run.py records each suite venv's plugins into
corpus/results.json (a `plugins` list per suite, see corpus/plugin_probe.py);
this script aggregates them into markdown rows to paste over the table.

    python3 corpus/plugin_versions.py                   # from corpus/results.json
    python3 corpus/plugin_versions.py --results r.json  # e.g. a CI artifact
    python3 corpus/plugin_versions.py --from-venvs      # scan corpus/work/*/venv
    python3 corpus/plugin_versions.py --plugins all     # every plugin seen

Only plugins some suite actually installed get a row; anything else in the
docs table (pytest-html, gated in e2e rather than the corpus) is kept by hand.
If the chosen results file has no `plugins` data (a results.json from a run that
predates plugin recording), the script says so and exits 1; use --from-venvs.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path
from typing import TypedDict

HERE = Path(__file__).resolve().parent
RESULTS = HERE / "results.json"
WORK = HERE / "work"

# The stack docs/guides/plugin-stack.md tracks, in its table order.
STACK = [
    "pytest-django",
    "pytest-asyncio",
    "hypothesis",
    "pytest-cov",
    "pytest-mock",
    "pytest-sugar",
    "freezegun",
]


class Entry(TypedDict):
    """One plugin's aggregate: suites per version, and every declared
    (pytest specifier, extra) pair seen."""

    versions: dict[str, list[str]]
    requires: set[tuple[str | None, str | None]]


def aggregate(rows) -> dict[str, Entry]:
    """{plugin: {"versions": {version: [suite]}, "requires": {(spec, extra)}}}
    over result rows that carry a `plugins` list."""
    agg: dict[str, Entry] = {}
    for row in rows:
        for p in row.get("plugins") or ():
            entry = agg.setdefault(p["name"], {"versions": {}, "requires": set()})
            entry["versions"].setdefault(p["version"], []).append(row["suite"])
            entry["requires"].add((p.get("requires_pytest"), p.get("requires_pytest_extra")))
    return agg


def _vkey(version):
    parts = []
    for piece in version.split("."):
        digits = "".join(ch for ch in piece if ch.isdigit())
        parts.append(int(digits) if digits else 0)
    return parts


def _declared(spec, extra):
    text = "any version" if spec == "any" else f"`{spec}`"
    return f"{text} (`[{extra}]` extra)" if extra else text


def render(agg: dict[str, Entry], names):
    """Markdown rows (`| plugin | verified with | declared range | exercised by |`)
    for each name in `names` the corpus recorded, in that order."""
    lines = []
    for name in names:
        entry = agg.get(name)
        if entry is None:
            continue
        versions = sorted(entry["versions"], key=_vkey)
        suites = sorted({s for v in versions for s in entry["versions"][v]})
        declared = (
            " / ".join(
                _declared(spec, extra) for spec, extra in sorted(entry["requires"], key=str) if spec
            )
            or "none (no pytest dependency)"
        )
        lines.append(f"| {name} | {' / '.join(versions)} | {declared} | {', '.join(suites)} |")
    return lines


def rows_from_venvs(work=WORK):
    """Result-shaped rows by probing every prepared corpus venv directly."""
    rows = []
    for py in sorted(work.glob("*/venv/bin/python")):
        suite = py.parent.parent.parent.name
        r = subprocess.run([str(py), str(HERE / "plugin_probe.py")], capture_output=True, text=True)
        if r.returncode != 0:
            print(f"skip {suite}: probe failed: {r.stderr[-200:]}", file=sys.stderr)
            continue
        rows.append({"suite": suite, "plugins": json.loads(r.stdout)})
    return rows


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    src = ap.add_mutually_exclusive_group()
    src.add_argument("--results", type=Path, default=RESULTS, help="a corpus results.json")
    src.add_argument("--from-venvs", action="store_true", help="probe corpus/work/*/venv")
    ap.add_argument(
        "--plugins",
        default=",".join(STACK),
        help="comma-separated plugin names, or 'all' (default: the plugin-stack table)",
    )
    args = ap.parse_args(argv)

    rows = rows_from_venvs() if args.from_venvs else json.loads(args.results.read_text())
    agg = aggregate(rows)
    if not agg:
        where = "corpus/work/*/venv" if args.from_venvs else str(args.results)
        hint = (
            "prepare the suite venvs first (corpus/run.py)"
            if args.from_venvs
            else "re-run with --from-venvs to probe corpus/work/*/venv directly, "
            "or pass --results with a results.json that records plugins"
        )
        print(f"no plugin data found in {where}; {hint}", file=sys.stderr)
        return 1
    names = sorted(agg) if args.plugins == "all" else args.plugins.split(",")
    missing = [n for n in names if n not in agg]
    print("| Plugin | Verified with | Declared pytest range | Exercised by |")
    print("|---|---|---|---|")
    for line in render(agg, names):
        print(line)
    if missing:
        print(f"not recorded by any suite: {', '.join(missing)}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
