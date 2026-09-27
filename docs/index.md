# rstest

A fast, pytest-compatible test runner. Rust orchestration, your tests
unchanged: same plugins, same fixtures, same outcomes, parallel by design,
with built-in suite diagnostics (`--doctor`).

!!! note "Not the Rust crate"
    This is the Python test runner on PyPI (`pip install rstest`). It is not
    related to the Rust fixture crate [`rstest`](https://crates.io/crates/rstest)
    on crates.io.

```console
$ pip install rstest
$ rstest -n 4
rstest 0.7.0 — 4 workers (parallel by default; -n 0 for single-worker mode)
........................................................................ [ 34%]
........................................................................ [ 69%]
......................................................                   [100%]

956 passed, 25 skipped in 2.5s
```

## Highlights

- **Your tests, unchanged.** rstest runs your tests through a vendored
  pytest core (pytest 9.1.1): conftest hierarchies, fixtures, parametrize,
  marks, and your installed pytest plugins (pytest-django, pytest-asyncio,
  hypothesis, pytest-mock, ...) load as under pytest. Most pytest flags
  (`-k`, `-m`, `-x`, `--lf`, plugin flags) forward unchanged; a few names
  such as `--timeout`, `--reruns` and `--html` are rstest's own. In parallel
  you get xdist's semantics (session fixtures once per worker) plus a
  [short list of differences](guides/migrate-from-pytest.md#what-changes);
  at `-n 0` outcomes match pytest exactly.
- **Parallel by design.** Test-granular work distribution across worker
  processes, duration-aware scheduling that starts your slowest tests
  first, and safety rails for tests that can't parallelize
  (`@pytest.mark.serial`, `--dist loadfile`).
- **Crash-safe.** A segfaulting test costs you one FAILED line: the worker
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
skips, xfails, and expected failures: with the suites' real plugins loaded.
See [Benchmarks](reference/benchmarks.md) for methodology and caveats.

Read the speed numbers honestly: the wins come from suite *shape*, not magic.
Wait-bound suites (aiohttp) gain most, and only on a **warm** duration cache:
the first run is cold, since duration-aware scheduling needs one run of timing
data. CPU-bound suites already split well under xdist, so rstest lands at
parity there, not a win (pandas): see [Already fast under
xdist?](guides/migrate-from-xdist.md#already-fast-cpu-bound) for what's still
worth it. In ephemeral CI, cache `.rstest_cache`
across runs or expect cold-run timing.

## The compatibility contract

- At `-n 0`, rstest runs one pytest session: **per-test outcomes match
  pytest exactly**, and pytest renders the output itself for `--co`, `-s`,
  and `--pdb`. The exceptions are the few flags rstest owns (`--junitxml`,
  `--html`, `--timeout`, `--reruns`, `--debug`), which rstest handles itself
  at every worker count; see
  [Compatibility](concepts/compatibility.md#the-contract).
- In parallel modes, outcomes are preserved for parallel-safe tests. Tests
  with hidden timing, ordering, or shared-state assumptions can flake under
  high concurrency: exactly as under pytest-xdist. The
  [parallel safety](guides/parallel-safety.md) guide covers finding and
  containing them.

## Where next

- [Installation](getting-started/installation.md)
- [Start from scratch](getting-started/your-first-test.md): no suite yet? from empty folder to green run
- [Run your existing suite](getting-started/first-steps.md): already have a pytest suite? run it from your project root
- [Migrating from pytest](guides/migrate-from-pytest.md)
- [Glossary](concepts/glossary.md): worker, byte-exact, long pole, and the rest
