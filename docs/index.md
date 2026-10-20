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

Illustrative run of rich's 981-test suite at `-n 4`. The wall time is left
out because it depends on the machine; measured timings, at `-n 8`, are in
[Benchmarks](reference/benchmarks.md):

```console
$ pip install rstest
$ rstest -n 4      # -n is optional; plain `rstest` picks a worker count
rstest 0.9.0 — 4 workers (parallel by default; -n 0 for single-worker mode)
........................................................................ [ 34%]
........................................................................ [ 69%]
......................................................                   [100%]

956 passed, 25 skipped in [...]s
```

## Highlights

- **Your tests, unchanged.** A vendored pytest 9.1.1 core runs your conftest
  hierarchies, fixtures, parametrize, marks, and installed plugins
  (pytest-django, pytest-asyncio, hypothesis, pytest-mock, ...) as pytest
  does. Most pytest flags (`-k`, `-m`, `-x`, `--lf`, plugin flags) forward
  unchanged; a few, such as `--timeout`, `--reruns` and `--html`, are
  rstest's own with the same basic syntax as the plugins they replace.
- **xdist semantics in parallel.** Session fixtures run
  [once per worker](guides/parallel-safety.md#session-scoped-fixtures-duplicate),
  as under pytest-xdist, plus a
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
  instead of compute, the [long-pole](concepts/glossary.md#long-pole) tests that cap any parallelism, fixture
  hotspots, slowest files.
- **`rstest --watch`.** Reruns on save; changed test files rerun
  alone, source changes rerun only the tests the import graph says are
  affected.

## Measured

Per-test outcome parity against pytest (identical setup/call/teardown
outcomes, with each suite's real plugins loaded) on four real suites
(201,343 tests total):

--8<-- "docs/reference/benchmarks.md:suite-table"

Methodology and caveats: [Benchmarks](reference/benchmarks.md). Speed
depends on suite *shape* and a warm duration cache:
[what to expect from yours](getting-started/evaluating.md#what-it-speeds-up).

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

- [Getting started](getting-started/index.md): pick a starting point by situation
- [Migrating from pytest](guides/migrate-from-pytest.md)
- [CI quickstart](guides/ci-quickstart.md): run rstest in GitHub Actions with a persisted cache
- [Glossary](concepts/glossary.md): worker, single-worker mode, long pole, and the rest
