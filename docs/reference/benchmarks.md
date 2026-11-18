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
| Runners | rstest 0.12.0, pytest 9.2.0, pytest-xdist 3.8.0 |
| Date | 2026-11-18 |

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
3.54 (suite table), 3.89 (aiohttp worksteal), 2.38 (aiohttp cold), 1.56
(sympy and scikit-learn sweep), 17.09 (scikit-learn BLAS grid), 13.28
(langgraph), 2.29 (scikit-learn memory). The grid and langgraph runs each
started right after the previous run's 14-worker points, so their start load
is mostly that run winding down. rstest commit: `4d6e2dd` (the 0.12.0
release commit, clean tree) for every file. The Python running the bench is
3.14.6, except 3.13.13 for the memory run (it needs psutil); the suites
themselves run in their own venvs (CPython 3.13.13, langgraph 3.12.12). The
suites are pinned to the same versions, and pytest and pytest-xdist are the
same, as in the 2026-11-10 run (rstest 0.11.0, raw files in the same
folder), so the change between the two is rstest's.

**Not recorded in those files:** the CPU model (taken from the
[`examples/cpu-bench`](https://github.com/KovantAI/rstest/tree/main/examples/cpu-bench)
run of 2026-11-10, which records it), memory size, power state (AC for these
runs, see below), whether the machine was kept awake by `caffeinate` (it was;
the tooling only detects and re-runs a slept-through run), other load on the
machine beyond the start load above, and the individual timed walls behind
each median.

During the sympy and scikit-learn sweep the machine was unplugged and its
lid closed, and it slept for about 30 minutes. The tooling discarded the two
runs the sleep interrupted and ran them again; the first 34 seconds of one
re-run (a sympy xdist `-n 2` run) were on battery before AC returned. Every
other run was on AC.

**Carried over from rstest 0.11.0 (2026-11-10), not re-measured for 0.12.0:**
the [cpu-bench](#cpu-bound-suites) row, the pandas CPU sample under
[Reading the numbers](#reading-the-numbers)
([`2026-11-10-pandas-cpu.json`](https://github.com/KovantAI/rstest/tree/main/corpus/bench-results)),
and the watch-mode cycle cost under [What's not claimed](#whats-not-claimed)
([`2026-11-10-watch-cycle.json`](https://github.com/KovantAI/rstest/tree/main/corpus/bench-results)).
Each is labeled where it appears.

## Results

<!-- --8<-- [start:suite-table] -->
| Suite | Tests | pytest serial | xdist `-n 8` | rstest `-n 8` | Outcome parity |
|---|---|---|---|---|---|
| pandas | 193,843 | 197s | 113s | **33s** | 100% |
| aiohttp | 4,469 | 193s | 160s (73s with `--dist worksteal`) | **67s** warm (150s cold) | 100% warm, 99.98% cold (socket-leak flake) |
| django-allauth | 2,050 | 25s | 11.4s | **5.4s** (8.5s at its recommended `-n 4`) | 100% |
| rich | 981 | 3.6s | 2.7s | **2.5s** | 100% |
<!-- --8<-- [end:suite-table] -->

Median of 5 timed runs after one warm-up, same `-n` for both runners. Bold:
faster, with no overlap in the min-max spreads (they are in
[`corpus/bench-results/`](https://github.com/KovantAI/rstest/tree/main/corpus/bench-results)).
Warm means rstest had its duration cache from a prior run; cold means it was
deleted before every run.

aiohttp's parity gap: some of its tests leak sockets, which are
garbage-collected inside whichever test runs next, and that test fails under
the suite's warnings-as-errors config. Any parallel runner moves that point:
rstest measured 100% in the table run and 99.98% (one test) in the worksteal
and cold runs; xdist at `-n 8` measured 99.96% in an earlier run and 100% in
both of this one's; see
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
| sympy (`polys`, `solvers`) | 3,061 tests, pure-Python symbolic math | 80.3s | 3.3x | 6.4x | 6.9x | xdist ahead at `-n 8` (8%), parity elsewhere |
| scikit-learn (`linear_model`) | 3,941 tests, numpy, 1 BLAS thread | 31.5s | 2.9x | 4.2x | 4.4x | rstest ahead at `-n 1` (3%), xdist ahead at `-n 10` and `-n 14` (11%), parity elsewhere |
<!-- --8<-- [end:cpu-table] -->

Speedups are rstest's, vs serial pytest. The cpu-bench row is from
2026-11-10 (rstest 0.11.0); sympy and scikit-learn are re-measured for
0.12.0. rstest kept 100% per-test outcome parity at every point of all three
suites, and so did xdist on sympy and scikit-learn.

**What the data says:**

- **CPU-bound suites gain up to the performance-core count, then flatten.**
  Efficiency is 82-92% at `-n 4` on the two pure-Python suites and falls past
  `-n 10`: the last four workers land on efficiency cores. sympy went from
  12.6s at `-n 10` to 11.7s at `-n 14` under rstest, with the spreads
  overlapping. On a machine with uniform cores, expect the bend at the
  physical core count.
- **Against xdist it is parity or a small xdist lead**, once the suite is long
  enough for per-run overhead to vanish. On sympy and scikit-learn xdist was
  ahead at some `-n` with no spread overlap, by up to 11% (scikit-learn at
  `-n 10` and `-n 14`), and the spreads overlapped elsewhere. Against the
  2026-11-10 run (rstest 0.11.0, same pytest and xdist), both runners' walls
  are unchanged within the spreads (rstest at `-n 8`: sympy 16.0s to 15.5s,
  scikit-learn 7.7s to 8.4s). The synthetic suite still shows rstest ahead
  at high `-n` because it is short (15 s serial, 1.8 s at `-n 14`), where
  start-up and dispatch cost is a visible share of the wall.
- **numpy-heavy suites scale less.** scikit-learn stops at about 4.4x: each
  worker pays the same imports and collection before it runs a test, and
  that fixed cost doesn't parallelize.

### Worker sweeps

Wall seconds, median of 5 (min-max). **Bold**: faster, with no overlap in the
spreads; otherwise the runners are at parity.

**sympy** (`sympy/polys sympy/solvers`, commit `d701e594`), pytest serial
80.3 (79.5-83.0):

| -n | rstest | speedup | xdist | speedup |
|---|---|---|---|---|
| 1 | 83.3 (82.0-85.2) | 0.96x | 83.1 (83.1-89.7) | 0.97x |
| 2 | 45.5 (44.6-48.6) | 1.77x | 44.0 (43.0-45.8) | 1.82x |
| 4 | 24.6 (24.3-25.2) | 3.27x | 24.3 (24.0-24.6) | 3.30x |
| 8 | 15.5 (15.0-17.2) | 5.19x | **14.2** (13.7-14.6) | 5.64x |
| 10 | 12.6 (12.3-22.2) | 6.38x | 12.0 (11.6-24.4) | 6.69x |
| 14 | 11.7 (11.3-12.6) | 6.89x | 11.5 (11.1-12.0) | 7.01x |

At `-n 10` each runner had one slow run (22.2s and 24.4s); the medians are
unaffected.

**scikit-learn** (`--pyargs sklearn.linear_model`, scikit-learn 1.9.1, numpy
2.5.3, `OMP_NUM_THREADS` and friends set to 1), pytest serial 31.5 (31.3-32.8):

| -n | rstest | speedup | xdist | speedup |
|---|---|---|---|---|
| 1 | **32.1** (31.8-32.2) | 0.98x | 33.0 (32.5-33.5) | 0.95x |
| 2 | 18.5 (18.2-18.6) | 1.70x | 18.7 (18.4-18.8) | 1.68x |
| 4 | 11.0 (10.6-11.2) | 2.86x | 10.6 (10.5-10.9) | 2.98x |
| 8 | 8.4 (7.9-9.0) | 3.75x | 7.1 (6.8-8.1) | 4.40x |
| 10 | 7.4 (6.9-8.1) | 4.23x | **6.6** (6.6-6.7) | 4.74x |
| 14 | 7.1 (7.0-7.3) | 4.43x | **6.3** (6.3-6.6) | 4.96x |

The `-n 1` rows sit 2-4% above the serial baseline under rstest, with the
spreads overlapping; a single worker costs about what one pytest process
does.

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
| 1 | 810 MiB | 920 MiB |
| 4 | 1,669 MiB | 1,823 MiB |
| 8 | 2,710 MiB | 2,832 MiB |
| 14 | 3,836 MiB | 3,903 MiB |

Both grow linearly with `-n` (rstest ≈ 693 MiB + N x 232 MiB, R² 0.990;
xdist ≈ 836 MiB + N x 227 MiB, R² 0.986). Every point is within 2% of the
2026-11-10 run (rstest 0.11.0) except xdist at `-n 4` (2,092 MiB then). The
intercept is large because some scikit-learn tests start their own joblib
worker pools. Setting the BLAS thread cap to 1, 2, 4 or leaving it unset made
no measurable difference at `-n` 1, 2, 4 and 14 on this suite: those rows'
cells overlap (`-n 1`: 32.4-33.1s, `-n 14`: 6.9-7.1s). At `-n 10`, caps of 1
and 2 (7.3s) ran ahead of 4 (8.0s) and unset (7.9s) with no overlap; that
run started at a load of 17, right after the sweep's 14-worker points. No
test changed outcome in any cell. The memory model and thread guidance are in
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
| pytest, 5 serial invocations | 838 | 192.0s (186.3-200.1) | (baseline) |
| rstest at the root, `-n auto` | 838 | **26.1s** (25.2-26.6), 7.4x | 100% |

Median of 5 timed runs after one warm-up, min-max in parentheses, rstest warm
(the [methodology](#methodology) below). The pytest wall is the sum of the
five invocations, what a developer waits through running them one after
another. The packages' venv runs CPython 3.12.12. Raw result:
[`2026-11-18-langgraph.json`](https://github.com/KovantAI/rstest/tree/main/corpus/bench-results).

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
  same method, at `-n 8` (median of 5, min-max): xdist `--dist load` 160.0s
  (160.0-160.0), xdist `--dist worksteal` 73.0s (73.0-73.0), rstest warm
  67.0s (67.0-67.0). rstest is still ahead with no spread overlap, but by 6s
  (8%), not by 2.4x. If a suite like this already runs xdist with worksteal,
  expect a small win from switching, not the headline one. Parity in those
  runs: 100% for xdist worksteal and xdist load, 100% for rstest in the table
  run and 99.98% in the worksteal run (one socket-leak test)
  ([`2026-11-18-aiohttp-worksteal.json`, `2026-11-18-pandas-aiohttp-allauth-rich.json`](https://github.com/KovantAI/rstest/tree/main/corpus/bench-results)).
  Reproduce: `python3 corpus/bench.py --only aiohttp --sweep aiohttp
  --sweep-workers 8 --xdist --xdist-dist worksteal --repeat 5`.
- **pandas is controller-bound under xdist.** With 193,843 tests, xdist's
  controller (one Python process that receives every test report) is the
  busiest process in the run. A CPU-sampled run (rstest 0.11.0, 2026-11-10,
  not re-measured for 0.12.0; 3 runs per runner at `-n 8`
  after one untimed warm-up, psutil every 0.5s, recorded in
  [`2026-11-10-pandas-cpu.json`](https://github.com/KovantAI/rstest/tree/main/corpus/bench-results))
  shows it directly. Under xdist the 8 workers ran at 94-97% of a core and
  all exited 52-54s into the run, about rstest's whole wall. The controller
  then ran alone at 100% of one core until 110-112s, working through the
  reports the workers had already sent: it used 86-87 CPU-seconds in total.
  Under rstest the workers did about the same work (366-368 CPU-seconds
  against xdist's 341-345), were on a core for 86-87% of the wall, and the
  run ended at 53s; the Rust driver used 12 CPU-seconds. The rstest figures
  are 0.11.0's (0.12.0 runs pandas in 33s, below); the xdist side has not
  changed (same pytest and pytest-xdist, 113s in the table).
- **pandas got faster under rstest 0.12.0.** At `-n 8` it went from 60s to
  33s against the 2026-11-10 run (rstest 0.11.0), with the same pandas
  3.0.5, pytest and pytest-xdist; at `-n auto` (14 workers) it runs in 27.5s
  (26.7-27.9). The corpus runs pandas as `--pyargs pandas`, and 0.12.0
  extends lazy collection to `--pyargs` targets and has the automatic choice
  weigh the time full collection costs, so each worker no longer collects
  all 193,843 tests (see
  [Lazy collection](../concepts/lazy-collection.md#auto-default)). The serial
  and xdist walls moved less (220s to 197s, 123s to 113s) with nothing
  changed on their side; that is the run-to-run difference between the two
  days. rstest's lead over xdist is now about 3.5x. (The 197s serial
  baseline is real, not estimated.[^pandas])
- **django-allauth at matched `-n`**: 5.4s against 11.4s. Its corpus policy
  is `-n 4` for wall-clock rate-limit tests that can flake at high worker
  counts; that run is 8.5s.
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
  10), on top of the 300 ms save debounce. Measured with rstest 0.11.0 on
  2026-11-10
  ([`2026-11-10-watch-cycle.json`](https://github.com/KovantAI/rstest/tree/main/corpus/bench-results))
  and not re-measured for 0.12.0, whose `--watch` forks workers from a warm
  template by default. Suite runtime is dominated by tests, not runners.
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
