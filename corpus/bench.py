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
or any measured suite's parity is below --parity-floor. Two carve-outs, both
surfaced as INVESTIGATE (warning, job stays green) rather than gated:
  * total collapse — the pytest baseline collected NOTHING (collect errors AND
    zero tests, rc=2, e.g. upstream drift on a HEAD-cloned suite): no comparable
    parity at all;
  * provable superset — the baseline partially drifted (some collect errors) but
    rstest lost no baseline test, diverged on none they shared, and every extra
    test it collected is provably from a module the baseline itself dropped. The
    parity shortfall is the baseline's missing coverage, not an rstest fault.
A baseline that only partially failed but still collected tests, where rstest
lost/diverged on those OR collected unexplained extras, IS gated on them.
Everything else is advisory — never gate ordinary CI on wall time.

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


def _parity_row(base_snap, cand_snap):
    """One per-test diff -> the row fields the gate classifies on.

    A non-zero collect-error count means the runner hit collection failures
    (pytest rc=2 / rstest equivalent) — usually upstream drift on a HEAD-cloned
    suite (a new dep warning tripping `filterwarnings = error`), not a runner
    bug. Pairing the baseline's collect-error count with its collected-test
    count lets the gate tell a total collapse (broke, collected nothing ->
    investigate, neutral) apart from a partial failure that still collected
    tests. The missing/mismatch counts plus `parity_unexplained` (extra tests
    from a module the baseline collected FINE — phantom over-collection) then let
    it tell an rstest that merely out-collected a drifting baseline (a PROVABLE
    superset: lost none, diverged on none, every extra from a dropped module ->
    investigate) from one that actually lost, diverged, or over-collected
    (-> regression).
    """
    d = diff(base_snap, cand_snap)
    return {
        "parity": d["score"],
        "parity_missing": d["missing_count"],
        "parity_mismatch": d["mismatch_count"],
        "parity_unexplained": d["extra_unexplained_count"],
        "baseline_collect_errors": d["baseline_collect_errors"],
        "baseline_tests": d["baseline_tests"],
        "candidate_collect_errors": d["candidate_collect_errors"],
    }


def bench_suite(suite, repeat):
    """Spectrum row: pytest baseline vs rstest under the suite's own policy."""
    py_walls, py_snap = _repeat_pytest(suite, repeat)
    rs_walls, rs_snap = _repeat_rstest(suite, repeat)  # None -> suite policy
    py_med, py_band = _spread(py_walls)
    rs_med, rs_band = _spread(rs_walls)
    health = _parity_row(py_snap, rs_snap)
    speedup = py_med / rs_med if rs_med else 0.0
    tag = (
        " [INVESTIGATE: baseline collection broken]"
        if (health["baseline_collect_errors"] and not health["baseline_tests"])
        else ""
    )
    log(
        f"  {suite.name}: {speedup:.2f}x "
        f"(pytest {py_med:.1f}s -> rstest {rs_med:.1f}s), parity {health['parity']}%{tag}"
    )
    return (
        {
            "suite": suite.name,
            "pytest_wall": round(py_med, 1),
            "pytest_band": py_band,
            "rstest_wall": round(rs_med, 1),
            "rstest_band": rs_band,
            "speedup": round(speedup, 2),
            **health,
        },
        py_walls,
        py_snap,
    )


def bench_sweep(suite, repeat, py_walls, py_snap):
    """Worker-scaling rows on one suite, reusing its spectrum pytest baseline."""
    py_med = statistics.median(py_walls)
    rows = []
    for n in SWEEP_WORKERS:
        rs_walls, rs_snap = _repeat_rstest(suite, repeat, workers=n)
        rs_med, rs_band = _spread(rs_walls)
        health = _parity_row(py_snap, rs_snap)  # shared baseline, re-diffed per point
        speedup = py_med / rs_med if rs_med else 0.0
        tag = (
            " [INVESTIGATE: baseline collection broken]"
            if (health["baseline_collect_errors"] and not health["baseline_tests"])
            else ""
        )
        log(
            f"  {suite.name} -n {n}: {speedup:.2f}x ({rs_med:.1f}s), "
            f"parity {health['parity']}%{tag}"
        )
        rows.append(
            {
                "workers": n,
                "rstest_wall": round(rs_med, 1),
                "rstest_band": rs_band,
                "speedup": round(speedup, 2),
                **health,
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


def _candidate_superset(r):
    """rstest lost NO test the baseline had, diverged on NONE they shared, and
    every EXTRA test it collected is PROVABLY from a module the baseline's own
    collect errors dropped (`parity_unexplained == 0`). So the parity shortfall
    is the baseline's missing coverage that rstest rescued — never phantom
    over-collection, which would leave unexplained extras and stay gated."""
    return (
        not r.get("parity_missing")
        and not r.get("parity_mismatch")
        and not r.get("parity_unexplained")
    )


def _row_status(r, parity_floor):
    """Classify a measured row: 'investigate' (reference collapsed, not gated),
    'regression' (usable baseline but rstest broke or diverged), or 'ok'."""
    if _baseline_unusable(r):
        return "investigate"
    # Partial baseline drift where rstest is a clean SUPERSET: the baseline hit
    # collect errors (dropping some modules) but rstest collected them fine, so
    # its "extra" tests drag parity under the floor while it lost/diverged on
    # nothing. That is the baseline's missing coverage, not an rstest fault —
    # investigate, don't fail rstest for out-collecting a drifting reference.
    if r.get("baseline_collect_errors") and r["parity"] < parity_floor and _candidate_superset(r):
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
        "🔍 investigate = parity is not comparable, so the row is not gated: either "
        "the pytest *baseline* collapsed at collection (collect errors and zero tests "
        "collected — upstream drift on a HEAD-cloned suite), or the baseline drifted "
        "but rstest out-collected it (a clean superset). Neither is an rstest "
        "regression. A baseline that only partially failed but still collected tests, "
        "with rstest losing or diverging on those, is gated normally.\n"
    )

    out.append("### Spectrum — speedup vs the parallelizable share\n")
    out.append("| suite | pytest (s) | rstest (s) | speedup | parity | status |")
    out.append("|---|---|---|---|---|---|")
    for r in spectrum:
        st = _STATUS_MARK[_row_status(r, parity_floor)]
        # A collapsed baseline ran fast because it collected nothing — its speedup
        # is noise, not a measurement. Blank it so the number isn't read as real.
        sp = "—" if _baseline_unusable(r) else f"{r['speedup']:.2f}x"
        out.append(
            f"| {r['suite']} | {r['pytest_band']} | {r['rstest_band']} | "
            f"{sp} | {r['parity']}% | {st} |"
        )

    if sweep:
        out.append(f"\n### Worker sweep — {sweep['suite']} (pytest {sweep['pytest_wall']:.1f}s)\n")
        out.append("| workers | rstest (s) | speedup | parity | status |")
        out.append("|---|---|---|---|---|")
        for p in sweep["points"]:
            st = _STATUS_MARK[_row_status(p, parity_floor)]
            sp = "—" if _baseline_unusable(p) else f"{p['speedup']:.2f}x"
            out.append(f"| -n {p['workers']} | {p['rstest_band']} | {sp} | {p['parity']}% | {st} |")
        if any(_baseline_unusable(p) for p in sweep["points"]):
            out.append("\nBest sweep speedup: — (baseline collapsed; wall times not comparable)")
        else:
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
            if row.get("baseline_tests"):
                msg = (
                    f"{suite}: pytest baseline drifted at collection ({n} collect errors) "
                    f"but rstest collected a superset — the parity shortfall is the "
                    f"baseline's missing coverage, not an rstest fault"
                )
            else:
                msg = (
                    f"{suite}: pytest baseline collapsed at collection "
                    f"({n} collect errors, 0 tests)"
                )
            investigate.setdefault(suite, msg)
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
