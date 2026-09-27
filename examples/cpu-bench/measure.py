#!/usr/bin/env python3
"""Worker sweep, memory model and worker x thread grid on a CPU-bound suite.

Three modes, all using the methodology in docs/reference/benchmarks.md
(Methodology): one untimed warm-up, then --repeat timed runs per point,
reported as median with the min-max spread; rstest and pytest-xdist always
at the same -n; every timed run's per-test outcomes diffed against the
serial pytest baseline (a run that disagrees is flagged, never reported as a
clean speed number).

  sweep (default)  pytest serial baseline, then rstest and pytest-xdist at each
                   -n in --workers. Speedup vs serial and efficiency
                   (speedup / n). rstest is measured warm: the warm-up run
                   leaves a duration cache, like a CI run with a persisted
                   .rstest_cache.
  --memory         per -n: exact peak RSS of the largest single process, and
                   the sampled peak of the whole process tree (needs psutil).
                   Also runs a no-op suite to get the per-worker baseline, then
                   fits total ~ orchestrator + N * per_worker.
  --grid           -n x BLAS threads over the `blas` tests (needs numpy): wall
                   time per cell, and whether any test's outcome changed.

Usage (from this directory, with a venv that has pytest, pytest-xdist, the
rstest wheel, and for --memory / --grid psutil and numpy):

    python measure.py                                   # sweep 1,2,4,.. up to cpu count
    python measure.py --workers 1,2,4,8,10,14 --repeat 5
    python measure.py --memory --workers 1,2,4,8,14 -m blas
    python measure.py --grid --workers 1,2,4,10,14 --threads 1,2,4,unset

Env: RSTEST (driver binary, default `rstest` on PATH), PYTHON (interpreter for
pytest / xdist and for rstest's workers, default this one).
"""

from __future__ import annotations

import argparse
import datetime
import json
import os
import platform
import shutil
import statistics
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
CORPUS = REPO / "corpus"
sys.path.insert(0, str(CORPUS))

from procmem import linear_fit, run_measured  # noqa: E402
from run import diff  # noqa: E402  # the corpus parity diff, unchanged

CACHE = HERE / ".rstest_cache"
RESULTS = HERE / "results.json"
# VECLIB_MAXIMUM_THREADS: numpy wheels on macOS arm64 use Accelerate, which
# ignores the OpenMP / OpenBLAS / MKL variables.
BLAS_VARS = ("OMP_NUM_THREADS", "OPENBLAS_NUM_THREADS", "MKL_NUM_THREADS", "VECLIB_MAXIMUM_THREADS")
MIB = 1024 * 1024
MAX_SUSPEND_RETRIES = 5


# ---------------------------------------------------------------- commands --


class Runner:
    """Builds the three command lines. Each run writes a recorder-shaped
    outcome snapshot (rstest's --report-json, or corpus/recorder.py under
    pytest) so it can be diffed against the serial baseline."""

    def __init__(self, python, rstest, marker, target, cwd=HERE):
        self.python = python
        self.rstest = rstest
        self.marker = marker
        self.target = target  # test dir; HERE/tests unless --memory's no-op suite
        self.cwd = cwd

    def _select(self):
        return ["-m", self.marker] if self.marker else []

    def pytest(self, snap, workers=None):
        cmd = [self.python, "-m", "pytest", "-q", "-p", "recorder", *self._select()]
        if workers is not None:
            cmd += ["-n", str(workers)]
        return [*cmd, str(self.target)]

    def rstest_cmd(self, snap, workers):
        return [
            self.rstest,
            "-n",
            str(workers),
            "-q",
            "--python",
            self.python,
            "--report-json",
            str(snap),
            *self._select(),
            str(self.target),
        ]

    def cmd(self, kind, snap, workers):
        if kind == "pytest":
            return self.pytest(snap)
        if kind == "xdist":
            return self.pytest(snap, workers)
        return self.rstest_cmd(snap, workers)


def env_for(snap, extra=None):
    env = dict(os.environ)
    env.pop("PYTEST_ADDOPTS", None)
    env["PYTHONHASHSEED"] = "0"
    env["PYTHONPATH"] = str(CORPUS)  # corpus/recorder.py for the pytest runs
    env["RSTEST_RECORD"] = str(snap)
    if extra:
        env.update(extra)
    return env


def run_once(runner, kind, workers, snap, *, extra_env=None, sample=None):
    cmd = runner.cmd(kind, snap, workers)
    for _ in range(MAX_SUSPEND_RETRIES):
        snap.unlink(missing_ok=True)
        res = run_measured(cmd, runner.cwd, env_for(snap, extra_env), sample=sample, timeout=3600)
        if not res["suspended"]:
            break
        log(f"{kind} -n {workers}: machine slept during the run, discarding it and re-running")
    else:
        raise SystemExit("machine keeps sleeping mid-run: keep it awake (macOS: caffeinate -ims)")
    if res["rc"] not in (0, 1) or not snap.exists():  # 1 = test failures
        raise SystemExit(
            f"command failed (rc={res['rc']}): {' '.join(cmd)}\n"
            f"{res['stdout'][-1500:]}\n{res['stderr'][-1500:]}"
        )
    res["cmd"] = cmd
    return res


def repeat(runner, kind, workers, snap, args, *, extra_env=None):
    """One untimed warm-up, then args.repeat timed runs. Returns the walls and
    the snapshot of the last run."""
    run_once(runner, kind, workers, snap, extra_env=extra_env)  # warm-up
    walls = []
    for _ in range(args.repeat):
        res = run_once(runner, kind, workers, snap, extra_env=extra_env)
        walls.append(res["wall"])
    return walls, res["cmd"]


def band(walls):
    med = statistics.median(walls)
    return {"median": round(med, 2), "min": round(min(walls), 2), "max": round(max(walls), 2)}


def fmt(b):
    return f"{b['median']:.2f} ({b['min']:.2f}-{b['max']:.2f})"


def overlaps(a, b):
    return a["min"] <= b["max"] and b["min"] <= a["max"]


def parity(base_snap, snap):
    d = diff(base_snap, snap)
    return {
        "parity": d["score"],
        "tests": d["candidate_tests"],
        "mismatch": d["mismatch_count"] + d["missing_count"] + d["extra_count"],
    }


# ------------------------------------------------------------- environment --


def _version(cmd):
    try:
        return subprocess.run(cmd, capture_output=True, text=True, timeout=60).stdout.strip()
    except (OSError, subprocess.TimeoutExpired):
        return "unknown"


def environment(runner):
    py = runner.python
    info = {
        "date": datetime.date.today().isoformat(),
        "platform": platform.platform(),
        "machine": platform.machine(),
        "cpu": _cpu_model(),
        "logical_cpus": os.cpu_count(),
        "python": _version([py, "-c", "import sys; print(sys.version.split()[0])"]),
        "pytest": _version([py, "-c", "import pytest; print(pytest.__version__)"]),
        "pytest_xdist": _version([py, "-c", "import xdist; print(xdist.__version__)"])
        or "not installed",
        "rstest": _version([runner.rstest, "--version"]),
        "rstest_commit": _version(["git", "-C", str(REPO), "rev-parse", "--short", "HEAD"]),
    }
    if hasattr(os, "getloadavg"):
        info["load_1m_at_start"] = round(os.getloadavg()[0], 2)
    return info


def _cpu_model():
    if sys.platform == "darwin":
        brand = _version(["sysctl", "-n", "machdep.cpu.brand_string"])
        perf = _version(["sysctl", "-n", "hw.perflevel0.logicalcpu"])
        eff = _version(["sysctl", "-n", "hw.perflevel1.logicalcpu"])
        if perf.isdigit() and eff.isdigit():
            return f"{brand} ({perf} performance + {eff} efficiency cores)"
        return brand
    try:
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith("model name"):
                return line.split(":", 1)[1].strip()
    except OSError:
        pass
    return platform.processor() or "unknown"


def print_env(info, args, header, runs=None):
    print(f"\n### {header}\n")
    print(
        f"Machine: {info['cpu']}, {info['logical_cpus']} logical CPUs, {info['platform']}. "
        f"1-minute load at start: {info.get('load_1m_at_start', 'n/a')}.  "
    )
    print(
        f"Python {info['python']}, pytest {info['pytest']}, pytest-xdist {info['pytest_xdist']}, "
        f"{info['rstest']} (commit {info['rstest_commit']}). {info['date']}.  "
    )
    if runs is not None:  # memory: medians of the sampled runs, no wall times
        print(f"Median of {runs} sampled runs after 1 untimed warm-up.\n")
        return
    print(
        f"Median of {args.repeat} runs after 1 untimed warm-up, min-max in parentheses. "
        f"rstest warm (duration cache from the warm-up).\n"
    )


def check_load(args):
    if not hasattr(os, "getloadavg"):
        return
    load = os.getloadavg()[0]
    if load > args.max_load:
        msg = f"1-minute load {load:.2f} is above --max-load {args.max_load}"
        if args.strict_load:
            raise SystemExit(msg + ": machine is busy, not measuring (drop --strict-load to force)")
        print(f"WARNING: {msg}; numbers may be noisy", file=sys.stderr)


def has_xdist(python):
    return subprocess.run([python, "-c", "import xdist"], capture_output=True).returncode == 0


# ---------------------------------------------------------------- the modes --


def do_sweep(runner, args, tmp):
    base_snap = tmp / "pytest.json"
    shutil.rmtree(CACHE, ignore_errors=True)
    log("pytest serial")
    base_walls, base_cmd = repeat(runner, "pytest", None, base_snap, args)
    base = band(base_walls)
    kinds = ["rstest"] + (["xdist"] if args.xdist else [])
    points, commands = [], {"pytest": base_cmd}
    for n in args.workers:
        row = {"workers": n}
        for kind in kinds:
            log(f"{kind} -n {n}")
            snap = tmp / f"{kind}-{n}.json"
            walls, cmd = repeat(runner, kind, n, snap, args)
            b = band(walls)
            row[kind] = {
                **b,
                "walls": [round(w, 3) for w in walls],
                "speedup": round(base["median"] / b["median"], 2),
                "efficiency": round(base["median"] / b["median"] / n, 2),
                **parity(base_snap, snap),
            }
            commands[kind] = cmd
        points.append(row)
    return {
        "baseline": {**base, "walls": [round(w, 3) for w in base_walls]},
        "points": points,
        "commands": {k: " ".join(_rel(c)) for k, c in commands.items()},
    }


def render_sweep(res):
    base = res["baseline"]
    xd = any("xdist" in p for p in res["points"])
    print(f"pytest serial: {fmt(base)} s\n")
    head = "| -n | rstest (s) | speedup | efficiency |"
    sep = "|---|---|---|---|"
    if xd:
        head += " xdist (s) | speedup | efficiency | faster |"
        sep += "---|---|---|---|"
    print(head + " parity |")
    print(sep + "---|")
    for p in res["points"]:
        r = p["rstest"]
        line = f"| {p['workers']} | {fmt(r)} | {r['speedup']:.2f}x | {r['efficiency']:.0%} |"
        par = [r["parity"]]
        if xd:
            x = p["xdist"]
            # Ties are ties: only name a winner when the spreads don't overlap.
            win = "parity" if overlaps(r, x) else "rstest" if r["median"] < x["median"] else "xdist"
            line += f" {fmt(x)} | {x['speedup']:.2f}x | {x['efficiency']:.0%} | {win} |"
            par.append(x["parity"])
        print(line + f" {min(par)}% |")
    print("\nCommands:\n")
    for k, c in res["commands"].items():
        print(f"- {k}: `{c}`")
    print()


NOOP_TEST = """\
import time

import pytest


@pytest.mark.parametrize("case", range({count}))
def test_noop(case):
    time.sleep(0.05)  # keeps every worker alive long enough to be sampled
"""


def do_memory(runner, args, tmp):
    """Peak memory per -n, on the selected suite and on a no-op suite."""
    noop_dir = tmp / "noop"
    noop_dir.mkdir()
    (noop_dir / "test_noop.py").write_text(NOOP_TEST.format(count=max(args.workers) * 8))
    kinds = ["rstest"] + (["xdist"] if args.xdist else [])
    noop = Runner(runner.python, runner.rstest, None, noop_dir, cwd=noop_dir)
    subjects = {"suite": runner, "noop": noop}
    out = {}
    for label, r in subjects.items():
        out[label] = {}
        for kind in kinds:
            rows = []
            for n in args.workers:
                log(f"memory {label} {kind} -n {n}")
                snap = tmp / f"mem-{label}-{kind}-{n}.json"
                run_once(r, kind, n, snap)  # warm-up
                samples = [
                    run_once(r, kind, n, snap, sample=args.sample)
                    for _ in range(args.memory_repeat)
                ]
                tree = [s["peak_tree_rss"] for s in samples if s["peak_tree_rss"] is not None]
                rows.append(
                    {
                        "workers": n,
                        "max_proc_rss_mib": round(
                            statistics.median(s["max_proc_rss"] for s in samples) / MIB, 1
                        ),
                        "peak_tree_rss_mib": round(statistics.median(tree) / MIB, 1)
                        if tree
                        else None,
                    }
                )
            fit = None
            pts = [(p["workers"], p["peak_tree_rss_mib"]) for p in rows if p["peak_tree_rss_mib"]]
            if len(pts) >= 2:
                a, b, r2 = linear_fit([x for x, _ in pts], [y for _, y in pts])
                fit = {"fixed_mib": round(a, 1), "per_worker_mib": round(b, 1), "r2": round(r2, 3)}
            out[label][kind] = {"points": rows, "fit": fit}
    return out


def render_memory(res, marker):
    what = f"`-m {marker}`" if marker else "the default selection"
    for label, title in (("suite", f"suite ({what})"), ("noop", "no-op suite (worker baseline)")):
        for kind, data in res[label].items():
            print(f"**{kind}, {title}**\n")
            print("| -n | largest process peak (MiB) | whole tree peak (MiB) |")
            print("|---|---|---|")
            for p in data["points"]:
                tree = p["peak_tree_rss_mib"]
                tree_s = f"{tree:.0f}" if tree is not None else "n/a (no psutil)"
                print(f"| {p['workers']} | {p['max_proc_rss_mib']:.0f} | {tree_s} |")
            if data["fit"]:
                f = data["fit"]
                print(
                    f"\nFit: total ≈ {f['fixed_mib']:.0f} MiB + N x {f['per_worker_mib']:.0f} MiB "
                    f"(R² {f['r2']})\n"
                )
            else:
                print()


def do_grid(runner, args, tmp):
    """-n x BLAS thread cap over the blas tests; outcome drift across cells."""
    grid, outcomes = [], {}
    for n in args.workers:
        for t in args.threads:
            extra = {} if t == "unset" else dict.fromkeys(BLAS_VARS, t)
            env_label = "unset" if t == "unset" else t
            log(f"grid -n {n} threads {env_label}")
            snap = tmp / f"grid-{n}-{env_label}.json"
            walls, _ = repeat(runner, "rstest", n, snap, args, extra_env=extra)
            doc = json.loads(snap.read_text())
            outcomes[(n, env_label)] = {
                nid: tuple(v.get(k) for k in ("setup", "call", "teardown"))
                for nid, v in doc["tests"].items()
            }
            counts = doc.get("meta", {}).get("counts", {})
            grid.append({"workers": n, "threads": env_label, **band(walls), "counts": counts})
    # Outcome drift: any test whose phase outcomes differ between cells.
    all_ids = set().union(*(o.keys() for o in outcomes.values()))
    drift = sorted(nid for nid in all_ids if len({o.get(nid) for o in outcomes.values()}) > 1)
    return {"cells": grid, "outcome_drift": drift}


def render_grid(res, threads):
    cells = {(c["workers"], c["threads"]): c for c in res["cells"]}
    workers = sorted({c["workers"] for c in res["cells"]})
    print(
        "Wall seconds, median (min-max). Columns: the thread cap set in "
        + ", ".join(BLAS_VARS)
        + ".\n"
    )
    print("| -n | " + " | ".join(threads) + " |")
    print("|---|" + "---|" * len(threads))
    for n in workers:
        row = [fmt(cells[(n, t)]) for t in threads]
        print(f"| {n} | " + " | ".join(row) + " |")
    drift = res["outcome_drift"]
    print(
        f"\nOutcome drift across cells: {len(drift)} test(s)"
        + (": " + ", ".join(drift[:10]) if drift else " (every test same outcome in every cell)")
        + "\n"
    )


# ------------------------------------------------------------------- main --


def _rel(cmd):
    """Command line with this machine's paths shortened, for publishing."""
    out = []
    for c in cmd:
        c = str(c)
        if c.startswith(str(HERE)):
            c = os.path.relpath(c, HERE)
        elif os.path.isabs(c):
            c = os.path.basename(c)
        out.append(c)
    return out


def log(msg):
    print(f"[measure] {msg}", file=sys.stderr, flush=True)


def int_list(s):
    return [int(x) for x in s.split(",") if x]


def default_workers():
    n, out = 1, []
    cpus = os.cpu_count() or 1
    while n < cpus:
        out.append(n)
        n *= 2
    return [*out, cpus]


def main():
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    ap.add_argument("--workers", type=int_list, default=None, help="comma-separated -n values")
    ap.add_argument(
        "--max-workers", type=int, default=None, help="sweep 1,2,4,.. up to this (and include it)"
    )
    ap.add_argument("--repeat", type=int, default=5, help="timed runs per point (median reported)")
    ap.add_argument("--no-xdist", dest="xdist", action="store_false", help="skip the xdist series")
    ap.add_argument("-m", dest="marker", default=None, help="marker expression, e.g. blas")
    ap.add_argument("--memory", action="store_true", help="memory model instead of the sweep")
    ap.add_argument("--memory-repeat", type=int, default=3)
    ap.add_argument("--sample", type=float, default=0.1, help="tree sampling interval (s)")
    ap.add_argument("--grid", action="store_true", help="-n x BLAS threads grid (blas tests)")
    ap.add_argument("--threads", default="1,2,4,unset", help="grid thread caps")
    ap.add_argument("--max-load", type=float, default=2.0)
    ap.add_argument("--strict-load", action="store_true", help="refuse to run above --max-load")
    ap.add_argument("--json", type=Path, default=RESULTS, help="raw results file")
    args = ap.parse_args()

    if args.workers is None:
        if args.max_workers:
            n, ws = 1, []
            while n < args.max_workers:
                ws.append(n)
                n *= 2
            args.workers = [*ws, args.max_workers]
        else:
            args.workers = default_workers()
    python = os.environ.get("PYTHON", sys.executable)
    rstest = os.environ.get("RSTEST", "rstest")
    if args.grid:
        args.marker = "blas"
        args.threads = [t for t in args.threads.split(",") if t]
    if args.xdist and not has_xdist(python):
        log("pytest-xdist not installed for this interpreter: skipping the xdist series")
        args.xdist = False
    runner = Runner(python, rstest, args.marker, HERE / "tests")

    check_load(args)
    info = environment(runner)
    mode = "grid" if args.grid else "memory" if args.memory else "sweep"
    with tempfile.TemporaryDirectory() as t:
        tmp = Path(t)
        if mode == "sweep":
            res = do_sweep(runner, args, tmp)
            print_env(info, args, "cpu-bench worker sweep")
            render_sweep(res)
        elif mode == "memory":
            res = do_memory(runner, args, tmp)
            print_env(info, args, "cpu-bench memory", runs=args.memory_repeat)
            render_memory(res, args.marker)
        else:
            res = do_grid(runner, args, tmp)
            print_env(info, args, "cpu-bench worker x BLAS-thread grid")
            render_grid(res, args.threads)

    # Keep every mode's latest result side by side in one file.
    doc = json.loads(args.json.read_text()) if args.json.exists() else {}
    doc[mode] = {"environment": info, "repeat": args.repeat, "marker": args.marker, **res}
    args.json.write_text(json.dumps(doc, indent=1) + "\n")

    bad = []
    if mode == "sweep":
        bad = [
            f"{k} -n {p['workers']}: parity {p[k]['parity']}%"
            for p in res["points"]
            for k in ("rstest", "xdist")
            if k in p and p[k]["parity"] < 100.0
        ]
    elif mode == "grid" and res["outcome_drift"]:
        bad = [f"outcome drift across grid cells: {len(res['outcome_drift'])} test(s)"]
    for b in bad:
        print(f"PARITY: {b}", file=sys.stderr)
    return 1 if bad else 0


if __name__ == "__main__":
    raise SystemExit(main())
