# rstest

A fast, pytest-compatible test runner. Rust orchestration, your tests
unchanged: same plugins, same fixtures, same outcomes — parallel by design,
with built-in suite diagnostics (`--doctor`).

```
pip install rstest   # pre-release: install from a built wheel for now, see docs/getting-started/installation.md
rstest
```

Parallel by default (`-n auto`). Tests that can't run in parallel can be
marked `@pytest.mark.serial` (they run exclusively, after the parallel
phase), order-dependent suites can use `--dist loadfile`, and `rstest -n 0`
gives a single pytest session with byte-exact pytest semantics.

The compat contract: at `-n 0`, outcomes are pytest-exact. In parallel
modes, outcomes are preserved for parallel-safe tests; tests with hidden
time/ordering/shared-state assumptions can flake under high concurrency —
exactly as under pytest-xdist. `rstest --doctor` and lower `-n` values help
find and contain them; `@pytest.mark.serial` is the escape hatch.

Highlights:
- Drop-in: forwards the pytest flag surface; runs conftest, fixtures,
  parametrize, marks, and pytest plugins (pytest-django, pytest-asyncio,
  hypothesis, ...) through a vendored pytest core.
- Parallel by design: test-granular work distribution with duration-aware
  scheduling; `@pytest.mark.serial` and `--dist loadfile` safety rails;
  crashed workers respawn without losing your run.
- `rstest --doctor`: wait-bound tests, parallel-floor analysis, fixture
  hotspots, slowest files.
- `rstest --watch`: instant reruns on save; changed test files rerun alone,
  source changes rerun only the tests the import graph says are affected.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
