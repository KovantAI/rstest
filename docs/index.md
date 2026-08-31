# rstest

A fast, pytest-compatible test runner. Rust orchestration, your tests
unchanged: same plugins, same fixtures, same outcomes — parallel by design,
with built-in suite diagnostics (`--doctor`).

```console
$ pip install rstest
$ rstest
rstest 0.3.1 — 8 workers (parallel by default; -n 0 for single-worker mode)
........................................................................ [ 34%]
........................................................................ [ 69%]
......................................................                   [100%]

956 passed, 25 skipped in 2.5s
```

## Highlights

- **Drop-in.** rstest runs your tests through a vendored pytest core:
  conftest hierarchies, fixtures, parametrize, marks, and your installed
  pytest plugins (pytest-django, pytest-asyncio, hypothesis, pytest-mock,
  ...) load and behave exactly as under pytest. The pytest flag surface
  (`-k`, `-m`, `-x`, `--lf`, plugin flags) forwards unchanged.
- **Parallel by design.** Test-granular work distribution across worker
  processes, duration-aware scheduling that starts your slowest tests
  first, and safety rails for tests that can't parallelize
  (`@pytest.mark.serial`, `--dist loadfile`).
- **Crash-safe.** A segfaulting test costs you one FAILED line — the worker
  is replaced, its remaining tests redistribute, and the run completes.
- **`rstest --doctor`.** Tells you *why* the suite is slow: tests that wait
  instead of compute, the long-pole tests that cap any parallelism, fixture
  hotspots, slowest files.
- **`rstest --watch`.** Instant reruns on save; changed test files rerun
  alone, source changes rerun only the tests the import graph says are
  affected.

## Measured

Outcome parity is measured per-test against pytest baselines across four
real suites (201,127 tests total):

--8<-- "docs/reference/benchmarks.md:suite-table"

Parity means identical per-test setup/call/teardown outcomes, including
skips, xfails, and expected failures — with the suites' real plugins loaded.
See [Benchmarks](reference/benchmarks.md) for methodology and caveats.

Read the speed numbers honestly: the wins come from suite *shape*, not magic.
Wait-bound suites (aiohttp) gain most, and only on a **warm** duration cache —
the first run is cold, since duration-aware scheduling needs one run of timing
data. CPU-bound suites already split well under xdist, so rstest lands at
parity there, not a win (pandas). In ephemeral CI, cache `.rstest_cache`
across runs or expect cold-run timing.

## The compatibility contract

- At `-n 0`, rstest runs one pytest session: **byte-exact pytest
  semantics**, including output rendered by pytest itself for `--co`, `-s`,
  and `--pdb`.
- In parallel modes, outcomes are preserved for parallel-safe tests. Tests
  with hidden timing, ordering, or shared-state assumptions can flake under
  high concurrency — exactly as under pytest-xdist. The
  [parallel safety](guides/parallel-safety.md) guide covers finding and
  containing them.

## Where next

- [Installation](getting-started/installation.md)
- [Your first test](getting-started/your-first-test.md) — no suite yet? from empty folder to green run
- [First steps](getting-started/first-steps.md) — already have a pytest suite? run it from your project root
- [Migrating from pytest](guides/migrate-from-pytest.md)
- [Glossary](concepts/glossary.md) — worker, byte-exact, long-pole, and the rest
