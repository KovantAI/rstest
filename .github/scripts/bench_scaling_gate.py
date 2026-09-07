#!/usr/bin/env python3
"""Algorithmic-scaling gate for the wire-protocol decode benchmark.

Absolute decode times are hardware- and load-dependent, so gating CI on
nanoseconds flakes on shared runners (see example-bench.yml: wall-time numbers
are advisory, never a gate). What IS machine-independent is the *shape* of the
scaling curve: decoding N collected ids is O(N), so the per-element cost must be
roughly constant as N grows. An accidental O(N^2) (e.g. a quadratic lookup
sneaking into a decode path) leaves per-element cost climbing with N - and that
ratio is stable across hardware.

This parses criterion's `--output-format bencher` lines for the
`decode_event/collection_done/{N}` benchmarks and fails if the per-element
decode time grows more than --max-per-elem-ratio between the two largest sizes
(where fixed overhead is negligible, so the ratio isolates the growth term).
Linear decode lands near 1.0; a quadratic regression lands near 10.

Writes a table to $GITHUB_STEP_SUMMARY and exposes the ratio via $GITHUB_OUTPUT.
"""

from __future__ import annotations

import argparse
import os
import re
import sys

# `test decode_event/collection_done/10000 ... bench:      287960 ns/iter (+/- 3021)`
_BENCH = re.compile(
    r"test\s+decode_event/collection_done/(\d+)\s+\.\.\.\s+bench:\s+([\d,]+)\s+ns/iter"
)


def _parse(text: str) -> dict[int, float]:
    """size -> ns/iter for every collection_done/<size> bench line."""
    out: dict[int, float] = {}
    for m in _BENCH.finditer(text):
        size = int(m.group(1))
        ns = float(m.group(2).replace(",", ""))
        out[size] = ns
    return out


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument(
        "--bench",
        required=True,
        help="file with criterion --output-format bencher output",
    )
    ap.add_argument(
        "--max-per-elem-ratio",
        type=float,
        default=3.0,
        help="fail if per-element cost grows by more than this across the two "
        "largest sizes (linear ~1.0, quadratic ~10)",
    )
    args = ap.parse_args()

    if not os.path.isfile(args.bench):
        print(f"::error::scaling gate: bench file '{args.bench}' not found", file=sys.stderr)
        return 1

    with open(args.bench, encoding="utf-8") as fh:
        sizes = _parse(fh.read())

    # Need at least two sizes to measure a slope.
    if len(sizes) < 2:
        print(
            "::error::scaling gate: fewer than 2 collection_done sizes parsed "
            f"(got {sorted(sizes)}); did the bench run with --output-format bencher?",
            file=sys.stderr,
        )
        return 1

    big, small = sorted(sizes)[-1], sorted(sizes)[-2]
    per_big = sizes[big] / big
    per_small = sizes[small] / small
    ratio = per_big / per_small if per_small else float("inf")
    threshold = args.max_per_elem_ratio
    ok = ratio <= threshold
    verdict = "PASS (scaling is linear)" if ok else "FAIL (super-linear decode)"

    rows = "\n".join(f"| {n} | {sizes[n]:.0f} | {sizes[n] / n:.3f} |" for n in sorted(sizes))
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as fh:
            fh.write("### decode scaling gate\n\n")
            fh.write("| ids (N) | ns/iter | ns per element |\n|--:|--:|--:|\n")
            fh.write(rows + "\n\n")
            fh.write(
                f"- Per-element ratio (N={big} vs N={small}): "
                f"**{ratio:.2f}**  |  Threshold: **{threshold:.2f}**\n"
            )
            fh.write(f"- **Result: {verdict}**\n")

    out = os.environ.get("GITHUB_OUTPUT")
    if out:
        with open(out, "a", encoding="utf-8") as fh:
            fh.write(f"per_elem_ratio={ratio:.4f}\n")

    print(
        f"scaling gate: N={big} per-elem={per_big:.3f}ns  "
        f"N={small} per-elem={per_small:.3f}ns  "
        f"ratio={ratio:.2f} threshold={threshold:.2f} -> {verdict}"
    )
    if not ok:
        print(
            f"::error::decode per-element cost grew {ratio:.2f}x between N={small} "
            f"and N={big} (threshold {threshold:.2f}); decode may have gone "
            "super-linear.",
            file=sys.stderr,
        )
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
