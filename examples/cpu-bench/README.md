# Example: CPU-bound worker scaling

A small, self-contained suite that is **purely CPU-bound**: no sleeps, no
network, no files, and the same result on every run. It answers one question
with measured numbers: how does wall time scale with `-n` when every test is
real compute, under rstest and under pytest-xdist at the same worker count?

The wait-bound counterpart (cold vs warm duration cache) is
[`examples/ci-bench`](../ci-bench).

## The suite

- `tests/test_compute_*.py`: 64 pure-Python tests (a prime sieve, an n-body
  integration, a longest-common-subsequence table), each about 0.25 s, about
  15 s serial. Pure Python on purpose: no BLAS thread pools blur the curve.
  Every test does the same work, so no single long test caps the scaling.
- `tests/test_blas.py`, marked `blas` and excluded by default: 32 numpy matmul
  and solve tests (three 3000x3000 float64 matrices each, about 216 MB). Used
  only for the memory model and the worker x thread grid. Needs numpy.

## Run it

From this directory, in a venv with `pytest`, `pytest-xdist` and the rstest
wheel (plus `psutil` and `numpy` for `--memory` / `--grid`):

```bash
python measure.py --workers 1,2,4,8,10,14 --repeat 5      # worker sweep
python measure.py --memory -m blas --workers 1,2,4,8,14   # memory per -n
python measure.py --grid --workers 1,2,4,10,14 --threads 1,2,4,unset
```

`RSTEST=/path/to/rstest` and `PYTHON=/path/to/venv/python` point at a specific
build and interpreter. Every mode does one untimed warm-up, then reports the
median of `--repeat` runs with the min-max spread, diffs every run's per-test
outcomes against the serial pytest baseline, and writes `results.json`.

The [CPU bench workflow](../../.github/workflows/example-cpu-bench.yml) runs the
sweep weekly on a GitHub `ubuntu-latest` runner (`-n 1,2,4`) and posts the
table to the job summary. Shared runners are noisy: treat those numbers as a
reproducibility check, not a headline.

## Measured result

Apple M4 Max (10 performance + 4 efficiency cores, 14 logical CPUs), 36 GB,
macOS 26.6.1, AC power, 1-minute load 5.18 (sweep), 10.05 (memory) and 4.12
(grid) at the start of each block, each block starting right after the
previous one's 14-worker points (above the tool's 2.0 guard, so treat gaps
inside a few percent as noise). CPython 3.13.13, pytest 9.2.0, pytest-xdist
3.8.0, rstest 0.11.0 (commit `6a8c9e4`, the `v0.11.0` tag), 2026-11-10. Median of 5 runs after 1 untimed warm-up, min-max in
parentheses; rstest warm. Every run: 100% per-test outcome parity with the
serial pytest baseline.

### Worker sweep

pytest serial: 15.20 s (15.06-15.23).

| -n | rstest (s) | speedup | efficiency | xdist (s) | speedup | efficiency |
|---|---|---|---|---|---|---|
| 1 | 15.30 (15.16-15.32) | 0.99x | 99% | 15.42 (15.13-17.12) | 0.99x | 99% |
| 2 | **7.88** (7.84-7.89) | 1.93x | 96% | 7.94 (7.93-7.98) | 1.91x | 96% |
| 4 | 4.14 (4.12-4.18) | 3.67x | 92% | 4.19 (4.17-4.21) | 3.63x | 91% |
| 8 | 2.27 (2.24-2.47) | 6.70x | 84% | 2.41 (2.39-2.41) | 6.31x | 79% |
| 10 | **2.06** (2.06-2.07) | 7.38x | 74% | 2.18 (2.18-2.19) | 6.97x | 70% |
| 14 | **1.82** (1.80-1.87) | 8.35x | 60% | 2.00 (2.00-2.01) | 7.60x | 54% |

Commands: `python -m pytest -q tests` (serial), `python -m pytest -q -n N tests`
(xdist), `rstest -n N -q tests`.

Reading it:

- Speedup tracks `-n` closely up to 4 and stays high to 8, for both runners.
- Past the 10 performance cores the curve flattens: the 4 extra workers land on
  efficiency cores. On this machine, CPU-bound suites gain **up to the
  performance-core count**, not the logical CPU count.
- Part of the flattening is this suite's size, not the runner: 64 tests of
  about 0.25 s split unevenly past `-n 8` (at `-n 14` the busiest worker still
  runs 5 tests, a 1.25 s floor before any start-up cost). A longer suite of the
  same shape flattens later.
- rstest is ahead of xdist at `-n 2`, `-n 10` and `-n 14` with no overlap in
  the spreads: 1% at `-n 2`, 9% at `-n 14`. At `-n 1`, `-n 4` and `-n 8` the
  spreads overlap. On a suite this short that gap is
  mostly per-run overhead (worker start-up and dispatch), so expect it to
  shrink as a share of wall time on longer suites.

### Memory

Peak RSS, median of 3 sampled runs. "Largest process" is exact
(`getrusage`); "whole tree" is the sum over the driver and every worker,
sampled every 100 ms with psutil.

No-op suite (the cost of a worker before any test code):

| | per worker | driver / controller |
|---|---|---|
| rstest | 39 MiB | ~12 MiB (Rust orchestrator) |
| pytest-xdist | 38 MiB | ~38 MiB (a full Python process) |

`blas` suite (about 216 MB of arrays per test), in MiB:

| -n | rstest largest process | rstest whole tree | xdist whole tree |
|---|---|---|---|
| 1 | 417 | 428 | 452 |
| 2 | 417 | 842 | 866 |
| 4 | 415 | 1,668 | 1,697 |
| 8 | 417 | 3,335 | 3,358 |
| 14 | 416 | 5,688 | 4,345 |

The tree peak is driver + N x per-worker peak (8 x 417 = 3,336; 14 x 406 =
5,684). With 32 short tests over 14 workers the workers' peaks can stop
coinciding: in this run they did under xdist (4,345) but not under rstest, and
the 2026-09-30 run had both at 4,300-4,600. So N x per-worker peak is the upper
bound, reached whenever all workers are busy at once, which is the normal case
for a long suite. Size `-n` against it.

### Worker x BLAS-thread grid

The `blas` tests, rstest, thread cap set in `OMP_NUM_THREADS`,
`OPENBLAS_NUM_THREADS`, `MKL_NUM_THREADS` and `VECLIB_MAXIMUM_THREADS` (numpy
on macOS arm64 uses Accelerate, which reads only the last one). Wall seconds:

| -n | 1 thread | 2 | 4 | unset |
|---|---|---|---|---|
| 1 | 13.32 (13.27-13.43) | 7.58 (7.44-7.66) | 7.48 (7.45-7.68) | 7.50 (7.47-7.67) |
| 2 | 7.26 (7.22-7.27) | 6.46 (6.40-6.63) | 6.48 (6.28-6.53) | 6.52 (6.42-6.64) |
| 4 | 5.85 (5.80-5.88) | 5.33 (5.29-5.35) | 5.31 (5.27-5.38) | 5.33 (5.21-5.35) |
| 10 | 5.33 (5.30-5.39) | 5.41 (5.37-5.50) | 5.33 (5.27-5.38) | 5.34 (5.28-5.38) |
| 14 | 5.80 (5.64-5.85) | 5.42 (5.38-5.49) | 5.44 (5.31-5.45) | 5.38 (5.23-5.43) |

No test changed outcome in any cell (these tests compare with a tolerance, so
this does not rule out a tight `==` flipping; see
[Numeric determinism](../../docs/guides/parallel-safety.md#numeric-determinism-ml-numerics-suites)).

Reading it (Accelerate, this machine):

- **Below the core count, library threads help.** At `-n 1` a one-thread cap
  takes 13.3 s against 7.5 s unset; at `-n 4`, 5.9 s against 5.3 s.
- **At or past the core count, the cap barely matters, or one thread is
  slower.** At `-n 10` the columns sit within about 2% of each other. At
  `-n 14` one thread was the slowest cell (5.8 s against 5.4 s unset). No
  oversubscription penalty showed up for leaving it unset.
- **So one thread per worker is not a free default.** It costs time at low
  `-n` and bought nothing at high `-n` here.
- This is Accelerate, whose threads the OS schedules onto shared matrix units.
  OpenBLAS (numpy's Linux wheels) and MKL run their own spinning thread pools
  and can behave differently: the CI workflow runs the same grid on Linux for
  that data point. Measure your own stack with `--grid` before pinning.
- Past about 4 workers this suite barely scales (13.3 s serial, 5.3 s best): the
  matrix units and memory bandwidth are shared, so more processes don't add
  throughput. BLAS-heavy suites gain less from `-n` than pure-Python ones.
