"""A/B: plain worker spawns vs `--fork-pool`, on the prepared corpus suites.

Decides whether `--fork-pool` should be the default: same release binary, same
suite venvs and policy (`run.py`'s `Suite`), only the flag differs. Runs
alternate plain/fork within each repeat (which goes first flips each repeat),
so machine drift lands on both arms. Every point is the median of `--repeat`
runs with its min-max band; the first repeat of each arm is diffed per test
(outcomes must match, or the fork arm is not a win whatever its time).

    caffeinate -ims python3 corpus/fork_ab.py --only click,anyio --workers auto,4

The worker comes from this checkout (RSTEST_WORKER_PATH), so the zygote's
argv protocol always matches the binary. Data -> corpus/fork_ab.json.
"""

from __future__ import annotations

import argparse
import json
import os
import statistics
import sys
import time

import tomllib
from bench import _awake_run
from run import HERE, REPO, Suite, diff, log


def _arm(suite, workers, fork):
    """One timed rstest run of `suite`; returns (snapshot path, wall)."""
    snap = suite.dir / f"fork_ab_{'fork' if fork else 'plain'}.json"
    snap.unlink(missing_ok=True)
    argv = suite.rstest_argv(snap, workers)
    if fork:
        argv.insert(1, "--fork-pool")
    env = suite.env()
    env["RSTEST_WORKER_PATH"] = str(REPO / "python")

    def run():
        from run import sh

        t0 = time.monotonic()
        r = sh(argv, cwd=suite.cwd(), env=env)
        wall = time.monotonic() - t0
        if not snap.exists():
            raise RuntimeError(f"{suite.name}: no snapshot (rc={r.returncode})\n{r.stderr[-600:]}")
        return snap, wall

    return _awake_run(run)


def _band(walls):
    return statistics.median(walls), min(walls), max(walls)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    ap.add_argument("--only", required=True, help="comma-separated suites")
    ap.add_argument("--workers", default="auto", help="comma-separated -n values (auto or N)")
    ap.add_argument("--repeat", type=int, default=5)
    ap.add_argument("--warmup", type=int, default=1)
    ap.add_argument("--rstest", default=str(REPO / "target" / "release" / "rstest"))
    args = ap.parse_args()

    cfg = tomllib.loads((HERE / "suites.toml").read_text())
    rows = []
    for name in [n for n in args.only.split(",") if n]:
        suite = Suite(name, cfg[name], "", args.rstest)
        for workers in args.workers.split(","):
            log(f"{name} -n {workers}: warmup")
            for _ in range(args.warmup):
                _arm(suite, workers, False)
                _arm(suite, workers, True)
            walls = {False: [], True: []}
            parity = None
            for rep in range(args.repeat):
                order = (False, True) if rep % 2 == 0 else (True, False)
                snaps = {}
                for fork in order:
                    snap, wall = _arm(suite, workers, fork)
                    walls[fork].append(wall)
                    if rep == 0:
                        keep = snap.with_name(snap.stem + "_first.json")
                        snap.replace(keep)
                        snaps[fork] = keep
                if rep == 0:
                    d = diff(snaps[False], snaps[True])
                    parity = (
                        f"{d['score']}% ({d['candidate_tests']} tests, "
                        f"{d['mismatch_count']} differ, {d['missing_count']} missing, "
                        f"{d['extra_count']} extra)"
                    )
                log(f"  rep {rep + 1}: plain {walls[False][-1]:.2f}s fork {walls[True][-1]:.2f}s")
            plain, fork = _band(walls[False]), _band(walls[True])
            rows.append(
                {
                    "suite": name,
                    "workers": workers,
                    "plain": walls[False],
                    "fork": walls[True],
                    "plain_median": plain[0],
                    "fork_median": fork[0],
                    "saved_pct": round(100 * (plain[0] - fork[0]) / plain[0], 1),
                    "parity": parity,
                }
            )

    (HERE / "fork_ab.json").write_text(
        json.dumps(
            {
                "date": time.strftime("%Y-%m-%d"),
                "logical_cpus": os.cpu_count(),
                "load_1m_at_end": os.getloadavg()[0],
                "repeat": args.repeat,
                "rows": rows,
            },
            indent=2,
        )
    )
    print("\n| suite | -n | plain (s) | fork (s) | saved | parity |")
    print("|---|---|---|---|---|---|")
    for r in rows:
        p, f = _band(r["plain"]), _band(r["fork"])
        print(
            f"| {r['suite']} | {r['workers']} | {p[0]:.2f} ({p[1]:.2f}-{p[2]:.2f}) "
            f"| {f[0]:.2f} ({f[1]:.2f}-{f[2]:.2f}) | {r['saved_pct']:+.1f}% | {r['parity']} |"
        )


if __name__ == "__main__":
    sys.exit(main())
