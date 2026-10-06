# rstest

A fast, pytest-compatible test runner. Rust orchestration; runs most pytest
suites unchanged, with the same plugins and fixtures, in parallel by default,
with built-in suite diagnostics (`--doctor`). The guarantees are in
[the compatibility contract](#the-compatibility-contract) below.

Evaluating rstest for your team? Start with
[Evaluating rstest](getting-started/evaluating.md).

!!! note "Not the Rust crate"
    This is the Python test runner on PyPI (`pip install rstest`). It is not
    related to the Rust fixture crate [`rstest`](https://crates.io/crates/rstest)
    on crates.io.

```console
$ pip install rstest
$ rstest -n 4      # -n is optional; plain `rstest` picks a worker count
rstest 0.8.0 — 4 workers (parallel by default; -n 0 for single-worker mode)
........................................................................ [ 34%]
........................................................................ [ 69%]
......................................................                   [100%]

956 passed, 25 skipped in 2.50s
```

## Highlights

- **Your tests, unchanged.** A vendored pytest 9.1.1 core runs your conftest
  hierarchies, fixtures, parametrize, marks, and installed plugins
  (pytest-django, pytest-asyncio, hypothesis, pytest-mock, ...) as pytest
  does. Most pytest flags (`-k`, `-m`, `-x`, `--lf`, plugin flags) forward
  unchanged; a few, such as `--timeout`, `--reruns` and `--html`, are
  rstest's own with the same basic syntax as the plugins they replace.
- **xdist semantics in parallel.** Session fixtures run once per worker, as
  under pytest-xdist, plus a
  [short list of differences](guides/migrate-from-pytest.md#what-changes).
- **Parallel by design.** Work distribution across worker processes,
  duration-aware scheduling that starts your slowest tests first (per test,
  or per file when a large suite gets
  [lazy collection](concepts/lazy-collection.md)), and safety rails for
  tests that can't parallelize
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
real suites (201,343 tests total):

--8<-- "docs/reference/benchmarks.md:suite-table"

Parity means identical per-test setup/call/teardown outcomes, including
skips, xfails, and expected failures, with the suites' real plugins loaded.
See [Benchmarks](reference/benchmarks.md) for methodology and caveats.

Speed depends on suite *shape* and on a warm duration cache (the first run
is cold): see [Evaluating rstest](getting-started/evaluating.md#what-it-speeds-up)
for what to expect from yours.

## The compatibility contract

- At `-n 0` (single-worker mode), rstest runs one pytest session:
  **per-test outcomes match pytest exactly**, and pytest renders the output
  itself for `--co`, `-s`, and `--pdb`. The exceptions are the
  [few flags rstest shares with pytest or a plugin](reference/cli.md#shadowed-flags),
  which rstest handles itself at every worker count; see
  [Compatibility](concepts/compatibility.md#the-contract).
- In parallel modes, outcomes are preserved for parallel-safe tests. Tests
  with hidden timing, ordering, or shared-state assumptions can flake under
  high concurrency, exactly as under pytest-xdist. The
  [parallel safety](guides/parallel-safety.md) guide covers finding and
  containing them.

## Go deeper

- [Installation](getting-started/installation.md)
- [Start from scratch](getting-started/your-first-test.md): no suite yet? from empty folder to green run
- [Run your existing suite](getting-started/first-steps.md): already have a pytest suite? run it from your project root
- [Migrating from pytest](guides/migrate-from-pytest.md)
- [Glossary](concepts/glossary.md): worker, single-worker mode, long pole, and the rest
