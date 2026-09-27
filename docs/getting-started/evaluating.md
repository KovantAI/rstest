# Evaluating rstest

A one-page summary for deciding whether to adopt rstest: what it speeds up,
when not to adopt it, how mature it is, and what adopting and backing out
cost. Every item links to the page with the detail.

## Try it first

In a project where plain pytest already works:

```console
$ pip install rstest
$ rstest try
```

`try` runs your suite once under plain pytest and once under `rstest -n auto`,
then reports whether per-test outcomes match and how much faster rstest was.
It needs pytest installed in the project environment for the baseline run, and
it takes as long as both runs. See [`try`](../reference/cli-commands.md#try).

## What it speeds up

The win depends on the suite's shape:

--8<-- "README.md:speed-table"

Measured numbers, machine and method: [Benchmarks](../reference/benchmarks.md).
The first run is cold; duration-aware scheduling needs one run of timing data,
so judge speed on the second run (in ephemeral CI, persist `.rstest_cache`).

## When not to adopt it

- **CPU-bound suites that already split evenly under xdist.** rstest lands at
  parity with xdist there
  ([CPU-bound suites](../reference/benchmarks.md#cpu-bound-suites)). The
  remaining value is `--doctor`, `--watch` and the rest, not speed; see
  [Already fast under xdist?](../guides/migrate-from-xdist.md#already-fast-cpu-bound).
- **Suites that can't move to pytest 9.** rstest runs a vendored pytest 9.1.1
  core whatever pytest you have installed, and there is no older-core build
  ([Vendored pytest version](../concepts/compatibility.md#vendored-pytest-version)).
- **Reliance on single-controller plugins at `-n ≥ 2`.** pytest-html (from
  `addopts` or after `--`), pytest-reportlog and pytest-json-report write
  nothing in parallel, and terminal-UI plugins such as pytest-sugar don't
  paint. rstest's own `--html` works at any worker count
  ([Known gaps](../concepts/compatibility.md#known-gaps)).
- **Unstable parametrize ids.** If ids come from memory addresses, reprs,
  uuids or sub-second timestamps, workers collect different test sets and
  rstest refuses to dispatch (pydantic is the measured case:
  [Parity divergences §2](../reference/parity-divergences.md#2-non-deterministic-nodeids-memory-addresses-reprs)).
  The fix is stable `ids=`, or `-n 0`.
- **Windows-heavy fleets.** Windows is supported and runs the full test gate
  in CI, but the public-suite corpus runs only on macOS/Linux, so
  real-world validation on Windows is lighter
  ([Known gaps](../concepts/compatibility.md#known-gaps)).
- **No tolerance for 0.x churn.** rstest is alpha: expect breaking changes
  between minor versions until 1.0.

## Maturity

- **Releases:** first release 0.0.1 on 2026-06-10; 15 releases through 0.7.0
  (2026-09-10), every change listed in the
  [CHANGELOG](https://github.com/KovantAI/rstest/blob/main/CHANGELOG.md).
- **Maintainer:** Kovant AB ([Security](../reference/security.md)).
- **Security support:** fixes land on the latest release only; there are no
  long-term-support branches yet
  ([Supported versions](../reference/security.md#supported-versions)). An
  upstream pytest security fix affecting the vendored code is expected in an
  rstest release within two weeks
  ([policy](../reference/security.md#handling-pytest-security-fixes)).
- **Compatibility evidence:** per-test outcome diffing against pytest on real
  suites ([Compatibility](../concepts/compatibility.md#what-verified-means)).

## Cost of adopting

Adopting rstest adopts pytest 9. A suite that is warning-clean on recent
pytest 8.x is almost always already pytest-9-clean; if not, clear the
deprecations first ([Upgrading to pytest 9](../guides/upgrade-to-pytest9.md)).
Tests that aren't parallel-safe need `@pytest.mark.serial` or a fix
([Parallel safety](../guides/parallel-safety.md)), and session fixtures run
once per worker, as under pytest-xdist. The step-by-step, with a shadow stage
next to your existing pytest job, is
[Migrating from pytest](../guides/migrate-from-pytest.md).

## Cost of backing out

To roll back, point CI at `pytest` again: pytest ignores `[tool.rstest]` and
`.rstest_cache/`. Each rstest feature you adopted (`@pytest.mark.serial`,
`--reruns`, `--timeout`, `--html`, `RSTEST_*` variables) adds a step; the list
with portable alternatives is
[What ties you to rstest](../guides/migrate-from-pytest.md#what-ties-you-to-rstest).
