# rstest

[![CI](https://github.com/KovantAI/rstest/actions/workflows/ci.yml/badge.svg)](https://github.com/KovantAI/rstest/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/KovantAI/rstest/graph/badge.svg)](https://codecov.io/gh/KovantAI/rstest)
[![PyPI](https://img.shields.io/pypi/v/rstest)](https://pypi.org/project/rstest/)
[![Python versions](https://img.shields.io/pypi/pyversions/rstest)](https://pypi.org/project/rstest/)
[![Wheel](https://img.shields.io/pypi/wheel/rstest)](https://pypi.org/project/rstest/)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](https://github.com/KovantAI/rstest#license)
[![Downloads](https://static.pepy.tech/badge/rstest/month)](https://pepy.tech/project/rstest)
[![Docs](https://readthedocs.org/projects/python-rstest/badge/?version=stable)](https://python-rstest.readthedocs.io/en/stable/)
[![GitHub stars](https://img.shields.io/github/stars/KovantAI/rstest)](https://github.com/KovantAI/rstest/stargazers)

A fast, pytest-compatible test runner. Rust orchestration, your tests
unchanged: same plugins, same fixtures, same outcomes — parallel by design,
with built-in suite diagnostics (`--doctor`).

📚 **[Full documentation → python-rstest.readthedocs.io](https://python-rstest.readthedocs.io/en/stable/)**

## Quick start

```bash
pip install rstest
rstest
```

Parallel by default (`-n auto`). Tests that can't run in parallel can be
marked `@pytest.mark.serial` (they run exclusively, after the parallel
phase), order-dependent suites can use `--dist loadfile`, and `rstest -n 0`
gives a single pytest session with byte-exact pytest semantics.

<details>
<summary><strong>Compatibility contract</strong></summary>

At `-n 0`, outcomes are byte-exact. In parallel modes, outcomes are
preserved for parallel-safe tests; tests with hidden
time/ordering/shared-state assumptions can flake under high concurrency —
exactly as under pytest-xdist. `rstest --doctor` and lower `-n` values help
find and contain them; `@pytest.mark.serial` is the escape hatch.

</details>

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

## Benchmarks

Real open-source suites, end-to-end, with per-test outcome diffing against
the pytest baseline — 100% parity.

<!-- SOURCE OF TRUTH: docs/reference/benchmarks.md — keep numbers in sync -->
| Suite | Tests | pytest | xdist (`-n 8`) | rstest |
|---|---|---|---|---|
| aiohttp | 4,469 | 197s | 160s | **68s** |
| pandas | 193,627 | 182s | 61s | 63s |
| django-allauth | 2,050 | 22s | 8s | **8s** (`-n 4`) |
| rich | 981 | 3.4s | 2.8s | **2.5s** (`-n 4`) |

Apple Silicon, CPython 3.13, pytest-xdist 3.8. Full methodology and monorepo
numbers: [benchmarks](https://python-rstest.readthedocs.io/en/stable/reference/benchmarks/).

## `rstest --doctor`

Runs your suite, then answers *where does the time actually go?* — from data
the runner already owns (per-test wall/CPU time, per-fixture setup):

<!-- SOURCE OF TRUTH: docs/guides/doctor.md — keep sample in sync -->
```text
================== rstest doctor ==================
4442 tests, 185.8s test time (wall 67.7s, 8 workers)

WAIT-BOUND: 95% of test time (176.5s) is waiting, not computing (sleeps / IO / timeouts).
    54.20s waiting of   54.25s  tests/test_proxy_functional.py::test_proxy_https_multi_conn_limit
  ... and 33 more

PARALLEL FLOOR: the longest test (54.2s) exceeds the ideal per-worker share (23.2s at -n 8);
no worker count can finish faster than its longest test.

FIXTURE HOTSPOTS (setup time across all workers):
     0.79s   4442x  scope=function blockbuster

SLOWEST FILES:
   150.46s (81.0%)  tests/test_proxy_functional.py
===================================================
```

That's aiohttp's real suite — one file is 81% of total test time, almost all
of it waiting on 10-second proxy timeouts.
[More →](https://python-rstest.readthedocs.io/en/stable/guides/doctor/)

## Docs

- [Getting started](https://python-rstest.readthedocs.io/en/stable/getting-started/)
- [Migrating from pytest](https://python-rstest.readthedocs.io/en/stable/guides/migrate-from-pytest/)
- [Migrating from pytest-xdist](https://python-rstest.readthedocs.io/en/stable/guides/migrate-from-xdist/)
- [Parallel safety](https://python-rstest.readthedocs.io/en/stable/guides/parallel-safety/)
- [Suite diagnostics (`--doctor`)](https://python-rstest.readthedocs.io/en/stable/guides/doctor/)
- [Watch mode](https://python-rstest.readthedocs.io/en/stable/guides/watch-mode/)
- [CI quickstart](https://python-rstest.readthedocs.io/en/stable/guides/ci-quickstart/)
- [CLI reference](https://python-rstest.readthedocs.io/en/stable/reference/cli/)

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
