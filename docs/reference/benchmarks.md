# Benchmarks

Numbers from rstest's compatibility battery: real open-source suites, run
end-to-end under pytest, pytest-xdist, and rstest at the same worker count,
with per-test outcome diffing against the pytest baseline. Every table is
measured under the [methodology](#methodology) below.

## Environment

| | |
|---|---|
| Machine | Apple M4 Max, 10 performance + 4 efficiency cores, 14 logical CPUs |
| OS | macOS 26.6.1 (arm64) |
| Python | CPython 3.13.13 |
| Runners | rstest 0.7.0, pytest 9.1.1, pytest-xdist 3.8.0 |
| Date | 2026-09-26 |

Suite versions: pandas 3.0.5 (wheel), aiohttp `12ea5a58`, django-allauth
`d7d5d39a`, rich `46cebbb0`, sympy `d701e594`, scikit-learn 1.9.1 (wheel).
Raw results: [`corpus/bench-results/`](https://github.com/KovantAI/rstest/tree/main/corpus/bench-results).

**What each result file records** (`environment` block): date, platform
string, logical CPU count, 1-minute load average at the start, `rstest
--version`, the rstest repo commit, every suite's pinned commit, repeat and
warm-up counts, and warm or cold cache. Per point it stores the median and
min-max of the timed walls and the parity counts. Recorded load at the start:
1.98 (suite table), 1.84 (aiohttp cold), 9.6 (sympy and scikit-learn sweep),
14.42 (scikit-learn memory grid). rstest commit: `b477551`, except the aiohttp
cold run (`728e2d4`); both are 0.7.0.

**Not recorded in those files:** the CPU model (taken from the
[`examples/cpu-bench`](https://github.com/KovantAI/rstest/tree/main/examples/cpu-bench)
run the same day, which records it), memory size, power state, whether the
machine was kept awake by `caffeinate` (the tooling only detects and re-runs a
slept-through run), the Python / pytest / pytest-xdist versions (they come from
the suites' venvs and per-run snapshots, which later runs overwrite), and the
individual timed walls behind each median.

## Results

<!-- --8<-- [start:suite-table] -->
| Suite | Tests | pytest serial | xdist `-n 8` | rstest `-n 8` | Outcome parity |
|---|---|---|---|---|---|
| pandas | 193,843 | 186s | 89s | **42s** | 100% |
| aiohttp | 4,469 | 193s | 161s | **67s** warm (150s cold) | 99.93-99.96% (socket-leak flake, hits xdist too) |
| django-allauth | 2,050 | 26s | 8.8s | **5.7s** (8.4s at its recommended `-n 4`) | 100% |
| rich | 981 | 3.7s | 2.7s | **2.4s** | 100% |
<!-- --8<-- [end:suite-table] -->

Median of 5 timed runs after one warm-up, same `-n` for both runners. Bold:
faster, with no overlap in the min-max spreads (they are in
[`corpus/bench-results/`](https://github.com/KovantAI/rstest/tree/main/corpus/bench-results)).
Warm means rstest had its duration cache from a prior run; cold means it was
deleted before every run.

aiohttp's parity gap: some of its tests leak sockets, which are
garbage-collected inside whichever test runs next, and that test fails under
the suite's warnings-as-errors config. Any parallel runner moves that point
(xdist at `-n 8` measured 99.96%); see
[Parity divergences §10](parity-divergences.md#10-leaked-resource-warning-attribution).

!!! note "Numbers single-sourced"
    This table is the canonical source for the suite numbers. Other pages
    (the [home page](../index.md)) embed it via snippet, so the figures only
    ever live here.

## CPU-bound suites

What happens when every test is real compute, with nothing to overlap? Three
suites, each swept over `-n` under rstest and pytest-xdist at the same worker
count, against the serial pytest baseline. Measured under the
[methodology](#methodology) on the primary machine (Apple M4 Max, 10
performance + 4 efficiency cores).

<!-- --8<-- [start:cpu-table] -->
| Suite | What it is | pytest serial | `-n 4` | `-n 10` | `-n 14` | rstest vs xdist |
|---|---|---|---|---|---|---|
| [cpu-bench](https://github.com/KovantAI/rstest/tree/main/examples/cpu-bench) | 64 synthetic pure-Python tests | 15.0s | 3.7x | 7.6x | 8.6x | rstest ahead at every `-n` (2-17%) |
| sympy (`polys`, `solvers`) | 3,061 tests, pure-Python symbolic math | 83.5s | 3.4x | 6.6x | 7.2x | parity at every `-n` |
| scikit-learn (`linear_model`) | 3,941 tests, numpy, 1 BLAS thread | 31.8s | 2.9x | 4.5x | 4.6x | rstest ahead to `-n 4` (3-4%), parity from `-n 8` |
<!-- --8<-- [end:cpu-table] -->

Speedups are rstest's, vs serial pytest. 100% per-test outcome parity at
every measured point of every suite.

**What the data says:**

- **CPU-bound suites gain up to the performance-core count, then flatten.**
  Efficiency is 85-93% at `-n 4` on the two pure-Python suites and falls past
  `-n 10`: the last four workers land on efficiency cores. On a machine with
  uniform cores, expect the bend at the physical core count.
- **Against xdist it is parity, not a win**, once the suite is long enough
  for per-run overhead to vanish. On sympy the spreads overlap at every `-n`.
  The synthetic suite shows rstest ahead because it is short (15 s serial,
  1.7 s at `-n 14`), where start-up and dispatch cost is a visible share of
  the wall.
- **numpy-heavy suites scale less.** scikit-learn stops at about 4.6x: each
  worker pays the same imports and collection before it runs a test, and
  that fixed cost doesn't parallelize.

### Worker sweeps

Wall seconds, median of 5 (min-max). **Bold**: faster, with no overlap in the
spreads; otherwise the runners are at parity.

**sympy** (`sympy/polys sympy/solvers`, commit `d701e594`), pytest serial
83.5 (82.3-88.5):

| -n | rstest | speedup | xdist | speedup |
|---|---|---|---|---|
| 1 | 83.4 (83.2-86.5) | 1.00x | 82.9 (81.3-89.7) | 1.01x |
| 2 | 45.2 (44.2-47.5) | 1.85x | 44.3 (44.2-47.7) | 1.89x |
| 4 | 24.5 (24.2-25.5) | 3.41x | 24.2 (24.1-24.4) | 3.46x |
| 8 | 15.6 (15.3-15.6) | 5.37x | 15.5 (14.0-18.2) | 5.40x |
| 10 | 12.6 (11.8-13.3) | 6.63x | 12.2 (11.6-14.6) | 6.81x |
| 14 | 11.7 (11.2-12.1) | 7.16x | 11.8 (11.3-22.1) | 7.09x |

**scikit-learn** (`--pyargs sklearn.linear_model`, scikit-learn 1.9.1, numpy
2.5.3, `OMP_NUM_THREADS` and friends set to 1), pytest serial 31.8 (31.8-31.9):

| -n | rstest | speedup | xdist | speedup |
|---|---|---|---|---|
| 1 | **32.0** (32.0-32.2) | 0.99x | 32.9 (32.9-32.9) | 0.97x |
| 2 | **17.9** (17.5-18.0) | 1.78x | 18.4 (18.2-18.8) | 1.73x |
| 4 | **11.0** (10.6-11.0) | 2.88x | 11.4 (11.3-11.4) | 2.80x |
| 8 | 7.6 (7.6-8.2) | 4.17x | 7.2 (6.8-7.8) | 4.41x |
| 10 | 7.0 (6.6-7.1) | 4.52x | 7.1 (7.0-7.2) | 4.51x |
| 14 | 6.9 (6.8-7.2) | 4.62x | 6.6 (6.5-6.8) | 4.79x |

The synthetic sweep, memory and BLAS-thread tables are in
[`examples/cpu-bench`](https://github.com/KovantAI/rstest/tree/main/examples/cpu-bench#measured-result).

Reproduce:

```bash
python3 corpus/run.py --prepare-only --only sympy,scikit-learn
python3 corpus/bench.py --only sympy,scikit-learn --sweep sympy,scikit-learn \
  --sweep-workers 1,2,4,8,10,14 --xdist --repeat 5
```

Commands the bench runs (`-p recorder` and `--report-json` capture per-test
outcomes for the parity diff): `python -m pytest -p recorder -q <target>`
(serial), `python -m pytest -p recorder -q -n N <target>` (xdist, default
`--dist load`), `rstest --report-json <file> --worker-timeout 120 -n N
<target>`.

### Memory and BLAS threads

scikit-learn `linear_model`, peak memory of the whole process tree (median of 3):

| -n | rstest | xdist |
|---|---|---|
| 1 | 920 MiB | 1,088 MiB |
| 4 | 2,138 MiB | 2,475 MiB |
| 8 | 3,670 MiB | 4,110 MiB |
| 14 | 5,437 MiB | 6,082 MiB |

Both grow linearly with `-n` (rstest ≈ 731 MiB + N x 344 MiB, R² 0.994). The
intercept is large because some scikit-learn tests start their own joblib
worker pools. Setting the BLAS thread cap to 1, 2, 4 or leaving it unset made
no measurable difference at any `-n` (1 to 14) on this suite, and no test
changed outcome. The memory model and thread guidance are in
[Migrating from xdist](../guides/migrate-from-xdist.md#already-fast-cpu-bound).

## Monorepo

langchain-ai/langgraph: discovery finds all 8 Python `libs/*` packages;
the measured subset is the six that need no live database services.
Each has its own pytest config: a repo a single pytest cannot run from the
root at all. Baseline is the only native workflow: six serial pytest
invocations. The numbers below are from the corpus runner (4,284 tests,
commit `97320843`); reproduce with `python3 corpus/run.py --only
langgraph`.

| | wall | outcome parity |
|---|---|---|
| pytest, 6 serial invocations | 880.4s | (baseline) |
| rstest at the root, cold (first run) | 245.7s (3.6×) | 100% |

Only the cold run is measured. **Projection, not a measurement:** from the
cold run's per-project duration caches, a warm run is projected at 121-133s
(6.6-7.3×). That run hasn't been recorded, so it has no parity number and
isn't in the table.

On the measured (cold) run, per-project outcomes matched exactly: every
one of the 4,284 tests' setup/call/teardown agreed, including the dominant
package's service-dependent fail/error signature (those tests fail
identically under vanilla pytest, hence the non-zero exit).

Why a warm run should be faster: a warm run plans each package's worker share
from its duration cache, so the dominant package gets the workers and the rest
ride along on single workers. The cold run has no caches to plan from, so it
lands at 3.6×.

**Policy.** `checkpoint-sqlite` is a small suite and runs single-worker
(`-n 1`, the same as `-n 0`). It pulls in pytest-retry, whose worker reporter reads
`workerinput["server_port"]`. That key once had no source under rstest (no
central controller to set it) and forced this pin. It is now **resolved**: rstest starts pytest-retry's own
report server inside each worker and seeds `workerinput["server_port"]`, so
pytest-retry takes its worker branch and its `@pytest.mark.flaky` TTL test
(which lives here) runs correctly at `-n ≥ 2` too (verified; see
[parity divergences §8](parity-divergences.md#8-plugin-controller-hook-gating-rstest-side-fixed)).
Single-worker remains the natural choice for a suite this small; the numbers
below are the `-n 0` run. The plugin also sits in the shared venv, so it loads
in the other five projects too; they don't use the marker, so the corpus disables
it there (`-p no:pytest-retry`, on both the baseline and rstest runs) to keep
per-test parity exact.

## Reading the numbers

- **aiohttp is the headline** and deserves its asterisk: one file,
  `tests/test_proxy_functional.py`, holds 34 of the 4,469 tests but about
  153s of the 193s serial run (nine ~11s tests and one ~55s test). The
  bench runs xdist with its default `--dist load` (`python -m pytest -p
  recorder -q -n 8`, and aiohttp's config sets no `--dist`). That scheduler hands out consecutive chunks of the collection in
  collection order, with no duration history, and never moves a test once
  it is queued on a worker. The file's tests are adjacent in collection
  order, so most of them land in one worker's queue and run back to back
  there: in a verification run, 28 of the 34, including all ten slow ones
  (so nearly all of the file's ~153s), ran on one worker, which sets the 161s floor. With a warm duration cache,
  rstest dispatches every test with a cached duration of 1s or more first,
  longest first and one at a time, so those tests spread across workers
  (67s, close to the single ~55s test). Without the cache the first run is
  collection-ordered: 150s at `-n 8`. The speedup arrives on run two, so
  persist `.rstest_cache` in CI.
- **pandas is controller-bound under xdist.** With 193,843 tests, xdist's
  controller (one Python process handling every test report) sat at 100% CPU
  for most of the run, and the workers waited on it. rstest's orchestrator is
  Rust, so the same `-n 8` finishes in 42s against 89s. An earlier single run
  on this page had the two at parity (61s vs 63s) on an older pandas; that
  number is superseded. (The 186s serial baseline is real, not
  estimated.[^pandas])
- **django-allauth at matched `-n`**: 5.7s against 8.8s. Its corpus policy
  is `-n 4` for wall-clock rate-limit tests that can flake at high worker
  counts; that run is 8.4s.
- **Small suites don't change much.** rich saves under a second. As a
  rule of thumb: under ~10 seconds of serial runtime, expect no
  meaningful wall-time win (worker startup amortizes poorly, and `-n
  auto` deliberately caps itself low on small suites); the value there
  is `--watch`, `--changed`, and `--doctor`, not raw speed. The win grows
  with suite size and is largest for wait-heavy suites.
- **Parity is the real claim.** 100% means every test's
  setup/call/teardown outcome matched the pytest baseline exactly on the
  measured run, with each suite's real plugins active. A few tests are
  intermittently flaky *under plain pytest itself* (not rstest): those
  cases and their ~99.x% run-to-run rates are catalogued in
  [Parity divergences](parity-divergences.md).

## Methodology

Every table on this page is measured the same way (the monorepo section is an
older corpus-runner measurement, described there):

1. **Repeats.** One untimed warm-up run (fills OS file caches and
   `__pycache__`), then 5 timed runs on the primary machine (3 in the weekly
   CI bench). Tables show the **median** with the **min-max spread** in
   parentheses.
2. **Matched worker counts.** rstest and pytest-xdist are compared at the same
   `-n` in every row. A suite's recommended policy (django-allauth's `-n 4`,
   for its timing-sensitive tests) is its own labeled row, never the comparison.
3. **Cache state is stated.** rstest numbers are warm (a duration cache from
   the warm-up run) unless a row says cold (`.rstest_cache` removed before every
   run).
4. **Parity gate.** Each point's per-test outcomes (from its last timed run)
   are diffed against the serial pytest baseline's last run. Below 99.5% parity
   the bench fails instead of reporting a speed number; known divergences are
   in [Parity divergences](parity-divergences.md).
5. **Ties are ties.** A result is bold only when its spread does not overlap the
   comparison's. Otherwise the row says parity.
6. **Sleep detection.** The tooling compares each run's wall-clock and
   monotonic spans and re-runs any run the machine slept through.
7. **Recorded:** see [Environment](#environment) for what the result files
   hold and what they don't.

The tooling: [`examples/cpu-bench/measure.py`](https://github.com/KovantAI/rstest/tree/main/examples/cpu-bench)
for the synthetic suite, `corpus/bench.py` for the public suites. Both write
raw JSON next to themselves, so the tables can be re-rendered.

## What's *not* claimed

- No cross-machine generality: one machine, comparative conditions.
- No cold-start microbenchmarks. On a one-test project, spawning workers and
  collecting takes about 100 ms per [watch cycle](../guides/local-dev.md);
  suite runtime is dominated by tests, not runners.
- Parallel speedups depend on suite shape: wait-bound suites gain most;
  CPU-bound suites gain up to the performance-core count
  ([measured](#cpu-bound-suites)); suites gated by one long test gain nothing
  beyond that test (run `--doctor`; it names the floor).
  Already fast under xdist? See [what's still worth
  it](../guides/migrate-from-xdist.md#already-fast-cpu-bound).

**Parity for django-allauth and rich.** On the measured run, outcomes
matched pytest exactly. Both suites contain tests that are intermittently
flaky *under plain pytest* (rich has a lexer-guess test that flakes ~1 in 5
sequential pytest runs; django-allauth has wall-clock rate-limit windows),
so on some runs the pytest baseline and rstest can disagree (~99.8–99.9%).
Per-case detail: [Parity divergences](parity-divergences.md).

[^pandas]: Measured, not estimated. pandas' default suite on Apple
    Silicon is dominated by sub-millisecond asserts, and the collected
    count includes its thousands of environment-dependent skips. The
    baseline command is in the corpus runner (`corpus/run.py`).
