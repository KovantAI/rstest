# rstest

[![CI](https://github.com/KovantAI/rstest/actions/workflows/ci.yml/badge.svg)](https://github.com/KovantAI/rstest/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/KovantAI/rstest/graph/badge.svg)](https://codecov.io/gh/KovantAI/rstest)
[![PyPI](https://img.shields.io/pypi/v/rstest)](https://pypi.org/project/rstest/)
[![Python versions](https://img.shields.io/pypi/pyversions/rstest)](https://pypi.org/project/rstest/)
[![Wheel](https://img.shields.io/pypi/wheel/rstest)](https://pypi.org/project/rstest/)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](https://github.com/KovantAI/rstest#license)
[![Downloads](https://static.pepy.tech/badge/rstest/month)](https://pepy.tech/project/rstest)
[![Docs](https://readthedocs.org/projects/rstest/badge/?version=latest)](https://rstest.readthedocs.io/)
[![GitHub stars](https://img.shields.io/github/stars/KovantAI/rstest)](https://github.com/KovantAI/rstest/stargazers)

A fast, pytest-compatible test runner. Rust orchestration, your tests
unchanged: same plugins, same fixtures, same outcomes — parallel by design,
with built-in suite diagnostics (`--doctor`).

```
pip install rstest
rstest
```

Parallel by default (`-n auto`). Tests that can't run in parallel can be
marked `@pytest.mark.serial` (they run exclusively, after the parallel
phase), order-dependent suites can use `--dist loadfile`, and `rstest -n 0`
gives a single pytest session with byte-exact pytest semantics.

The compat contract: at `-n 0`, outcomes are byte-exact. In parallel
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
