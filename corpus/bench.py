#!/usr/bin/env python3
"""Speed-ramp bench: prove rstest's speedup over pytest on a minimal corpus.

Two views, both measured on the SAME prepared venvs the parity corpus uses
(corpus/run.py), so the timing story and the correctness story share one setup:

  spectrum — a handful of suites spanning ~1x (tiny / CPU-bound, no room to
             parallelize) up to ~4x (heavily I/O- or await-bound): shows the
             speedup ramps with the *parallelizable share* of a suite, not with
             its raw size. Each suite runs under its own recommended policy
             (the `rstest_args` a user would actually get).
  sweep    — one suite (default: anyio) at -n 1,2,4: shows wall time dropping
             and speedup ramping with worker count, plateauing once workers
             reach the runner's core count. This is the literal "ramp".

Wall time on shared CI runners is noisy, so every point is the MEDIAN of
--repeat runs and the min-max spread is reported alongside it — a median that
moved but whose spread bands still overlap the old one is noise, not signal.

Correctness is guarded too: each rstest run is diffed against its pytest
baseline; a parity drop below --parity-floor fails the job. A faster-but-wrong
runner is not a win, so the speed numbers are only ever reported for runs that
also matched pytest's per-test outcomes.

Assumes `corpus/run.py --prepare-only --only <suites>` already built the venvs.
This phase is fully offline. Emits a markdown report (stdout +, when set,
$GITHUB_STEP_SUMMARY) and corpus/bench.json.

Soft gate: exits non-zero if the sweep suite's BEST speedup is below --floor,
or any measured suite's parity is below --parity-floor. One carve-out: a suite
whose pytest baseline collected NOTHING (total collection collapse, rc=2 — e.g.
upstream drift on a HEAD-cloned suite) has no comparable parity, so it is
surfaced as INVESTIGATE (warning, job stays green) rather than gated. A baseline
that only partially failed collection but still collected tests IS gated on the
tests it did collect. Everything else is advisory — never gate ordinary CI on
wall time.

Reproduce:
    python3 corpus/run.py --prepare-only --only marshmallow,attrs,fastapi,anyio
    python3 corpus/bench.py --only marshmallow,attrs,fastapi,anyio --sweep anyio
"""

import argparse
import glob
import json
import os
import statistics
import sys
from pathlib import Path

import tomllib

# Executed as `python corpus/bench.py`, so this dir is on sys.path and the
# corpus machinery (Suite/prepare/diff) imports directly — no duplication.
from run import HERE, REPO, Suite, diff, log

BENCH = HERE / "bench.json"

# Worker counts for the sweep. Beyond the runner's core count the curve
# plateaus (oversubscription), which is itself an honest, expected result.
SWEEP_WORKERS = [1, 2, 4]


def _spread(walls):
    """Median wall plus its observed band, e.g. '26.5 (25.9-27.8)'."""
    med = statistics.median(walls)
    return med, f"{med:.1f} ({min(walls):.1f}-{max(walls):.1f})"


def _repeat_pytest(suite, repeat):
    walls, snap = [], None
    for _ in range(repeat):
        snap, wall = suite.run_pytest()
        walls.append(wall)
    return walls, snap


def _repeat_rstest(suite, repeat, workers=None):
    walls, snap = [], None
    for _ in range(repeat):
        snap, wall = suite.run_rstest(workers=workers)
        walls.append(wall)
    return walls, snap


def _parity(base_snap, cand_snap):
    """Reuse the corpus per-test diff; return the parity score (0-100)."""
    return diff(base_snap, cand_snap)["score"]


def _snap_health(snap):
    """(collect-error count, collected-test count) from a snapshot.

    A non-zero collect-error count means the runner hit collection failures
    (pytest rc=2 / rstest equivalent) — usually upstream drift on a HEAD-cloned
    suite (a new dep warning tripping `filterwarnings = error`), not a runner
    bug. The gate pairs it with the collected-test count so it can tell a total
    collapse (broke, collected nothing → investigate, neutral) apart from a
    partial failure that still collected tests (parity over those IS meaningful,
    so gate it) and from rstest diverging from a healthy baseline (fail).
    """
    data = json.loads(Path(snap).read_text())
    return len(data.get("collect_errors", [])), len(data.get("tests", {}))


def bench_suite(suite, repeat):
    """Spectrum row: pytest baseline vs rstest under the suite's own policy."""
    py_walls, py_snap = _repeat_pytest(suite, repeat)
    rs_walls, rs_snap = _repeat_rstest(suite, repeat)  # None -> suite policy
    py_med, py_band = _spread(py_walls)
    rs_med, rs_band = _spread(rs_walls)
    parity = _parity(py_snap, rs_snap)
    base_broken, base_tests = _snap_health(py_snap)
    cand_broken, _ = _snap_health(rs_snap)
    speedup = py_med / rs_med if rs_med else 0.0
    tag = " [INVESTIGATE: baseline collection broken]" if (base_broken and not base_tests) else ""
    log(
        f"  {suite.name}: {speedup:.2f}x "
        f"(pytest {py_med:.1f}s -> rstest {rs_med:.1f}s), parity {parity}%{tag}"
    )
    return (
        {
            "suite": suite.name,
            "pytest_wall": round(py_med, 1),
            "pytest_band": py_band,
            "rstest_wall": round(rs_med, 1),
            "rstest_band": rs_band,
            "speedup": round(speedup, 2),
            "parity": parity,
            "baseline_collect_errors": base_broken,
            "baseline_tests": base_tests,
            "candidate_collect_errors": cand_broken,
        },
        py_walls,
        py_snap,
    )


def bench_sweep(suite, repeat, py_walls, py_snap):
    """Worker-scaling rows on one suite, reusing its spectrum pytest baseline."""
    py_med = statistics.median(py_walls)
    # shared baseline: its health is measured once, applies to every point
    base_broken, base_tests = _snap_health(py_snap)
    rows = []
    for n in SWEEP_WORKERS:
        rs_walls, rs_snap = _repeat_rstest(suite, repeat, workers=n)
        rs_med, rs_band = _spread(rs_walls)
        parity = _parity(py_snap, rs_snap)
        cand_broken, _ = _snap_health(rs_snap)
        speedup = py_med / rs_med if rs_med else 0.0
        tag = (
            " [INVESTIGATE: baseline collection broken]" if (base_broken and not base_tests) else ""
        )
        log(f"  {suite.name} -n {n}: {speedup:.2f}x ({rs_med:.1f}s), parity {parity}%{tag}")
        rows.append(
            {
                "workers": n,
                "rstest_wall": round(rs_med, 1),
                "rstest_band": rs_band,
                "speedup": round(speedup, 2),
                "parity": parity,
                "baseline_collect_errors": base_broken,
                "baseline_tests": base_tests,
                "candidate_collect_errors": cand_broken,
            }
        )
    return {"suite": suite.name, "pytest_wall": round(py_med, 1), "points": rows}


def _baseline_unusable(r):
    """The baseline is 'unusable' (parity incomparable) only on TOTAL collection
    collapse: collect errors AND zero tests collected. A baseline with a few
    collect errors that still collected tests yields a meaningful parity over the
    intersection — gate it normally, or rstest regressions on the collectible
    portion go uncaught."""
    return bool(r.get("baseline_collect_errors")) and not r.get("baseline_tests")


def _row_status(r, parity_floor):
    """Classify a measured row: 'investigate' (reference collapsed, not gated),
    'regression' (usable baseline but rstest broke or diverged), or 'ok'."""
    if _baseline_unusable(r):
        return "investigate"
    # rstest breaking collection is a standalone fault only when pytest DIDN'T
    # (healthy baseline). If the same upstream drift broke collection under BOTH
    # runners, they miss the same modules and agree by absence — parity is the
    # arbiter, not the raw error count (else identical drift reads as a false
    # regression).
    rstest_broke_alone = r.get("candidate_collect_errors") and not r.get("baseline_collect_errors")
    if rstest_broke_alone or r["parity"] < parity_floor:
        return "regression"
    return "ok"


_STATUS_MARK = {"ok": "✅", "investigate": "🔍 investigate", "regression": "❌"}


def render(spectrum, sweep, floor, parity_floor):
    out = ["## rstest speed ramp (median of repeated runs)\n"]
    out.append("Wall time is advisory (noisy shared runners); parity is the hard gate.\n")
    out.append(
        "🔍 investigate = the pytest *baseline* itself collapsed at collection "
        "(collect errors and zero tests collected — upstream drift on a HEAD-cloned "
        "suite), so parity is not comparable — not an rstest regression, and not "
        "gated. A baseline that only partially failed but still collected tests is "
        "gated normally on the tests it collected.\n"
    )

    out.append("### Spectrum — speedup vs the parallelizable share\n")
    out.append("| suite | pytest (s) | rstest (s) | speedup | parity | status |")
    out.append("|---|---|---|---|---|---|")
    for r in spectrum:
        st = _STATUS_MARK[_row_status(r, parity_floor)]
        out.append(
            f"| {r['suite']} | {r['pytest_band']} | {r['rstest_band']} | "
            f"{r['speedup']:.2f}x | {r['parity']}% | {st} |"
        )

    if sweep:
        out.append(f"\n### Worker sweep — {sweep['suite']} (pytest {sweep['pytest_wall']:.1f}s)\n")
        out.append("| workers | rstest (s) | speedup | parity | status |")
        out.append("|---|---|---|---|---|")
        for p in sweep["points"]:
            st = _STATUS_MARK[_row_status(p, parity_floor)]
            out.append(
                f"| -n {p['workers']} | {p['rstest_band']} | {p['speedup']:.2f}x | "
                f"{p['parity']}% | {st} |"
            )
        best = max((p["speedup"] for p in sweep["points"]), default=0.0)
        out.append(f"\nBest sweep speedup: {best:.2f}x (floor {floor:.1f}x)")

    out.append(f"\nParity floor: {parity_floor:.1f}%")
    return "\n".join(out) + "\n"


def main():
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    # Default set spans every ramp regime AND every failure mode worth proving:
    #   python-dateutil  sub-1x (tiny suite, rstest's own startup dominates)
    #   httpx            ~1x, forced -n 0 (session fixture on a fixed port)
    #   fastapi          ~2.6x mid gain
    #   anyio            ~3.9x big gain (also the worker-sweep suite)
    #   langgraph        monorepo: N serial per-lib pytest vs one root rstest
    #                    run; ~1.45x, capped by slow wait/IO tests in checkpoint
    # All five are documented at 100% per-test parity, so the parity gate is
    # meaningful (a real regression drops well below --parity-floor).
    ap.add_argument(
        "--only",
        default="python-dateutil,httpx,fastapi,anyio,langgraph",
        help="comma-separated spectrum suites (must be prepared)",
    )
    ap.add_argument(
        "--sweep",
        default="anyio",
        help="suite to run the -n worker sweep on (must be in --only), or '' to skip",
    )
    ap.add_argument("--repeat", type=int, default=3, help="runs per point; median is reported")
    ap.add_argument(
        "--rstest", default=str(REPO / "target" / "release" / "rstest"), help="rstest driver binary"
    )
    ap.add_argument("--wheel", default=None, help="rstest wheel (unused offline; for Suite ctor)")
    ap.add_argument(
        "--floor",
        type=float,
        default=2.0,
        help="soft gate: fail if the sweep's best speedup is below this",
    )
    ap.add_argument(
        "--parity-floor",
        type=float,
        default=99.5,
        help="hard gate: fail if any measured suite's parity is below this",
    )
    args = ap.parse_args()

    wheel = args.wheel or (
        max(
            glob.glob(str(REPO / "target" / "wheels" / "rstest-*.whl")),
            key=os.path.getmtime,
            default="",
        )
    )
    cfg = tomllib.loads((HERE / "suites.toml").read_text())
    names = [n for n in args.only.split(",") if n]
    missing = [n for n in names if n not in cfg]
    if missing:
        sys.exit(f"unknown suite(s): {', '.join(missing)}")

    suites = {n: Suite(n, cfg[n], wheel, args.rstest) for n in names}

    spectrum, py_cache = [], {}
    for name, suite in suites.items():
        if not (suite.venv / "bin" / "python").exists():
            sys.exit(
                f"{name}: not prepared — run `corpus/run.py --prepare-only --only {name}` first"
            )
        log(f"spectrum: {name}")
        row, py_walls, py_snap = bench_suite(suite, args.repeat)
        spectrum.append(row)
        py_cache[name] = (py_walls, py_snap)

    sweep = None
    if args.sweep:
        if args.sweep not in suites:
            sys.exit(f"--sweep {args.sweep} must be one of --only ({args.only})")
        log(f"sweep: {args.sweep}")
        py_walls, py_snap = py_cache[args.sweep]
        sweep = bench_sweep(suites[args.sweep], args.repeat, py_walls, py_snap)

    report = render(spectrum, sweep, args.floor, args.parity_floor)
    print("\n" + report)
    BENCH.write_text(json.dumps({"spectrum": spectrum, "sweep": sweep}, indent=1))

    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a") as fh:
            fh.write(report)

    # ---------- gates ----------
    # A row is only gated when its pytest baseline is usable: a baseline that
    # collapsed at collection (upstream drift on a HEAD-cloned suite: collect
    # errors AND nothing collected) makes parity/speedup incomparable, so it is
    # surfaced as INVESTIGATE (a warning, job stays green) rather than counted as
    # an rstest regression. rstest is faulted only when a usable baseline is
    # beaten wrong: parity below the floor, or rstest alone failing to collect
    # while pytest could. `investigate` is keyed by suite: the worker sweep
    # reuses one baseline, so a collapsed sweep baseline is ONE finding, not one
    # per -n point.
    fails, investigate = [], {}

    def _classify(row, suite, label):
        status = _row_status(row, args.parity_floor)
        if status == "investigate":
            n = row["baseline_collect_errors"]
            investigate.setdefault(
                suite,
                f"{suite}: pytest baseline collapsed at collection ({n} collect errors, 0 tests)",
            )
        elif status == "regression":
            if row.get("candidate_collect_errors"):
                fails.append(
                    f"{label}: rstest broke collection "
                    f"({row['candidate_collect_errors']} collect errors) while pytest baseline "
                    f"was healthy"
                )
            else:
                fails.append(f"{label} parity {row['parity']}% < {args.parity_floor}%")

    for r in spectrum:
        _classify(r, r["suite"], r["suite"])
    if sweep:
        for p in sweep["points"]:
            _classify(p, sweep["suite"], f"{sweep['suite']} -n {p['workers']}")
        # Speedup floor only means something against a usable baseline. If the
        # sweep suite's baseline collapsed, its wall numbers are noise — skip the
        # floor gate (the investigate mark already flags it).
        if not any(_baseline_unusable(p) for p in sweep["points"]):
            best = max((p["speedup"] for p in sweep["points"]), default=0.0)
            if best < args.floor:
                fails.append(f"{sweep['suite']} best speedup {best:.2f}x < floor {args.floor:.1f}x")

    for msg in investigate.values():
        log(f"INVESTIGATE: {msg}")
        print(f"::warning title=corpus drift::{msg}")
    if fails:
        for f in fails:
            log(f"GATE FAIL: {f}")
            print(f"::error title=rstest regression::{f}")
        sys.exit(1)
    if investigate:
        log(f"all rstest gates passed ({len(investigate)} suite(s) to investigate — see warnings)")
    else:
        log("all gates passed")


if __name__ == "__main__":
    main()
