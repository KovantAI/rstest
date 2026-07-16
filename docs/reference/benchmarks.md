# Benchmarks

Numbers from rstest's compatibility battery: four real open-source suites,
run end-to-end under pytest, pytest-xdist, and rstest, with per-test
outcome diffing against the pytest baseline.

## Environment

Apple Silicon (M-series), CPython 3.13, pytest-xdist 3.8. Runs were done
against both vendored-pytest versions (9.0.3 and the current 9.1.1); the
figures below are stable across the two. Background load was present during
measurement (numbers are comparative, not lab-grade). Wall times are single
runs, warm caches noted.

## Results

<!-- --8<-- [start:suite-table] -->
| Suite | Tests | pytest serial | xdist | rstest | Outcome parity |
|---|---|---|---|---|---|
| pandas | 193,627 | 182s | 61s (`-n 8`) | 63s (`-n 8`, parity) | 100% |
| aiohttp | 4,469 | 197s | 160s (`-n 8`) | **68s** (`-n 8`, warm cache; 151s cold) | 100% |
| django-allauth | 2,050 | 22s | 8s (`-n 8`) | **8s** (`-n 4`) | 100%[^parity] |
| rich | 981 | 3.4s | 2.8s | **2.5s** (`-n 4`) | 100%[^parity] |
<!-- --8<-- [end:suite-table] -->

!!! note "Numbers single-sourced"
    This table is the canonical source for the suite numbers. Other pages
    (the [home page](../index.md)) embed it via snippet, so the figures only
    ever live here.

## Monorepo

langchain-ai/langgraph: discovery finds all 8 Python `libs/*` packages;
the measured subset is the six that need no live database services.
Each has its own pytest config — a repo pytest cannot run from the
root at all. Baseline is the only native workflow: six serial pytest
invocations. The numbers below are from the corpus runner (4,284 tests,
commit `97320843`); reproduce with `python3 corpus/run.py --only
langgraph`.

| | wall | outcome parity |
|---|---|---|
| pytest, 6 serial invocations | 880.4s | — (baseline) |
| rstest at the root, cold (first run) | 245.7s (3.6×) | 100% (measured) |
| rstest at the root, warm | **121–133s** (6.6–7.3×) | — (projected) |

The cold run is measured; the **warm figure is a projection**, not a
measured run — the cold run's per-project duration caches predict where the
planner lands once warm, but that run hasn't been recorded here, so it
carries no parity number.

On the measured (cold) run, per-lib outcomes matched to the digit — every
one of the 4,284 tests' setup/call/teardown agreed, including the dominant
package's service-dependent fail/error signature (those tests fail
identically under vanilla pytest, hence the non-zero exit). The warm run
plans each package's worker share from its duration cache, so the dominant
package gets the workers and the rest ride along on single workers.

Two effects compound in the projected warm speedup: parallelism inside the
dominant package and concurrency across packages. The first run is
cold — shares are planned from per-project duration caches that don't
exist yet, so it lands at 3.6×; a warm run, planned from those
caches, is projected to reach 6.6–7.3× as the planner self-corrects toward
the next bottleneck.

**Policy.** `checkpoint-sqlite` runs at `-n 0`: it pulls in pytest-retry,
whose worker reporter reads xdist's controller-injected
`workerinput["server_port"]` — a key rstest does not provide (see
[Known gaps](../concepts/compatibility.md#known-gaps)). Single-worker
mode takes the plugin's non-xdist path and keeps its `flaky` marker
registered (the suite's one `@pytest.mark.flaky` TTL test lives here).
The plugin also sits in the shared venv, so it loads in the other five
libs too; they don't use the marker, so the corpus disables it there
(`-p no:pytest-retry`, on both the baseline and rstest runs) to keep
per-test parity exact.

## Reading the numbers honestly

- **aiohttp is the headline** and deserves its asterisk: the suite is
  dominated by one file of 10-second-timeout tests. xdist's file-affinity
  scheduling leaves that file on one worker (160s floor); rstest's
  test-granular dispatch plus duration-cache scheduling splits it (68s).
  The first, cold-cache run is 151s — the speedup arrives on run two.
- **pandas shows parity, not victory**: both runners pay the same
  per-worker collection cost on a 193k-test suite; rstest's wins there are
  startup-path and scheduling refinements, within noise of xdist. (The 182s
  serial baseline is real, not estimated.[^pandas])
- **Small suites don't change much.** rich saves under a second. As a
  rule of thumb: under ~10 seconds of serial runtime, expect no
  meaningful wall-time win (worker startup amortizes poorly, and `-n
  auto` deliberately caps itself low on small suites) — the value there
  is `--watch`, `--changed`, and `--doctor`, not raw speed. The win grows
  with suite size and is largest for wait-heavy suites.
- **Parity is the real claim.** 100% means every test's
  setup/call/teardown outcome matched the pytest baseline exactly on the
  measured run, with each suite's real plugins active. A few tests are
  intermittently flaky *under plain pytest itself* (not rstest) — those
  cases and their ~99.x% run-to-run rates are catalogued in
  [Parity divergences](parity-divergences.md).

## What's *not* claimed

- No cross-machine generality: one machine, comparative conditions.
- No cold-start microbenchmarks: rstest's process startup is milliseconds,
  but suite runtime is dominated by tests, not runners.
- Parallel speedups depend on suite shape: wait-bound suites gain most;
  CPU-bound suites gain up to core count; suites gated by one long test
  gain nothing beyond that test (run `--doctor`; it names the floor).

[^parity]: On the measured run, outcomes matched pytest exactly. Both
    suites contain tests that are intermittently flaky *under plain pytest*
    (rich has a lexer-guess test that flakes ~1 in 5 sequential pytest runs;
    django-allauth has wall-clock rate-limit windows), so on some runs the
    pytest baseline and rstest can disagree (~99.8–99.9%). Per-case detail:
    [Parity divergences](parity-divergences.md).

[^pandas]: Yes, really — measured, not estimated: pandas' default
    suite on Apple Silicon is dominated by sub-millisecond asserts, and
    the collected count includes its thousands of environment-dependent
    skips. The same pinned-pytest baseline command is in the corpus
    runner; reproduce it before doubting it.
