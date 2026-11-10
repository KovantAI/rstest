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
| Python | CPython 3.13.13 (langgraph: CPython 3.12.12) |
| Runners | rstest 0.11.0, pytest 9.2.0, pytest-xdist 3.8.0 |
| Date | 2026-11-10 |

Suite versions: pandas 3.0.5 (wheel), aiohttp `12ea5a58`, django-allauth
`d7d5d39a`, rich `46cebbb0`, sympy `d701e594`, scikit-learn 1.9.1 (wheel),
langgraph `97320843`.
Raw results: [`corpus/bench-results/`](https://github.com/KovantAI/rstest/tree/main/corpus/bench-results).

**What each result file records** (`environment` block): date, platform
string, logical CPU count, 1-minute load average at the start, `rstest
--version`, the rstest repo commit, every suite's pinned commit, repeat and
warm-up counts, warm or cold cache, the Python version running the bench, the
xdist `--dist` mode, and, per suite venv, the Python, pytest, pytest-xdist and
rstest worker versions (`suite_runtimes`). Per point it stores the median and
min-max of the timed walls and the parity counts. Recorded load at the start:
3.06 (suite table), 6.31 (aiohttp worksteal), 3.76 (aiohttp cold), 9.44
(sympy and scikit-learn sweep), 9.68 (scikit-learn BLAS grid), 15.53
(langgraph), 3.35 (scikit-learn memory). The sweep, grid and langgraph runs each started right
after the previous run's 14-worker points, so their start load is mostly
that run winding down. rstest commit: `6a8c9e4` (the `v0.11.0` tag, clean
tree) for every file. The suites are pinned to the same versions as earlier
runs (2026-09-30 and 2026-10-06, rstest 0.8.0, raw files in the same
folder); the change on the pytest side is pytest 9.1.1 to 9.2.0.

**Not recorded in those files:** the CPU model (taken from the
[`examples/cpu-bench`](https://github.com/KovantAI/rstest/tree/main/examples/cpu-bench)
run the same day, which records it), memory size, power state (AC for these
runs), whether the machine was kept awake by `caffeinate` (it was; the tooling
only detects and re-runs a slept-through run), other load on the machine
beyond the start load above, and the individual timed walls behind each
median.

## Results

<!-- --8<-- [start:suite-table] -->
| Suite | Tests | pytest serial | xdist `-n 8` | rstest `-n 8` | Outcome parity |
|---|---|---|---|---|---|
| pandas | 193,843 | 220s | 123s | **60s** | 100% |
| aiohttp | 4,469 | 196s | 161s (73s with `--dist worksteal`) | **67s** warm (150s cold) | 100% warm, 99.91-99.98% cold (socket-leak flake) |
| django-allauth | 2,050 | 28s | 9.3s | **6.3s** (9.1s at its recommended `-n 4`) | 100% |
| rich | 981 | 3.8s | 2.8s | **2.6s** | 100% |
<!-- --8<-- [end:suite-table] -->

Median of 5 timed runs after one warm-up, same `-n` for both runners. Bold:
faster, with no overlap in the min-max spreads (they are in
[`corpus/bench-results/`](https://github.com/KovantAI/rstest/tree/main/corpus/bench-results)).
Warm means rstest had its duration cache from a prior run; cold means it was
deleted before every run.

aiohttp's parity gap: some of its tests leak sockets, which are
garbage-collected inside whichever test runs next, and that test fails under
the suite's warnings-as-errors config. Any parallel runner moves that point
(xdist at `-n 8` measured 99.96% in an earlier run; 100% in this one); see
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
| [cpu-bench](https://github.com/KovantAI/rstest/tree/main/examples/cpu-bench) | 64 synthetic pure-Python tests | 15.2s | 3.7x | 7.4x | 8.4x | rstest ahead at `-n 2`, `10` and `14` (1-9%), parity at `-n 1`, `4` and `8` |
| sympy (`polys`, `solvers`) | 3,061 tests, pure-Python symbolic math | 82.7s | 3.3x | 6.4x | 6.5x | xdist ahead at `-n 4` (3%) and `-n 8` (11%), parity elsewhere |
| scikit-learn (`linear_model`) | 3,941 tests, numpy, 1 BLAS thread | 31.5s | 2.8x | 4.3x | 4.5x | rstest ahead at `-n 1` (2%), xdist ahead at `-n 8` and `-n 14` (10%), parity elsewhere |
<!-- --8<-- [end:cpu-table] -->

Speedups are rstest's, vs serial pytest. rstest kept 100% per-test outcome
parity at every point of cpu-bench and sympy. On scikit-learn both runners
measured 99.97% at every `-n` of the sweep run (one test differing from that
run's serial baseline); the memory and grid runs the same day measured 100%.

**What the data says:**

- **CPU-bound suites gain up to the performance-core count, then flatten.**
  Efficiency is 83-92% at `-n 4` on the two pure-Python suites and falls past
  `-n 10`: the last four workers land on efficiency cores. sympy went from
  13.0s at `-n 10` to 12.7s at `-n 14` under rstest, inside the spread. On a
  machine with uniform cores, expect the bend at the physical core count.
- **Against xdist it is parity or a small xdist lead**, once the suite is long
  enough for per-run overhead to vanish. On sympy and scikit-learn xdist was
  ahead at some `-n` with no spread overlap, by up to 11% (sympy at `-n 8`),
  and the spreads overlapped elsewhere. Against the 2026-09-30 run (rstest
  0.8.0, pytest 9.1.1), rstest's walls are unchanged within the
  spreads (sympy `-n 8` 16.1s to 16.0s, scikit-learn 7.6s to 7.7s) while
  xdist's dropped (14.8s to 14.3s, 7.4s to 6.9s) with the move to pytest
  9.2.0. The synthetic suite still shows rstest ahead at high `-n` because it
  is short (15 s serial, 1.8 s at `-n 14`), where start-up and dispatch cost
  is a visible share of the wall.
- **numpy-heavy suites scale less.** scikit-learn stops at about 4.5x: each
  worker pays the same imports and collection before it runs a test, and
  that fixed cost doesn't parallelize.

### Worker sweeps

Wall seconds, median of 5 (min-max). **Bold**: faster, with no overlap in the
spreads; otherwise the runners are at parity.

**sympy** (`sympy/polys sympy/solvers`, commit `d701e594`), pytest serial
82.7 (81.6-89.7):

| -n | rstest | speedup | xdist | speedup |
|---|---|---|---|---|
| 1 | 83.0 (81.8-91.5) | 1.00x | 86.1 (84.8-88.6) | 0.96x |
| 2 | 45.6 (44.7-46.3) | 1.81x | 44.9 (44.2-45.5) | 1.84x |
| 4 | 24.9 (24.7-25.0) | 3.32x | **24.2** (23.9-24.5) | 3.42x |
| 8 | 16.0 (15.6-18.6) | 5.18x | **14.3** (13.8-14.6) | 5.78x |
| 10 | 13.0 (12.6-19.2) | 6.37x | 12.1 (11.7-12.7) | 6.83x |
| 14 | 12.7 (12.2-17.4) | 6.49x | 11.6 (11.4-18.4) | 7.13x |

**scikit-learn** (`--pyargs sklearn.linear_model`, scikit-learn 1.9.1, numpy
2.5.3, `OMP_NUM_THREADS` and friends set to 1), pytest serial 31.5 (31.4-31.6):

| -n | rstest | speedup | xdist | speedup |
|---|---|---|---|---|
| 1 | **31.8** (31.7-32.0) | 0.99x | 32.6 (32.4-32.6) | 0.97x |
| 2 | 17.7 (17.6-18.2) | 1.78x | 18.0 (17.8-18.5) | 1.75x |
| 4 | 11.1 (11.1-11.2) | 2.83x | 11.2 (11.0-11.3) | 2.81x |
| 8 | 7.7 (7.6-7.9) | 4.09x | **6.9** (6.4-7.4) | 4.57x |
| 10 | 7.2 (6.7-7.3) | 4.34x | 6.7 (6.2-6.8) | 4.70x |
| 14 | 7.0 (7.0-7.0) | 4.50x | **6.3** (5.9-6.4) | 5.00x |

The `-n 1` rows sit within 1% of the serial baseline under rstest; a single
worker costs about what one pytest process does.

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
`--dist load`; `bench.py --xdist-dist worksteal` appends `--dist worksteal`),
`rstest --report-json <file> --worker-timeout 120 -n N <target>`.

### Memory and BLAS threads

scikit-learn `linear_model`, peak memory of the whole process tree (median of 3):

| -n | rstest | xdist |
|---|---|---|
| 1 | 817 MiB | 924 MiB |
| 4 | 1,698 MiB | 2,092 MiB |
| 8 | 2,708 MiB | 2,819 MiB |
| 14 | 3,865 MiB | 3,888 MiB |

Both grow linearly with `-n` (rstest ≈ 702 MiB + N x 233 MiB, R² 0.991;
xdist ≈ 960 MiB + N x 218 MiB, R² 0.966). Both runners peak well below the
2026-09-30 run (rstest 5,694 MiB, xdist 6,097 MiB at `-n 14`) on the same
scikit-learn and numpy versions. The
intercept is large because some scikit-learn tests start their own joblib
worker pools. Setting the BLAS thread cap to 1, 2, 4 or leaving it unset made
no measurable difference at any `-n` from 1 to 14 on this suite: every
row's cells overlap (`-n 1`: 31.9-32.0s, `-n 14`: 6.9-7.1s). No test changed
outcome in any cell. The memory model and thread guidance are in
Parallel safety: [memory per worker](../guides/parallel-safety.md#memory-per-worker)
and [BLAS threads and speed](../guides/parallel-safety.md#blas-threads-and-speed).

## Monorepo

langchain-ai/langgraph: discovery finds all 8 Python `libs/*` packages. Each
has its own pytest config, so a single pytest cannot run from the root at
all, and the only native workflow is one pytest invocation per package. The
measured subset is the five packages that need no live services:
`libs/checkpoint`, `libs/checkpoint-sqlite`, `libs/cli`, `libs/prebuilt` and
`libs/sdk-py`. The two postgres-backed checkpoint stores need a running
database under any runner, and `libs/langgraph` itself is left out because
its live-service tests hang the plain-pytest baseline in a service-less
environment (see `corpus/suites.toml`). Reproduce with:

```bash
python3 corpus/run.py --prepare-only --only langgraph
python3 corpus/bench.py --only langgraph --sweep '' --repeat 5
```

| | tests | wall | outcome parity |
|---|---|---|---|
| pytest, 5 serial invocations | 838 | 191.3s (186.6-197.5) | (baseline) |
| rstest at the root, `-n auto` | 838 | **25.6s** (24.5-31.7), 7.5x | 100% |

Median of 5 timed runs after one warm-up, min-max in parentheses, rstest warm
(the [methodology](#methodology) below). The pytest wall is the sum of the
five invocations, what a developer waits through running them one after
another. The packages' venv runs CPython 3.12.12. Raw result:
[`2026-11-10-langgraph.json`](https://github.com/KovantAI/rstest/tree/main/corpus/bench-results).

`libs/sdk-py` contributes no tests to either side: its 10 test modules fail
to import in the prepared venv (`starlette` is not installed by its test
dependency group), and both runners report the same 10 collection errors.
The 838 tests are `libs/cli` (330), `libs/prebuilt` (228), `libs/checkpoint`
(172) and `libs/checkpoint-sqlite` (108). `libs/checkpoint` alone takes
about 127s serially and has 13 tests that fail under plain pytest; rstest
reports the same outcomes.

**Policy:** none. `checkpoint-sqlite` pulls in pytest-retry, which expects
an xdist controller; rstest gives each worker its own report server, so the
package runs unmodified at the full worker count (see
[xdist hooks](../concepts/xdist-hooks.md#numprocesses-visibility)).

## Reading the numbers

- **aiohttp is the headline** and deserves its asterisk: one file,
  `tests/test_proxy_functional.py`, holds 34 of the 4,469 tests but about
  153s of the 196s serial run (nine ~11s tests and one ~55s test). The
  bench runs xdist with its default `--dist load` (`python -m pytest -p
  recorder -q -n 8`, and aiohttp's config sets no `--dist`). That scheduler
  hands out consecutive chunks of the collection in collection order, with no duration history, and never moves a test once
  it is queued on a worker. The file's tests are adjacent in collection
  order, so most of them land in one worker's queue and run back to back
  there: in a verification run, 28 of the 34, including all ten slow ones
  (so nearly all of the file's ~153s), ran on one worker, which sets the 161s floor. With a warm duration cache,
  rstest dispatches every test with a cached duration of 1s or more first,
  longest first and one at a time, so those tests spread across workers
  (67s, close to the single ~55s test). Without the cache the first run is
  collection-ordered: 150s at `-n 8`. The speedup arrives on run two, so
  persist `.rstest_cache` in CI.
- **Against xdist `--dist worksteal`, the aiohttp gap mostly closes.**
  Worksteal lets an idle worker take queued tests from a busy one, so the
  slow file spreads out without any duration history. Measured the same day,
  same method, at `-n 8` (median of 5, min-max): xdist `--dist load` 161.0s
  (160.9-161.0), xdist `--dist worksteal` 73.0s (72.9-73.1), rstest warm
  67.0s (67.0-67.0). rstest is still ahead with no spread overlap, but by 6s
  (8%), not by 2.4x. If a suite like this already runs xdist with worksteal,
  expect a small win from switching, not the headline one. Parity in those
  runs: 100% for rstest, xdist worksteal and xdist load
  ([`2026-11-10-aiohttp-worksteal.json`, `2026-11-10-pandas-aiohttp-allauth-rich.json`](https://github.com/KovantAI/rstest/tree/main/corpus/bench-results)).
  Reproduce: `python3 corpus/bench.py --only aiohttp --sweep aiohttp
  --sweep-workers 8 --xdist --xdist-dist worksteal --repeat 5`.
- **pandas is controller-bound under xdist.** With 193,843 tests, xdist's
  controller (one Python process that receives every test report) is the
  busiest process in the run. A CPU-sampled run (3 runs per runner at `-n 8`
  after one untimed warm-up, psutil every 0.5s, recorded in
  [`2026-11-10-pandas-cpu.json`](https://github.com/KovantAI/rstest/tree/main/corpus/bench-results))
  shows it directly. Under xdist the 8 workers ran at 94-97% of a core and
  all exited 52-54s into the run, about rstest's whole wall. The controller
  then ran alone at 100% of one core until 110-112s, working through the
  reports the workers had already sent: it used 86-87 CPU-seconds in total.
  Under rstest the workers did about the same work (366-368 CPU-seconds
  against xdist's 341-345), were on a core for 86-87% of the wall, and the
  run ended at 53s; the Rust driver used 12 CPU-seconds. The table's `-n 8`
  medians (60s against 123s) come from the bench run earlier the same day
  and sit above these sampled walls.
- **The pytest 9.2.0 move cost pandas on both sides.** The suite is pinned
  to the same pandas 3.0.5 as before, yet serial pytest went from 190s to
  220s, xdist from 89s to 123s and rstest from 43s to 60s against the
  2026-09-30 run (rstest 0.8.0, pytest 9.1.1). rstest's lead over xdist held at
  about 2x. (The 220s serial baseline is real, not estimated.[^pandas])
- **django-allauth at matched `-n`**: 6.3s against 9.3s. Its corpus policy
  is `-n 4` for wall-clock rate-limit tests that can flake at high worker
  counts; that run is 9.1s.
- **Small suites don't change much.** rich saves about a second. As a
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

Every table on this page is measured the same way:

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
- No cold-start microbenchmarks against other runners. The one fixed cost
  measured on its own: on a one-test project, a
  [watch-mode](../guides/watch-mode.md#per-cycle-cost) rerun takes 139 ms at
  `-n 0` and 163 ms at `-n 2` from the detected save to the result (median of
  10), on top of the 300 ms save debounce. Suite runtime is dominated by
  tests, not runners.
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
so on some runs the pytest baseline and rstest can disagree (~99.8-99.9%).
Per-case detail: [Parity divergences](parity-divergences.md).

[^pandas]: Measured, not estimated. pandas' default suite on Apple
    Silicon is dominated by sub-millisecond asserts, and the collected
    count includes its thousands of environment-dependent skips. The
    baseline command is in the corpus runner (`corpus/run.py`).
