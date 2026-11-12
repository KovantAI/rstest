"""A/B worker start-up modes on the prepared corpus suites.

Arms (`--arms`, first is the baseline the others are diffed against):

  plain  plain worker spawns
  core   `--fork-pool` (fork off a zygote that imported the vendored core)
  app    `--fork-pool=app` (the zygote also imported the app's modules)
  core2  `core` again: a control for the suite's own run-to-run noise

Same release binary, same suite venvs and policy (`run.py`'s `Suite`), only the
flag differs. Arms rotate within each repeat (which goes first moves each
repeat), so machine drift lands on every arm. Every point is the median of
`--repeat` runs with its min-max band. Every repeat of every arm is diffed per
test against the baseline arm (outcomes must match, or the arm is not a win
whatever its time; a fork-related crash can be intermittent, so the worst
repeat is reported).

`app` needs a manifest from earlier runs: after the warmup, it is re-run until
a run prints no `--fork-pool=app:` note (the template converged), up to
`--converge` runs.

`--memory` samples system-wide used memory (psutil) during each run and reports
the peak above the pre-run level. Forked workers are reparented to launchd, so
the per-tree sampler in procmem.py can't see them; this is coarser (other
processes count too), so keep the machine quiet. Its sampler costs a little CPU:
take timing numbers from a run without it.

    caffeinate -ims python3 corpus/fork_ab.py --only click,anyio --workers auto,4
    caffeinate -ims .venv/bin/python corpus/fork_ab.py --arms core,app --only langchain \\
        --memory --out corpus/bench-results/<date>-warm-template-ab.json

The worker comes from this checkout (RSTEST_WORKER_PATH), so the zygote's
argv protocol always matches the binary.
"""

from __future__ import annotations

import argparse
import json
import os
import statistics
import sys
import threading
import time
from pathlib import Path
from typing import Any

import tomllib
from bench import _awake_run
from run import HERE, REPO, Suite, diff, log, sh

# `core2` repeats `core`: a control arm, so `core` vs `core2` diffs measure the
# suite's own run-to-run noise, apart from what `app` changes.
ARMS = {"plain": [], "core": ["--fork-pool"], "core2": ["--fork-pool"], "app": ["--fork-pool=app"]}
NOTE = "--fork-pool=app:"


class _MemPeak:
    """Peak system-wide used memory above the level at start, sampled."""

    def __init__(self, interval=0.1):
        import psutil

        self._vm = psutil.virtual_memory
        self.base = self._vm().used
        self.peak = 0
        self._stop = threading.Event()
        self._t = threading.Thread(target=self._loop, args=(interval,), daemon=True)
        self._t.start()

    def _loop(self, interval):
        while not self._stop.wait(interval):
            self.peak = max(self.peak, self._vm().used - self.base)

    def stop(self):
        self._stop.set()
        self._t.join()
        return self.peak


def _arm(suite, workers, arm, memory=False):
    """One timed rstest run of `suite`; returns (snapshot, wall, stderr, mem)."""
    snap = suite.dir / f"fork_ab_{arm}.json"
    snap.unlink(missing_ok=True)
    argv = suite.rstest_argv(snap, workers)
    argv[1:1] = ARMS[arm]
    env = suite.env()
    env["RSTEST_WORKER_PATH"] = str(REPO / "python")
    out = {}

    def run():
        mem = _MemPeak() if memory else None
        t0 = time.monotonic()
        r = sh(argv, cwd=suite.cwd(), env=env)
        wall = time.monotonic() - t0
        out["mem"] = mem.stop() if mem else None
        out["stderr"] = r.stderr or ""
        if not snap.exists():
            raise RuntimeError(f"{suite.name}: no snapshot (rc={r.returncode})\n{r.stderr[-600:]}")
        return snap, wall

    snap, wall = _awake_run(run)
    return snap, wall, out["stderr"], out["mem"]


def _band(xs):
    return statistics.median(xs), min(xs), max(xs)


def _converge(suite, workers, limit):
    """Run `app` until a run prints no template note; returns how many it took
    and the last run's notes (empty when converged)."""
    notes = []
    for i in range(1, limit + 1):
        _, _, err, _ = _arm(suite, workers, "app")
        notes = [line for line in err.splitlines() if NOTE in line]
        for line in notes:
            log(f"  converge {i}: {line}")
        if not notes:
            return i, []
    return limit, notes


def _point(suite, workers, arms, args, noisy, noisy_failures):
    """One (suite, -n) row: warmup, app convergence, timed repeats, parity."""
    name, base = suite.name, arms[0]
    log(f"{name} -n {workers}: warmup")
    for _ in range(args.warmup):
        for arm in arms:
            _arm(suite, workers, arm)
    converged = None
    if "app" in arms:
        runs, left = _converge(suite, workers, args.converge)
        converged = {"runs": runs, "unsettled_notes": left}
    walls = {a: [] for a in arms}
    mems = {a: [] for a in arms}
    parity = {a: [] for a in arms[1:]}
    notes = dict.fromkeys(arms, 0)
    for rep in range(args.repeat):
        order = arms[rep % len(arms) :] + arms[: rep % len(arms)]
        snaps = {}
        for arm in order:
            snap, wall, err, mem = _arm(suite, workers, arm, args.memory)
            walls[arm].append(wall)
            if mem is not None:
                mems[arm].append(mem)
            notes[arm] += sum(NOTE in line for line in err.splitlines())
            keep = snap.with_name(f"{snap.stem}_rep{rep}.json")
            snap.replace(keep)
            snaps[arm] = keep
        for arm in arms[1:]:
            d = diff(snaps[base], snaps[arm], noisy, noisy_failures)
            parity[arm].append(
                {
                    "score": d["score"],
                    "tests": d["candidate_tests"],
                    "differ": d["mismatch_count"],
                    "missing": d["missing_count"],
                    "extra": d["extra_count"],
                }
            )
        for keep in snaps.values():
            keep.unlink(missing_ok=True)
        log(
            f"  rep {rep + 1}: "
            + " ".join(f"{a} {walls[a][-1]:.2f}s" for a in arms)
            + "".join(f" | {a} parity {parity[a][-1]['score']}%" for a in arms[1:])
        )
    base_med = _band(walls[base])[0]
    row: dict[str, Any] = {
        "suite": name,
        "workers": workers,
        "walls": walls,
        "medians": {a: round(_band(walls[a])[0], 3) for a in arms},
        "saved_pct": {
            a: round(100 * (base_med - _band(walls[a])[0]) / base_med, 1) for a in arms[1:]
        },
        "worst_parity": {a: min(parity[a], key=lambda p: p["score"]) for a in arms[1:]},
        "parity": parity,
        "template_notes_during_timing": notes,
    }
    if converged is not None:
        row["app_converged"] = converged
    if args.memory:
        row["mem_peak_mib"] = {a: round(_band(mems[a])[0] / 2**20, 1) for a in arms if mems[a]}
    return row


def _write(args, arms, rows):
    Path(args.out).write_text(
        json.dumps(
            {
                "date": time.strftime("%Y-%m-%d"),
                "logical_cpus": os.cpu_count(),
                "load_1m_at_end": os.getloadavg()[0],
                "arms": arms,
                "repeat": args.repeat,
                "memory": args.memory,
                "rows": rows,
            },
            indent=2,
        )
    )


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    ap.add_argument("--only", required=True, help="comma-separated suites")
    ap.add_argument("--arms", default="plain,core", help=f"comma-separated, of {','.join(ARMS)}")
    ap.add_argument("--workers", default="auto", help="comma-separated -n values (auto or N)")
    ap.add_argument("--repeat", type=int, default=5)
    ap.add_argument("--warmup", type=int, default=1)
    ap.add_argument("--converge", type=int, default=6, help="max app runs to settle the manifest")
    ap.add_argument("--memory", action="store_true", help="sample system memory (needs psutil)")
    ap.add_argument("--rstest", default=str(REPO / "target" / "release" / "rstest"))
    ap.add_argument("--out", default=str(HERE / "fork_ab.json"))
    args = ap.parse_args()
    arms = [a for a in args.arms.split(",") if a]
    if len(arms) < 2 or any(a not in ARMS for a in arms):
        sys.exit(f"--arms: two or more of {', '.join(ARMS)}")

    cfg = tomllib.loads((HERE / "suites.toml").read_text())
    rows: list[dict[str, Any]] = []
    for name in [n for n in args.only.split(",") if n]:
        suite = Suite(name, cfg[name], "", args.rstest)
        noisy = cfg[name].get("noisy", [])
        noisy_failures = cfg[name].get("noisy_failures", [])
        for workers in args.workers.split(","):
            try:
                row = _point(suite, workers, arms, args, noisy, noisy_failures)
            except Exception as e:
                # One broken suite must not cost the rest of a long run.
                log(f"  {name} -n {workers}: failed: {e}")
                row: dict[str, Any] = {"suite": name, "workers": workers, "error": str(e)[-800:]}
            rows.append(row)
            _write(args, arms, rows)

    _write(args, arms, rows)
    other = arms[1:]
    print(
        "\n| suite | -n | "
        + " | ".join(f"{a} (s)" for a in arms)
        + " | "
        + " | ".join(f"{a} saved | {a} worst parity" for a in other)
        + " |"
    )
    print("|---" * (2 + len(arms) + 2 * len(other)) + "|")
    for r in rows:
        if "error" in r:
            print(f"| {r['suite']} | {r['workers']} | error: {r['error'].splitlines()[0]} |")
            continue
        cells = []
        for a in arms:
            m, lo, hi = _band(r["walls"][a])
            cells.append(f"{m:.2f} ({lo:.2f}-{hi:.2f})")
        for a in other:
            p = r["worst_parity"][a]
            cells.append(f"{r['saved_pct'][a]:+.1f}%")
            cells.append(f"{p['score']}% ({p['differ']} differ, {p['missing']} missing)")
        print(f"| {r['suite']} | {r['workers']} | " + " | ".join(cells) + " |")


if __name__ == "__main__":
    sys.exit(main())
