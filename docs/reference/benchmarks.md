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
| Runners | rstest 0.8.0, pytest 9.1.1, pytest-xdist 3.8.0 |
| Date | 2026-09-30; 2026-10-06 for the monorepo table, the aiohttp worksteal comparison, the pandas CPU sample and the watch-cycle cost |

Suite versions: pandas 3.0.5 (wheel), aiohttp `12ea5a58`, django-allauth
`d7d5d39a`, rich `46cebbb0`, sympy `d701e594`, scikit-learn 1.9.1 (wheel),
langgraph `97320843`.
Raw results: [`corpus/bench-results/`](https://github.com/KovantAI/rstest/tree/main/corpus/bench-results).

**What each result file records** (`environment` block): date, platform
string, logical CPU count, 1-minute load average at the start, `rstest
--version`, the rstest repo commit, every suite's pinned commit, repeat and
warm-up counts, and warm or cold cache. Per point it stores the median and
min-max of the timed walls and the parity counts. The 2026-10-06 files also
record the Python version running the bench, the xdist `--dist` mode, and,
per suite venv, the Python, pytest, pytest-xdist and rstest worker versions
(`suite_runtimes`). Recorded load at the start: 2.36 (suite table), 2.49
(aiohttp cold), 1.84 (sympy and scikit-learn sweep), 2.15 (scikit-learn BLAS
grid), 11.46 (scikit-learn memory, started right after the grid run), 5.75
and 3.77 (aiohttp worksteal and load), 24.15 (langgraph, started right after
the aiohttp run). rstest commit: `caa9bcc` (0.8.0) for the 2026-09-30 files;
`cd0b03d` (0.8.0, built with uncommitted documentation and CLI changes in the
working tree) for the 2026-10-06 files.

**Not recorded in those files:** the CPU model (taken from the
[`examples/cpu-bench`](https://github.com/KovantAI/rstest/tree/main/examples/cpu-bench)
run the same day, which records it), memory size, power state (AC for these
runs), whether the machine was kept awake by `caffeinate` (it was; the tooling
only detects and re-runs a slept-through run), other load on the machine (a
video call was running during at least the first 2026-10-06 runs), the individual timed walls
behind each median, and, for the 2026-09-30 files, the Python / pytest /
pytest-xdist versions: those come from the suites' venvs as they were that
day.

## Results

<!-- --8<-- [start:suite-table] -->
| Suite | Tests | pytest serial | xdist `-n 8` | rstest `-n 8` | Outcome parity |
|---|---|---|---|---|---|
| pandas | 193,843 | 190s | 89s | **43s** | 100% |
| aiohttp | 4,469 | 193s | 160s (73s with `--dist worksteal`) | **67s** warm (150s cold) | 99.91-99.98% (socket-leak flake, hits xdist too) |
| django-allauth | 2,050 | 26s | 8.9s | **5.8s** (8.4s at its recommended `-n 4`) | 100% |
| rich | 981 | 3.7s | 2.7s | **2.5s** | 100% |
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
| [cpu-bench](https://github.com/KovantAI/rstest/tree/main/examples/cpu-bench) | 64 synthetic pure-Python tests | 15.2s | 3.8x | 7.6x | 8.6x | parity at `-n 1`-`2`, rstest ahead from `-n 4` (4-15%) |
| sympy (`polys`, `solvers`) | 3,061 tests, pure-Python symbolic math | 80.8s | 3.2x | 6.3x | 5.7x | parity at every `-n` |
| scikit-learn (`linear_model`) | 3,941 tests, numpy, 1 BLAS thread | 31.9s | 3.0x | 4.5x | 4.6x | rstest ahead at `-n 2` (4%), parity elsewhere |
<!-- --8<-- [end:cpu-table] -->

Speedups are rstest's, vs serial pytest. rstest kept 100% per-test outcome
parity at every measured point of every suite (one xdist point, scikit-learn
at `-n 10`, measured 99.97%).

**What the data says:**

- **CPU-bound suites gain up to the performance-core count, then flatten.**
  Efficiency is 80-94% at `-n 4` on the two pure-Python suites and falls past
  `-n 10`: the last four workers land on efficiency cores. sympy got slower
  from `-n 10` to `-n 14` on both runners (12.8s to 14.1s under rstest, with a
  wide spread). On a machine with uniform cores, expect the bend at the
  physical core count.
- **Against xdist it is parity, not a win**, once the suite is long enough
  for per-run overhead to vanish. On sympy the spreads overlap at every `-n`.
  The synthetic suite shows rstest ahead from `-n 4` because it is short
  (15 s serial, 1.8 s at `-n 14`), where start-up and dispatch cost is a
  visible share of the wall.
- **numpy-heavy suites scale less.** scikit-learn stops at about 4.6x: each
  worker pays the same imports and collection before it runs a test, and
  that fixed cost doesn't parallelize.

### Worker sweeps

Wall seconds, median of 5 (min-max). **Bold**: faster, with no overlap in the
spreads; otherwise the runners are at parity.

**sympy** (`sympy/polys sympy/solvers`, commit `d701e594`), pytest serial
80.8 (80.0-82.4):

| -n | rstest | speedup | xdist | speedup |
|---|---|---|---|---|
| 1 | 84.4 (82.6-88.1) | 0.96x | 84.0 (82.0-85.3) | 0.96x |
| 2 | 47.8 (46.1-48.3) | 1.69x | 46.0 (44.9-47.7) | 1.75x |
| 4 | 25.1 (24.6-29.0) | 3.22x | 24.6 (24.4-25.4) | 3.28x |
| 8 | 16.1 (15.3-16.6) | 5.00x | 14.8 (13.8-24.4) | 5.47x |
| 10 | 12.8 (12.3-15.9) | 6.30x | 12.6 (12.2-13.0) | 6.40x |
| 14 | 14.1 (13.1-20.4) | 5.73x | 14.5 (12.9-14.9) | 5.59x |

**scikit-learn** (`--pyargs sklearn.linear_model`, scikit-learn 1.9.1, numpy
2.5.3, `OMP_NUM_THREADS` and friends set to 1), pytest serial 31.9 (31.8-33.5):

| -n | rstest | speedup | xdist | speedup |
|---|---|---|---|---|
| 1 | 36.1 (33.2-37.3) | 0.88x | 35.6 (35.2-38.0) | 0.90x |
| 2 | **19.0** (18.3-19.2) | 1.67x | 19.7 (19.6-19.8) | 1.62x |
| 4 | 10.7 (10.6-11.5) | 2.97x | 11.5 (11.4-11.6) | 2.76x |
| 8 | 7.6 (7.2-7.8) | 4.16x | 7.4 (7.1-7.8) | 4.28x |
| 10 | 7.2 (6.7-7.4) | 4.45x | 7.3 (7.3-7.5) | 4.35x |
| 14 | 6.9 (6.9-7.1) | 4.61x | 7.0 (6.8-7.2) | 4.52x |

The `-n 1` rows sit above the serial baseline on both runners; the memory run
below measured rstest at 32.1 on the same point, so read it as noise, not
per-worker overhead.

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
| 1 | 925 MiB | 1,087 MiB |
| 4 | 2,124 MiB | 2,496 MiB |
| 8 | 3,377 MiB | 4,079 MiB |
| 14 | 5,694 MiB | 6,097 MiB |

Both grow linearly with `-n` (rstest ≈ 646 MiB + N x 357 MiB, R² 0.997). The
intercept is large because some scikit-learn tests start their own joblib
worker pools. Setting the BLAS thread cap to 1, 2, 4 or leaving it unset made
no measurable difference from `-n 2` to `-n 14` on this suite. At `-n 1`,
leaving it unset was slower: 34.4s (34.1-36.0) against 31.9-32.1s with any
cap. No test changed outcome in any cell. The memory model and thread guidance are in
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
| pytest, 5 serial invocations | 838 | 190.0s (185.7-201.8) | (baseline) |
| rstest at the root, `-n auto` | 838 | **26.1s** (25.0-27.2), 7.3x | 100% |

Median of 5 timed runs after one warm-up, min-max in parentheses, rstest warm
(the [methodology](#methodology) below). The pytest wall is the sum of the
five invocations, what a developer waits through running them one after
another. The packages' venv runs CPython 3.12.12. Raw result:
[`2026-10-06-langgraph.json`](https://github.com/KovantAI/rstest/tree/main/corpus/bench-results).

`libs/sdk-py` contributes no tests to either side: its 10 test modules fail
to import in the prepared venv (`starlette` is not installed by its test
dependency group), and both runners report the same 10 collection errors.
The 838 tests are `libs/cli` (330), `libs/prebuilt` (228), `libs/checkpoint`
(172) and `libs/checkpoint-sqlite` (108). `libs/checkpoint` alone takes
about 127s serially and has 13 tests that fail under plain pytest; rstest
reports the same outcomes.

**Policy:** none. `checkpoint-sqlite` pulls in pytest-retry, whose worker
reporter reads `workerinput["server_port"]`, a key xdist's controller sets.
rstest has no central controller, so it starts pytest-retry's own report
server inside each worker and seeds `workerinput["server_port"]`; the package
runs unmodified at the full worker count (see
[parity divergences §8](parity-divergences.md#8-plugin-controller-hook-gating-rstest-side-fixed)).

## Reading the numbers

- **aiohttp is the headline** and deserves its asterisk: one file,
  `tests/test_proxy_functional.py`, holds 34 of the 4,469 tests but about
  153s of the 193s serial run (nine ~11s tests and one ~55s test). The
  bench runs xdist with its default `--dist load` (`python -m pytest -p
  recorder -q -n 8`, and aiohttp's config sets no `--dist`). That scheduler
  hands out consecutive chunks of the collection in collection order, with no duration history, and never moves a test once
  it is queued on a worker. The file's tests are adjacent in collection
  order, so most of them land in one worker's queue and run back to back
  there: in a verification run, 28 of the 34, including all ten slow ones
  (so nearly all of the file's ~153s), ran on one worker, which sets the 160s floor. With a warm duration cache,
  rstest dispatches every test with a cached duration of 1s or more first,
  longest first and one at a time, so those tests spread across workers
  (67s, close to the single ~55s test). Without the cache the first run is
  collection-ordered: 150s at `-n 8`. The speedup arrives on run two, so
  persist `.rstest_cache` in CI.
- **Against xdist `--dist worksteal`, the aiohttp gap mostly closes.**
  Worksteal lets an idle worker take queued tests from a busy one, so the
  slow file spreads out without any duration history. Measured the same day,
  same method, at `-n 8` (median of 5, min-max): xdist `--dist load` 158.9s
  (157.0-163.4), xdist `--dist worksteal` 73.0s (72.0-73.1), rstest warm
  67.0s (67.0-67.0). rstest is still ahead with no spread overlap, but by 6s
  (8%), not by 2.4x. If a suite like this already runs xdist with worksteal,
  expect a small win from switching, not the headline one. Parity in that
  run: 100% for rstest and for xdist worksteal, 99.98% for xdist load
  ([`2026-10-06-aiohttp-worksteal.json`, `2026-10-06-aiohttp-load.json`](https://github.com/KovantAI/rstest/tree/main/corpus/bench-results)).
  Reproduce: `python3 corpus/bench.py --only aiohttp --sweep aiohttp
  --sweep-workers 8 --xdist --xdist-dist worksteal --repeat 5`.
- **pandas is controller-bound under xdist.** With 193,843 tests, xdist's
  controller (one Python process that receives every test report) is the
  busiest process in the run. A CPU-sampled run (3 runs per runner at `-n 8`,
  psutil every 0.5s, recorded in
  [`2026-10-06-pandas-cpu.json`](https://github.com/KovantAI/rstest/tree/main/corpus/bench-results)):
  the controller used 74-85 CPU-seconds, a median sample of 89-100% of one
  core, at or above 90% in 47-73% of samples. Its 8 workers were on a core
  for only 36-41% of the wall. Under rstest the workers did about the same
  work (356-362 CPU-seconds against xdist's 314-343) but were on a core for
  84-85% of the wall, and the Rust driver used 11-12 CPU-seconds. Same work,
  idle workers: that is consistent with xdist's workers waiting on the
  controller, though sampling CPU alone cannot attribute every idle second to
  it. The same `-n 8` finishes in 43s against 89s. (A video call was running
  during the sampled runs, so their walls, 98-118s for xdist and 52-54s for
  rstest, sit above the medians in the table; read them for the CPU split.)
  An earlier single run on this page had the two at parity (rstest 63s,
  xdist 61s) on an older pandas; that number is superseded. (The 190s serial
  baseline is real, not estimated.[^pandas])
- **django-allauth at matched `-n`**: 5.8s against 8.9s. Its corpus policy
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

Every table on this page is measured the same way:

1. **Repeats.** One untimed warm-up run (fills OS file caches and
   `__pycache__`), then 5 timed runs on the primary machine (3 in the weekly
   CI bench). Tables show the **median** with the **min-max spread** in
   parentheses. One exception: the serial pytest baselines in the 2026-09-30
   files were timed without the warm-up run (the bench's first pass skipped
   it); the 2026-10-06 files include it.
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
  [watch-mode](../guides/watch-mode.md#per-cycle-cost) rerun takes 162 ms at
  `-n 0` and 202 ms at `-n 2` from the detected save to the result (median of
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
