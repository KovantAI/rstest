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
macOS 26.6.1, AC power, 1-minute load under 2.0 at the start of each block.
CPython 3.13.13, pytest 9.1.1, pytest-xdist 3.8.0, rstest 0.7.0 (commit
`b477551`), 2026-09-26. Median of 5 runs after 1 untimed warm-up, min-max in
parentheses; rstest warm. Every run: 100% per-test outcome parity with the
serial pytest baseline.

### Worker sweep

pytest serial: 14.97 s (14.88-14.99).

| -n | rstest (s) | speedup | efficiency | xdist (s) | speedup | efficiency |
|---|---|---|---|---|---|---|
| 1 | **14.99** (14.94-15.06) | 1.00x | 100% | 15.26 (15.17-15.35) | 0.98x | 98% |
| 2 | **7.78** (7.77-7.80) | 1.92x | 96% | 7.96 (7.89-8.03) | 1.88x | 94% |
| 4 | **4.03** (4.01-4.04) | 3.71x | 93% | 4.20 (4.17-4.21) | 3.56x | 89% |
| 8 | **2.18** (2.15-2.20) | 6.87x | 86% | 2.43 (2.43-2.46) | 6.16x | 77% |
| 10 | **1.97** (1.96-1.97) | 7.60x | 76% | 2.20 (2.19-2.21) | 6.80x | 68% |
| 14 | **1.74** (1.72-1.86) | 8.60x | 61% | 2.04 (2.03-2.06) | 7.34x | 52% |

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
- rstest is ahead of xdist at every `-n` from 2 up, with no overlap in the
  spreads: 4% at `-n 4`, 17% at `-n 14`. On a suite this short that gap is
  mostly per-run overhead (worker start-up and dispatch), so expect it to
  shrink as a share of wall time on longer suites.

### Memory

Peak RSS, median of 3 sampled runs. "Largest process" is exact
(`getrusage`); "whole tree" is the sum over the driver and every worker,
sampled every 100 ms with psutil.

No-op suite (the cost of a worker before any test code):

| | per worker | driver / controller |
|---|---|---|
| rstest | 38 MiB | ~10 MiB (Rust orchestrator) |
| pytest-xdist | 38 MiB | ~38 MiB (a full Python process) |

`blas` suite (about 216 MB of arrays per test), in MiB:

| -n | rstest largest process | rstest whole tree | xdist whole tree |
|---|---|---|---|
| 1 | 416 | 424 | 452 |
| 2 | 416 | 838 | 865 |
| 4 | 417 | 1,666 | 1,694 |
| 8 | 417 | 3,328 | 3,358 |
| 10 | 417 | 4,088 | 3,872 |
| 14 | 415 | 4,701 | 4,342 |

Up to `-n 8` the tree peak is exactly driver + N x per-worker peak (8 x 417 =
3,336). At `-n 10` and `-n 14` it comes in under that: with 32 short tests
over 14 workers, the workers' peaks stop coinciding. So N x per-worker peak is
the upper bound, reached whenever all workers are busy at once, which is the
normal case for a long suite. Size `-n` against it.

### Worker x BLAS-thread grid

The `blas` tests, rstest, thread cap set in `OMP_NUM_THREADS`,
`OPENBLAS_NUM_THREADS`, `MKL_NUM_THREADS` and `VECLIB_MAXIMUM_THREADS` (numpy
on macOS arm64 uses Accelerate, which reads only the last one). Wall seconds:

| -n | 1 thread | 2 | 4 | unset |
|---|---|---|---|---|
| 1 | 13.24 (13.03-13.30) | 7.45 (7.38-7.52) | 7.53 (7.40-7.62) | 7.47 (7.38-7.55) |
| 2 | 7.24 (7.22-7.26) | 6.44 (6.42-6.50) | 6.48 (6.47-6.53) | 6.47 (6.45-6.51) |
| 4 | 6.01 (5.95-6.09) | 5.45 (5.40-5.47) | 5.45 (5.37-5.45) | 5.45 (5.42-5.51) |
| 10 | 5.32 (5.24-5.63) | 5.40 (5.37-5.45) | 5.43 (5.35-5.48) | 5.42 (5.33-5.50) |
| 14 | 5.08 (4.83-5.16) | 4.87 (4.79-4.92) | 4.91 (4.84-5.14) | 5.03 (4.97-5.10) |

No test changed outcome in any cell (these tests compare with a tolerance, so
this does not rule out a tight `==` flipping; see
[Numeric determinism](../../docs/guides/parallel-safety.md#numeric-determinism-ml-numerics-suites)).

Reading it (Accelerate, this machine):

- **Below the core count, library threads help.** At `-n 1` a one-thread cap
  takes 13.2 s against 7.5 s unset; at `-n 4`, 6.0 s against 5.5 s.
- **At or past the core count, the cap doesn't matter.** Every column at
  `-n 10` and `-n 14` sits within the others' spread. No oversubscription
  penalty showed up.
- **So one thread per worker is not a free default.** It costs time at low
  `-n` and bought nothing at high `-n` here.
- This is Accelerate, whose threads the OS schedules onto shared matrix units.
  OpenBLAS (numpy's Linux wheels) and MKL run their own spinning thread pools
  and can behave differently: the CI workflow runs the same grid on Linux for
  that data point. Measure your own stack with `--grid` before pinning.
- Past about 4 workers this suite barely scales (13.2 s serial, 5 s best): the
  matrix units and memory bandwidth are shared, so more processes don't add
  throughput. BLAS-heavy suites gain less from `-n` than pure-Python ones.
