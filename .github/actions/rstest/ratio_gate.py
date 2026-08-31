#!/usr/bin/env python3
"""Fail-ratio gate for nondeterministic (real-LLM) suites.

Parses rstest's JUnit XML and decides the job outcome:

  * any failure whose text matches --hard-fail-on  -> fail immediately
    (a real bug, never tolerated regardless of ratio);
  * otherwise, fail only if the assertion-failure fraction of executed
    tests exceeds --fail-under-ratio.

Replaces the per-repo JUnit-parsing gate scripts (e.g. agent-library's
run_acceptance.py). Writes a short table to $GITHUB_STEP_SUMMARY and exposes
passed/failed via $GITHUB_OUTPUT.
"""

from __future__ import annotations

import argparse
import os
import re
import sys
import xml.etree.ElementTree as ET


def _parse(path: str) -> ET.Element:
    """Parse JUnit XML with entity/DTD attacks disabled.

    Prefer defusedxml when available; otherwise fall back to the stdlib parser
    with DOCTYPE declarations forbidden — that blocks XXE (external entities)
    and billion-laughs (internal entity definitions), both of which require a
    DTD. JUnit has no legitimate DTD.
    """
    try:
        from defusedxml.ElementTree import parse as _dparse  # type: ignore
        return _dparse(path).getroot()
    except ImportError:
        pass
    with open(path, "rb") as fh:
        data = fh.read()
    # Entities (XXE, billion-laughs) can only be declared inside a DTD, so
    # rejecting any DOCTYPE blocks both attack classes. JUnit has no DTD.
    if re.search(rb"<!DOCTYPE", data, re.IGNORECASE):
        raise ValueError("DOCTYPE/DTD is not allowed in JUnit XML")
    return ET.fromstring(data)


def _text(case: ET.Element) -> str:
    """Concatenate failure/error message + body for regex matching."""
    parts: list[str] = []
    for tag in ("failure", "error"):
        for el in case.findall(tag):
            parts.append(el.get("message", ""))
            parts.append(el.text or "")
    return "\n".join(parts)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--junit", required=True)
    ap.add_argument("--fail-under-ratio", required=True, type=float)
    ap.add_argument("--hard-fail-on", default="")
    ap.add_argument("--rstest-exit", default="0")
    args = ap.parse_args()

    if not os.path.isfile(args.junit):
        print(f"::error::fail-ratio gate: JUnit file '{args.junit}' not found. "
              "Set the `junit` input (empty disables the report).", file=sys.stderr)
        return 1

    try:
        root = _parse(args.junit)
    except (ET.ParseError, ValueError) as e:
        print(f"::error::fail-ratio gate: cannot parse '{args.junit}': {e}", file=sys.stderr)
        return 1

    suites = [root] if root.tag == "testsuite" else root.findall(".//testsuite")
    cases = [c for s in suites for c in s.findall("testcase")]

    total = len(cases)
    skipped = sum(1 for c in cases if c.find("skipped") is not None)
    failed_cases = [c for c in cases if c.find("failure") is not None or c.find("error") is not None]
    failed = len(failed_cases)
    executed = total - skipped
    passed = executed - failed

    hard = re.compile(args.hard_fail_on) if args.hard_fail_on else None
    hard_hits = []
    if hard:
        for c in failed_cases:
            if hard.search(_text(c)):
                name = f"{c.get('classname', '')}::{c.get('name', '')}".strip(":")
                hard_hits.append(name)

    ratio = (failed / executed) if executed else 0.0
    threshold = args.fail_under_ratio

    # Decide outcome.
    if hard_hits:
        verdict, ok = "HARD FAIL (matched hard-fail-on)", False
    elif ratio > threshold:
        verdict, ok = "FAIL (over ratio)", False
    else:
        verdict, ok = "PASS", True

    # Job summary.
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as fh:
            fh.write("### rstest fail-ratio gate\n\n")
            fh.write(f"- Executed: **{executed}** (passed {passed}, failed {failed}, skipped {skipped})\n")
            fh.write(f"- Fail ratio: **{ratio:.3f}**  |  Threshold: **{threshold:.3f}**\n")
            if hard_hits:
                fh.write(f"- Hard-fail matches ({len(hard_hits)}): "
                         + ", ".join(f"`{h}`" for h in hard_hits[:10])
                         + ("…" if len(hard_hits) > 10 else "") + "\n")
            fh.write(f"- **Result: {verdict}**\n")

    # Outputs.
    out = os.environ.get("GITHUB_OUTPUT")
    if out:
        with open(out, "a", encoding="utf-8") as fh:
            fh.write(f"passed={passed}\n")
            fh.write(f"failed={failed}\n")

    print(f"fail-ratio gate: executed={executed} failed={failed} "
          f"ratio={ratio:.3f} threshold={threshold:.3f} -> {verdict}")
    if hard_hits:
        print("hard-fail matches: " + ", ".join(hard_hits))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
