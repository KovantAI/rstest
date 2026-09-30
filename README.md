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

**Runs most pytest suites unchanged, in parallel, usually faster.** Same
fixtures, same plugins: byte-exact per-test outcomes at `-n 0`, and in
parallel the same caveats as pytest-xdist (see
[Known gaps](https://python-rstest.readthedocs.io/en/stable/concepts/compatibility/#known-gaps)).
Rust-orchestrated, parallel by default, with built-in suite diagnostics
(`--doctor`) that tell you *where your test time actually goes*.

> **Note:** This is the Python test runner on PyPI (`pip install rstest`). It is
> not related to the Rust fixture crate [`rstest`](https://crates.io/crates/rstest)
> on crates.io.

```text
aiohttp, 4,469 tests:   pytest 193s  →  rstest 67s warm (150s cold), -n 8
```

<p align="center">
  <img src="https://raw.githubusercontent.com/KovantAI/rstest/main/docs/assets/rstest-demo.gif" alt="Terminal recording: the aiohttp suite under pytest (193s), then rstest --doctor (67s, 14 parallel workers) pinpointing the wait-bound file that gates the suite" width="820">
</p>

<p align="center"><sub>Same suite: <b>pytest 193s → rstest 67s</b> (warm, <code>-n auto</code> = 14 workers, as recorded); <code>--doctor</code> shows <i>where the time goes</i>. Current measured numbers: <a href="https://python-rstest.readthedocs.io/en/stable/reference/benchmarks/">benchmarks</a>.</sub></p>

📚 **[Full documentation → python-rstest.readthedocs.io](https://python-rstest.readthedocs.io/en/stable/)**

## Quick start

Evaluating rstest? Run `rstest try` first: it runs your suite under plain
pytest and under rstest, then tells you whether outcomes match and how much
faster rstest was ([details](#will-rstest-speed-up-your-suite)).

In any pytest project:

```bash
pip install rstest      # or: uv pip install rstest
rstest
```

Also works with your tool of choice:

```bash
uv add --dev rstest           # uv projects
poetry add --group dev rstest # Poetry
pdm add -dG test rstest       # PDM
```

Install rstest into the same environment as your tests: workers run in
that interpreter. (A `pipx` / `uv tool` install also needs rstest in the
project environment; see
[Installation](https://python-rstest.readthedocs.io/en/stable/getting-started/installation/).)

Requires Python 3.10+ on macOS, Linux, or Windows. Windows runs the full
test gate in CI, but the 33-suite public corpus runs only on macOS/Linux, so
Windows is validated at a smaller scale. rstest is alpha (0.x):
expect breaking changes between minor versions until 1.0.

No config and no test changes needed: rstest runs your pytest suite in
parallel (`-n auto`) out of the box. As with pytest-xdist, each worker is a
separate process, so session- and module-scoped fixtures run once per
worker, not once per run.

- Tests that can't run in parallel (shared files, ports, databases) →
  `@pytest.mark.serial` (run exclusively, after the parallel phase).
- Tests that depend on order within a file → `--dist loadfile` (keeps each
  file's tests on one worker); across files → `rstest -n 0`.
- Want byte-exact pytest semantics → `rstest -n 0` (single pytest session;
  per-test outcomes match pytest exactly).

## Will rstest speed up *your* suite?

The wins come from suite *shape*, not magic. Quick self-check:

<!-- --8<-- [start:speed-table] -->
| Your suite | What to expect |
|---|---|
| Wait-bound (IO, sleeps, network, timeouts) | **Biggest win**: xdist's default `--dist load` hands out consecutive batches in collection order with no timing data, so a file of slow tests clusters on a few workers and starts late; rstest's duration cache starts the slowest tests first, spread across workers. |
| CPU-bound, already splits well under xdist | **Parity, not a win**: gain up to the performance-core count, same as xdist (sympy `-n 8`: 15.6s vs 15.5s). |
| Very many tests (100k+) | **Win over xdist**: xdist's single Python controller becomes the bottleneck; rstest's orchestrator is Rust (pandas `-n 8`: 43s vs 89s). |
| Gated by one long test | **No win beyond that test**: no worker count beats the long pole. `--doctor` names it. |
| Small (< ~10s serial) | **Little wall-time change**: value is `--watch`, `--changed`, `--doctor`, not raw speed. |
| Many tiny per-service suites | Speedup is per-suite; the aggregate CI win depends on your largest suites. |

<!-- --8<-- [end:speed-table] -->

Two more things to know before you benchmark:

- **Warm cache matters.** Duration-aware scheduling needs one run of timing
  data. First run is cold; the win arrives on run two. In ephemeral CI,
  persist `.rstest_cache` or expect cold-run timing.
- **Adopting rstest adopts pytest 9.** rstest runs a vendored pytest 9.1.1
  core. A suite that's warning-clean on recent pytest 8.x is almost always
  already pytest-9-clean; if not, clear deprecations first (the same upgrade
  you'd owe pytest anyway). `rstest -n 0` surfaces them. Plugins run on
  that core too, so a plugin's `pytest<9` pin is inert
  ([details](https://python-rstest.readthedocs.io/en/stable/concepts/compatibility/#plugin-versions-vs-the-vendored-core)).

Fastest way to find out for real: `rstest try` runs your suite under plain
pytest and under `rstest -n auto`, then reports whether outcomes match and
how much faster rstest was. No migration, no config. (It runs the baseline
as `python -m pytest`, so pytest must be installed in the project's
environment for this one command.)

## Benchmarks

Real open-source suites, end-to-end, with per-test outcome diffing against
the pytest baseline: 100% parity on pandas, django-allauth and rich, and
99.91-99.98% on aiohttp, whose socket-leak warning flake hits xdist too (every
known flake is catalogued in the docs).

<!-- SOURCE OF TRUTH: docs/reference/benchmarks.md, keep numbers in sync -->
| Suite | Tests | pytest | xdist `-n 8` | rstest `-n 8` |
|---|---|---|---|---|
| aiohttp | 4,469 | 193s | 160s | **67s** warm · 150s cold |
| pandas | 193,843 | 190s | 89s | **43s** (xdist's controller is the bottleneck) |
| django-allauth | 2,050 | 26s | 8.9s | **5.8s** |
| rich | 981 | 3.7s | 2.7s | **2.5s** |

Apple M4 Max, CPython 3.13, pytest-xdist 3.8, median of 5 runs at the same
`-n` for both runners. CPU-bound suites (sympy, scikit-learn) land at parity
with xdist; see the benchmarks page.

**Monorepo** (langchain-ai/langgraph, 6 `libs/*` packages, 4,284 tests, each
with its own pytest config; a single pytest can't run from the root at all):

<!-- SOURCE OF TRUTH: docs/reference/benchmarks.md, keep numbers in sync -->
| | wall | parity |
|---|---|---|
| pytest: 6 serial invocations | 880.4s | baseline |
| rstest at the root, cold | **245.7s** (3.6×) | 100% |

Only the cold run is measured. From its per-project duration caches, a warm
run is **projected** (not measured) at 121–133s (6.6–7.3×); discount that
until you measure your own.

Full methodology:
[benchmarks](https://python-rstest.readthedocs.io/en/stable/reference/benchmarks/).

## Why rstest

| | pytest | pytest-xdist | rstest |
|---|:---:|:---:|:---:|
| Runs your suite unchanged | ✅ | parallel-safe tests | ✅ at `-n 0`; parallel-safe tests at `-n ≥ 2` |
| Parallel by default | ❌ | ⚙️ opt-in | ✅ |
| Duration-aware scheduling | plugin (pytest-split, across CI jobs) | ❌ | ✅ |
| Crashed workers replaced mid-run | ❌ | ✅ | ✅ |
| Crash pinned to the exact test | ❌ | inferred from queue | ✅ explicit per-test signal |
| Suite diagnostics | ❌ | ❌ | ✅ `--doctor` |
| Watch mode | plugin (pytest-watch) | `--looponfail` (deprecated) | ✅ built-in |

- **pytest underneath.** Forwards the pytest flag surface; runs conftest, fixtures,
  parametrize, marks, and pytest plugins (pytest-django, pytest-asyncio,
  hypothesis, …) through a vendored pytest core.
- **Parallel by design.** Duration-aware work distribution that starts the
  slowest tests first (per test, or per file when a large suite gets
  [lazy collection](https://python-rstest.readthedocs.io/en/stable/concepts/lazy-collection/)); `@pytest.mark.serial`
  and `--dist loadfile` safety rails;
  crashed workers respawn without losing your run.
- **`rstest --doctor`.** Wait-bound tests, parallel-floor analysis, fixture
  hotspots, slowest files.
- **`rstest --watch`.** Instant reruns on save; changed test files rerun
  alone, source changes rerun only the tests the import graph says are
  affected.

<details>
<summary><strong>Compatibility contract</strong></summary>

At `-n 0` (byte-exact mode), per-test outcomes match pytest exactly: one
vendored-pytest session; any difference at `-n 0` is a bug. The guarantee is
per-test outcomes (every phase, skips, xfails). With no `--output` set,
the terminal output at `-n 0` is pytest's own as well, with
rstest's extras (doctor, coverage, gate messages) appended after it, and
`--junitxml` is pytest's own document at every worker count. In parallel modes, outcomes are preserved for
parallel-safe tests; tests with hidden time/ordering/shared-state
assumptions can flake under high concurrency, exactly as under
pytest-xdist. `rstest --doctor` and lower `-n` values help find and contain
them; `@pytest.mark.serial` is the escape hatch.

**Vendored pytest 9.** rstest runs a vendored **pytest 9.1.1** core, so
adopting rstest adopts pytest 9's behavior regardless of the pytest version
installed. The 8→9 gap is a cleanup major (removes already-deprecated APIs);
a suite warning-clean on recent pytest 8.x is almost always pytest-9-clean.
If it isn't, clear the deprecations first: the same upgrade you'd owe pytest
anyway. There is one vendored core, tracked forward; no older-core build.

**Silent-at-`-n ≥ 2` plugins.** A few plugins that need a single controller
process are no-ops in parallel: report plugins such as pytest-reportlog and
pytest-json-report write nothing (no crash), and terminal-UI plugins
(pytest-sugar and friends) don't paint; data-level behavior is unaffected.
`--html` is handled by rstest itself and works at any worker count. Full list:
[Known gaps](https://python-rstest.readthedocs.io/en/stable/concepts/compatibility/#known-gaps).

</details>

## `rstest --doctor`

Runs your suite, then answers *where does the time actually go?*, from data
the runner already owns (per-test wall/CPU time, per-fixture setup):

<!-- SOURCE OF TRUTH: docs/guides/doctor.md, keep sample in sync -->
```text
================== rstest doctor ==================
4442 tests, 185.8s test time (wall 67.7s, 8 workers)

WAIT-BOUND: 95% of test time (176.5s) is waiting, not computing (sleeps / IO / timeouts).
    54.20s waiting of   54.25s  tests/test_proxy_functional.py::test_proxy_https_multi_conn_limit
    10.97s waiting of   10.97s  tests/test_proxy_functional.py::test_proxy_https_connect
  ... and 33 more

PARALLEL FLOOR: the longest test (54.2s) exceeds the ideal per-worker share (23.2s at -n 8);
no worker count can finish faster than its longest test. Gate tests:
    54.25s  tests/test_proxy_functional.py::test_proxy_https_multi_conn_limit

FIXTURE HOTSPOTS (setup time across all workers):
     0.79s   4442x  scope=function blockbuster
     0.54s    157x  scope=function transport

SLOWEST FILES:
   150.46s (81.0%)  tests/test_proxy_functional.py
     8.60s ( 4.6%)  tests/test_client_functional.py
===================================================
```

That's aiohttp's real suite: one file is 81% of total test time, almost all
of it waiting on 10-second proxy timeouts. (Doctor counts tests with a
recorded call duration, so skipped tests drop out: 4442 here against the
4,469 collected.)
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

- Apache License, Version 2.0 ([LICENSE-APACHE](https://github.com/KovantAI/rstest/blob/main/LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](https://github.com/KovantAI/rstest/blob/main/LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
