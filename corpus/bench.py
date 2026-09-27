#!/usr/bin/env python3
"""Speed-ramp bench: prove rstest's speedup over pytest on a minimal corpus.

Two views, both measured on the SAME prepared venvs the parity corpus uses
(corpus/run.py), so the timing story and the correctness story share one setup:

  spectrum — a handful of suites spanning ~1x (tiny / CPU-bound, no room to
             parallelize) up to ~4x (heavily I/O- or await-bound): shows the
             speedup ramps with the *parallelizable share* of a suite, not with
             its raw size. Each suite runs under its own recommended policy
             (the `rstest_args` a user would actually get).
  sweep    — one or more suites (default: anyio) at each -n in
             --sweep-workers (default 1,2,4): shows wall time dropping and
             speedup ramping with worker count, plateauing once workers reach
             the runner's core count. This is the literal "ramp". With
             --xdist, pytest-xdist runs at the same -n values too (the suite
             venv needs pytest-xdist installed), so the two are always compared
             at matched worker counts.

Two optional extra measurements for the CPU-bound benchmarks (Phase 3 of the
benchmark plan; both use corpus/procmem.py):

  --memory — per -n: the largest single process's peak RSS and the sampled peak
             of the whole process tree (needs psutil in the interpreter running
             bench.py), with a fit of total ~ fixed + N * per_worker.
  --grid   — one suite at -n x BLAS thread cap (OMP_NUM_THREADS and friends):
             wall time per cell, and any test whose outcome changes between
             cells.

Wall time on shared CI runners is noisy, so every point is the MEDIAN of
--repeat runs (after --warmup untimed runs) and the min-max spread is reported
alongside it — a median that
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
    python3 corpus/bench.py --only sympy,scikit-learn --sweep sympy,scikit-learn \
        --sweep-workers 1,2,4,8,10,14 --xdist --repeat 5
    python3 corpus/bench.py --only scikit-learn --sweep '' --memory scikit-learn \
        --grid scikit-learn --grid-workers 1,2,4,10,14 --grid-threads 1,2,4,unset
"""

import argparse
import datetime
import glob
import json
import os
import platform
import shutil
import statistics
import subprocess
import sys
import time

import tomllib

# Executed as `python corpus/bench.py`, so this dir is on sys.path and the
# corpus machinery (Suite/prepare/diff) imports directly — no duplication.
from procmem import linear_fit, run_measured, suspended
from run import HERE, REPO, Suite, diff, log

BENCH = HERE / "bench.json"

# Default worker counts for the sweep (--sweep-workers). Beyond the runner's
# core count the curve plateaus (oversubscription), which is itself an honest,
# expected result.
SWEEP_WORKERS = [1, 2, 4]
# Every BLAS thread-pool knob the grid sets together. VECLIB_MAXIMUM_THREADS:
# numpy wheels on macOS arm64 use Accelerate, which ignores the other three.
BLAS_VARS = ("OMP_NUM_THREADS", "OPENBLAS_NUM_THREADS", "MKL_NUM_THREADS", "VECLIB_MAXIMUM_THREADS")
MIB = 1024 * 1024


def _spread(walls):
    """Median wall plus its observed band, e.g. '26.5 (25.9-27.8)'."""
    med = statistics.median(walls)
    return med, f"{med:.1f} ({min(walls):.1f}-{max(walls):.1f})"


MAX_SUSPEND_RETRIES = 5


def _awake_run(run):
    """`run()`, re-run if the machine slept during it: the monotonic wall the
    runners report would silently leave out the suspended stretch."""
    for _ in range(MAX_SUSPEND_RETRIES):
        c0 = time.time()
        snap, wall = run()
        if not suspended(time.time() - c0, wall):
            return snap, wall
        log("  machine slept during the run: discarding it and re-running")
    sys.exit("machine keeps sleeping mid-run: keep it awake (macOS: caffeinate -ims)")


def _measured_awake(cmd, cwd, env, **kw):
    """procmem.run_measured, re-run while the machine slept during the run."""
    for _ in range(MAX_SUSPEND_RETRIES):
        r = run_measured(cmd, cwd, env, **kw)
        if not r["suspended"]:
            return r
        log("  machine slept during the run: discarding it and re-running")
    sys.exit("machine keeps sleeping mid-run: keep it awake (macOS: caffeinate -ims)")


def _repeat(run, repeat, warmup):
    """`warmup` untimed runs (fill OS file caches, __pycache__, and rstest's
    duration cache), then `repeat` timed ones. `run()` -> (snap, wall)."""
    for _ in range(warmup):
        run()
    walls, snap = [], None
    for _ in range(repeat):
        snap, wall = _awake_run(run)
        walls.append(wall)
    return walls, snap


def _repeat_pytest(suite, repeat, warmup=0):
    return _repeat(suite.run_pytest, repeat, warmup)


def _repeat_xdist(suite, repeat, workers, warmup=0):
    return _repeat(lambda: suite.run_pytest(xdist_workers=workers), repeat, warmup)


# --cold: drop rstest's duration cache before every rstest run, so each one
# schedules without timing history (an ephemeral CI runner with nothing
# persisted). Default is warm: the warm-up run leaves the cache for the rest.
COLD = False


def _repeat_rstest(suite, repeat, workers=None, warmup=0):
    def run():
        if COLD:
            shutil.rmtree(suite.cwd() / ".rstest_cache", ignore_errors=True)
        return suite.run_rstest(workers=workers)

    return _repeat(run, repeat, warmup)


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


def bench_suite(suite, repeat, warmup=0):
    """Spectrum row: pytest baseline vs rstest under the suite's own policy."""
    py_walls, py_snap = _repeat_pytest(suite, repeat, warmup)
    rs_walls, rs_snap = _repeat_rstest(suite, repeat, warmup=warmup)  # None -> suite policy
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


def _overlaps(a, b):
    """Two (min, max) bands overlap: the difference is inside the noise."""
    return a[0] <= b[1] and b[0] <= a[1]


def _sweep_point(suite, n, kind, repeat, warmup, py_med, py_snap):
    if kind == "xdist":
        walls, snap = _repeat_xdist(suite, repeat, n, warmup)
    else:
        walls, snap = _repeat_rstest(suite, repeat, workers=n, warmup=warmup)
    med, band = _spread(walls)
    health = _parity_row(py_snap, snap)  # shared baseline, re-diffed per point
    speedup = py_med / med if med else 0.0
    tag = (
        " [INVESTIGATE: baseline collection broken]"
        if (health["baseline_collect_errors"] and not health["baseline_tests"])
        else ""
    )
    log(
        f"  {suite.name} {kind} -n {n}: {speedup:.2f}x ({med:.1f}s), "
        f"parity {health['parity']}%{tag}"
    )
    return {
        "wall": round(med, 1),
        "band": band,
        "min": round(min(walls), 2),
        "max": round(max(walls), 2),
        "speedup": round(speedup, 2),
        "efficiency": round(speedup / n, 2) if n else None,
        **health,
    }


def bench_sweep(suite, repeat, py_walls, py_snap, workers=None, xdist=False, warmup=0):
    """Worker-scaling rows on one suite, reusing its spectrum pytest baseline.

    rstest fields stay at the top level of each point (the gate reads them);
    the matched-n xdist result, when measured, sits under "xdist"."""
    py_med = statistics.median(py_walls)
    if xdist and not _has_xdist(suite):
        log(f"  {suite.name}: pytest-xdist not installed in its venv, skipping the xdist series")
        xdist = False
    rows = []
    for n in workers or SWEEP_WORKERS:
        rs = _sweep_point(suite, n, "rstest", repeat, warmup, py_med, py_snap)
        row = {
            "workers": n,
            "rstest_wall": rs.pop("wall"),
            "rstest_band": rs.pop("band"),
            **rs,
        }
        if xdist:
            row["xdist"] = _sweep_point(suite, n, "xdist", repeat, warmup, py_med, py_snap)
        rows.append(row)
    return {
        "suite": suite.name,
        "pytest_wall": round(py_med, 1),
        "pytest_band": _spread(py_walls)[1],
        "points": rows,
    }


def bench_memory(suite, workers, repeat, sample, xdist=False):
    """Peak memory per -n (rstest, plus xdist when asked), with a linear fit of
    the whole-tree peak: total ~ fixed + N * per_worker. One untimed warm-up per
    point, then the median of `repeat` sampled runs."""
    out = {"suite": suite.name}
    xdist = xdist and _has_xdist(suite)
    for kind in ["rstest"] + (["xdist"] if xdist else []):
        rows = []
        for n in workers:
            snap = suite.dir / f"mem-{kind}.json"
            cmd = suite.rstest_argv(snap, n) if kind == "rstest" else _xdist_argv(suite, n)
            env = suite.env()
            env["PYTHONPATH"] = str(HERE)  # recorder, for the xdist baseline run
            env["RSTEST_RECORD"] = str(snap)
            log(f"  {suite.name}: memory {kind} -n {n}")
            run_measured(cmd, suite.cwd(), env)  # warm-up
            runs = [_measured_awake(cmd, suite.cwd(), env, sample=sample) for _ in range(repeat)]
            tree = [r["peak_tree_rss"] for r in runs if r["peak_tree_rss"] is not None]
            rows.append(
                {
                    "workers": n,
                    "max_proc_rss_mib": round(
                        statistics.median(r["max_proc_rss"] for r in runs) / MIB, 1
                    ),
                    "peak_tree_rss_mib": round(statistics.median(tree) / MIB, 1) if tree else None,
                    "wall": round(statistics.median(r["wall"] for r in runs), 1),
                }
            )
        pts = [(r["workers"], r["peak_tree_rss_mib"]) for r in rows if r["peak_tree_rss_mib"]]
        fit = None
        if len(pts) >= 2:
            a, b, r2 = linear_fit([x for x, _ in pts], [y for _, y in pts])
            fit = {"fixed_mib": round(a, 1), "per_worker_mib": round(b, 1), "r2": round(r2, 3)}
        out[kind] = {"points": rows, "fit": fit}
    return out


def _has_xdist(suite):
    py = suite.venv / "bin" / "python"
    return subprocess.run([str(py), "-c", "import xdist"], capture_output=True).returncode == 0


def _xdist_argv(suite, n):
    return [
        str(suite.venv / "bin" / "python"),
        "-m",
        "pytest",
        "-p",
        "recorder",
        "-q",
        "-n",
        str(n),
        *suite.target_args(),
    ]


def bench_grid(suite, workers, threads, repeat, warmup):
    """rstest at -n x BLAS thread cap. The cap overrides whatever the suite's
    own `env` sets; "unset" removes every BLAS variable (the library default,
    usually one thread per core). Reports wall per cell and the tests whose
    phase outcomes differ between any two cells."""
    cells, outcomes = [], {}
    for n in workers:
        for t in threads:
            env = suite.env()
            for var in BLAS_VARS:
                env.pop(var, None)
            if t != "unset":
                env.update(dict.fromkeys(BLAS_VARS, t))
            snap = suite.dir / f"grid-{n}-{t}.json"
            cmd = suite.rstest_argv(snap, n)
            log(f"  {suite.name}: grid -n {n} threads {t}")
            walls = []
            for i in range(warmup + repeat):
                snap.unlink(missing_ok=True)
                r = _measured_awake(cmd, suite.cwd(), env)
                if not snap.exists():
                    raise RuntimeError(
                        f"rstest produced no snapshot (rc={r['rc']}): {r['stderr'][-400:]}"
                    )
                if i >= warmup:
                    walls.append(r["wall"])
            doc = json.loads(snap.read_text())
            outcomes[(n, t)] = {
                nid: tuple(v.get(k) for k in ("setup", "call", "teardown"))
                for nid, v in doc["tests"].items()
            }
            med, band = _spread(walls)
            cells.append({"workers": n, "threads": t, "wall": round(med, 1), "band": band})
    all_ids = set().union(*(o.keys() for o in outcomes.values())) if outcomes else set()
    drift = sorted(nid for nid in all_ids if len({o.get(nid) for o in outcomes.values()}) > 1)
    return {"suite": suite.name, "threads": threads, "cells": cells, "outcome_drift": drift}


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


def _render_sweep(sweep, floor, parity_floor, gated):
    out = []
    xd = any("xdist" in p for p in sweep["points"])
    band = sweep.get("pytest_band", f"{sweep['pytest_wall']:.1f}")
    out.append(f"\n### Worker sweep — {sweep['suite']} (pytest serial {band}s)\n")
    head = "| workers | rstest (s) | speedup | efficiency |"
    sep = "|---|---|---|---|"
    if xd:
        head += " xdist (s) | speedup | efficiency | faster |"
        sep += "---|---|---|---|"
    out.append(head + " parity | status |")
    out.append(sep + "---|---|")
    for p in sweep["points"]:
        st = _STATUS_MARK[_row_status(p, parity_floor)]
        sp = "—" if _baseline_unusable(p) else f"{p['speedup']:.2f}x"
        eff = f"{p['efficiency']:.0%}" if p.get("efficiency") is not None else "—"
        line = f"| -n {p['workers']} | {p['rstest_band']} | {sp} | {eff} |"
        parity = p["parity"]
        if xd:
            x = p["xdist"]
            if _overlaps((p["min"], p["max"]), (x["min"], x["max"])):
                win = "parity"
            else:
                win = "rstest" if p["rstest_wall"] < x["wall"] else "xdist"
            line += f" {x['band']} | {x['speedup']:.2f}x | {x['efficiency']:.0%} | {win} |"
            parity = min(parity, x["parity"])
        out.append(line + f" {parity}% | {st} |")
    if any(_baseline_unusable(p) for p in sweep["points"]):
        out.append("\nBest sweep speedup: — (baseline collapsed; wall times not comparable)")
    else:
        best = max((p["speedup"] for p in sweep["points"]), default=0.0)
        note = f" (floor {floor:.1f}x)" if gated else " (not gated)"
        out.append(f"\nBest sweep speedup: {best:.2f}x{note}")
    return out


def _render_memory(mem):
    out = [f"\n### Memory — {mem['suite']}\n"]
    for kind in ("rstest", "xdist"):
        if kind not in mem:
            continue
        data = mem[kind]
        out.append(f"**{kind}**\n")
        out.append("| workers | largest process peak (MiB) | whole tree peak (MiB) | wall (s) |")
        out.append("|---|---|---|---|")
        for p in data["points"]:
            tree = p["peak_tree_rss_mib"]
            tree_s = f"{tree:.0f}" if tree is not None else "n/a (no psutil)"
            out.append(
                f"| -n {p['workers']} | {p['max_proc_rss_mib']:.0f} | {tree_s} | {p['wall']} |"
            )
        if data["fit"]:
            f = data["fit"]
            out.append(
                f"\nFit: total ≈ {f['fixed_mib']:.0f} MiB + N x {f['per_worker_mib']:.0f} MiB "
                f"(R² {f['r2']})"
            )
        out.append("")
    return out


def _render_grid(grid):
    threads = grid["threads"]
    cells = {(c["workers"], c["threads"]): c for c in grid["cells"]}
    workers = sorted({c["workers"] for c in grid["cells"]})
    out = [f"\n### Worker x BLAS-thread grid — {grid['suite']}\n"]
    out.append(
        "Wall seconds, median (min-max). Columns: the cap set in " + ", ".join(BLAS_VARS) + ".\n"
    )
    out.append("| workers | " + " | ".join(threads) + " |")
    out.append("|---|" + "---|" * len(threads))
    for n in workers:
        out.append(f"| -n {n} | " + " | ".join(cells[(n, t)]["band"] for t in threads) + " |")
    drift = grid["outcome_drift"]
    out.append(
        f"\nOutcome drift across cells: {len(drift)} test(s)"
        + (": " + ", ".join(drift[:10]) if drift else "")
    )
    return out


def render(spectrum, sweeps, floor, parity_floor, memory=(), grid=None):
    if isinstance(sweeps, dict):  # a single sweep, as older callers pass it
        sweeps = [sweeps]
    sweeps = [s for s in (sweeps or []) if s]
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

    for i, sweep in enumerate(sweeps):
        out += _render_sweep(sweep, floor, parity_floor, gated=i == 0)
    for mem in memory:
        out += _render_memory(mem)
    if grid:
        out += _render_grid(grid)

    out.append(f"\nParity floor: {parity_floor:.1f}%")
    return "\n".join(out) + "\n"


def _int_list(s):
    return [int(x) for x in s.split(",") if x]


def _environment(args):
    """What every published table records next to it (benchmark methodology)."""

    def _out(cmd):
        try:
            return subprocess.run(cmd, capture_output=True, text=True, timeout=60).stdout.strip()
        except (OSError, subprocess.TimeoutExpired):
            return "unknown"

    lock = HERE / "lock.json"
    return {
        "date": datetime.date.today().isoformat(),
        "platform": platform.platform(),
        "logical_cpus": os.cpu_count(),
        "load_1m_at_start": round(os.getloadavg()[0], 2) if hasattr(os, "getloadavg") else None,
        "rstest": _out([args.rstest, "--version"]),
        "rstest_commit": _out(["git", "-C", str(REPO), "rev-parse", "--short", "HEAD"]),
        "suite_commits": json.loads(lock.read_text()) if lock.exists() else {},
        "repeat": args.repeat,
        "warmup": args.warmup,
        "rstest_cache": "cold" if args.cold else "warm",
    }


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
        help="comma-separated suites to run the -n worker sweep on (must be in --only), "
        "or '' to skip. The --floor gate applies to the first one",
    )
    ap.add_argument(
        "--sweep-workers",
        type=_int_list,
        default=SWEEP_WORKERS,
        help="comma-separated -n values for the sweep (default 1,2,4)",
    )
    ap.add_argument(
        "--xdist",
        action="store_true",
        help="also run pytest-xdist at every sweep -n (the suite venv needs pytest-xdist)",
    )
    ap.add_argument("--repeat", type=int, default=5, help="runs per point; median is reported")
    ap.add_argument(
        "--warmup", type=int, default=1, help="untimed runs before each point's timed ones"
    )
    ap.add_argument(
        "--cold",
        action="store_true",
        help="drop .rstest_cache before every rstest run (default: warm, cache from the warm-up)",
    )
    ap.add_argument(
        "--memory",
        default="",
        help="comma-separated suites (in --only) to measure peak memory per -n on "
        "(uses --sweep-workers; needs psutil for the whole-tree number)",
    )
    ap.add_argument("--memory-repeat", type=int, default=3, help="sampled runs per memory point")
    ap.add_argument("--sample", type=float, default=0.1, help="memory sampling interval (s)")
    ap.add_argument("--grid", default="", help="suite (in --only) for the -n x BLAS-thread grid")
    ap.add_argument("--grid-workers", type=_int_list, default=[1, 2, 4], help="grid -n values")
    ap.add_argument(
        "--grid-threads", default="1,2,4,unset", help="grid BLAS thread caps ('unset' = default)"
    )
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
    global COLD
    COLD = args.cold

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
    env_info = _environment(args)  # before any run: the load it records is the start load

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

    def _in_only(flag, names):
        bad = [n for n in names if n not in suites]
        if bad:
            sys.exit(f"--{flag} {','.join(bad)} must be in --only ({args.only})")
        return names

    sweeps = []
    for name in _in_only("sweep", [n for n in args.sweep.split(",") if n]):
        log(f"sweep: {name}")
        py_walls, py_snap = py_cache[name]
        sweeps.append(
            bench_sweep(
                suites[name],
                args.repeat,
                py_walls,
                py_snap,
                workers=args.sweep_workers,
                xdist=args.xdist,
                warmup=args.warmup,
            )
        )

    memory = []
    for name in _in_only("memory", [n for n in args.memory.split(",") if n]):
        log(f"memory: {name}")
        memory.append(
            bench_memory(
                suites[name], args.sweep_workers, args.memory_repeat, args.sample, args.xdist
            )
        )

    grid = None
    if args.grid:
        _in_only("grid", [args.grid])
        log(f"grid: {args.grid}")
        threads = [t for t in args.grid_threads.split(",") if t]
        grid = bench_grid(suites[args.grid], args.grid_workers, threads, args.repeat, args.warmup)

    report = render(spectrum, sweeps, args.floor, args.parity_floor, memory, grid)
    print("\n" + report)
    BENCH.write_text(
        json.dumps(
            {
                "environment": env_info,
                "spectrum": spectrum,
                "sweeps": sweeps,
                "memory": memory,
                "grid": grid,
            },
            indent=1,
        )
    )

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
    for sweep in sweeps:
        for p in sweep["points"]:
            _classify(p, sweep["suite"], f"{sweep['suite']} -n {p['workers']}")
    if sweeps:
        sweep = sweeps[0]
        # Speedup floor only means something against a usable baseline. If the
        # sweep suite's baseline collapsed, its wall numbers are noise — skip the
        # floor gate (the investigate mark already flags it). Only the first
        # sweep suite is gated: the others are measurements, not regression
        # checks.
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
