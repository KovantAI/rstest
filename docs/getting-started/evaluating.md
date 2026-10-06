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
No migration and no config. A typical report:

```text
================= rstest try =================
  ✓ parity:  8337 tests — identical outcomes to pytest
  ⚡ speed:   pytest 1m36s  →  rstest 21.0s   (4.6× at -n auto = -n 8)
  💸 saves   1m15s per run — ≈ 52m30s over your last 30 days (42 commits ≈ CI runs)
================================================
  → drop-in ready: `rstest` is `pytest`, in parallel. Switch with confidence.
```

The 30-day figure projects the saving over the repository's recent commits
(one commit taken as one CI run); outside a git checkout the line stops at
`per run`.

It needs pytest installed in the project environment for the baseline run
(it runs `python -m pytest` with the interpreter rstest uses), and it takes
as long as both runs. The baseline uses your installed pytest, so on pytest 8
a difference can be pytest 8 versus 9 rather than rstest
([below](#when-not-to-adopt-it)). If outcomes differ, it exits 1 and points
you at `rstest migrate-check`. Exit codes and the no-verdict cases:
[`try`](../reference/cli-commands.md#try).

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
- **Suites that can't move to pytest 9.** rstest always runs its vendored
  pytest 9 core
  ([Your suite runs on pytest 9](installation.md#your-suite-runs-on-pytest-9)).
- **Reliance on controller-only report plugins at `-n ≥ 2`.** Some plugins
  write their report only from pytest-xdist's
  [controller](../concepts/glossary.md#controller) process, which rstest
  doesn't have: pytest-html (from `addopts` or after `--`), pytest-reportlog
  and pytest-json-report write nothing in parallel, and terminal-UI plugins such as pytest-sugar don't
  paint. rstest's own `--html` works at any worker count
  ([Known gaps](../concepts/compatibility.md#known-gaps)).
- **Unstable parametrize ids.** Ids built from memory addresses, reprs,
  uuids or timestamps make workers collect different test sets, and when
  rstest sees that it refuses to dispatch rather than run the wrong tests.
  It can only see it under full collection, when every worker collects the
  whole suite: any run that
  [lazy collection's auto-default](../concepts/lazy-collection.md#auto-default)
  doesn't pick (including the first, cold-cache run of any suite and every
  run of a suite below the size threshold) and any run pinned to
  `--collect full`. Such a suite will hit it. The fix is stable `ids=`, or `-n 0`
  ([Unstable parametrize ids](../concepts/compatibility.md#unstable-parametrize-ids)).
- **Windows-heavy fleets.** Windows is supported and gated in CI, but
  real-world validation there is lighter and `--timeout` can't interrupt a
  test in-process
  ([Running on Windows](../guides/windows.md#validation)).
- **No tolerance for 0.x churn.** rstest is alpha: expect breaking changes
  between minor versions until 1.0.

## Maturity

- **Status:** alpha (0.x). CLI flags and the report-json schema aim for
  stability but may change between minor versions until 1.0.
- **Releases:** first release 0.0.1 on 2026-06-10; 16 releases through 0.8.0
  (2026-09-30), every change listed in the
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

Adopting rstest adopts pytest 9 (see above). Tests that aren't
parallel-safe need `@pytest.mark.serial` or a fix
([Parallel safety](../guides/parallel-safety.md)), and session fixtures run
[once per worker](../guides/parallel-safety.md#session-scoped-fixtures-duplicate),
as under pytest-xdist. The step-by-step is
[Migrating from pytest](../guides/migrate-from-pytest.md); it starts with a
[shadow stage](../guides/migrate-from-pytest.md#rolling-out-in-stages-and-rolling-back),
an rstest CI job that runs next to your existing pytest job without being a
required check.

## Cost of backing out

To roll back, point CI at `pytest` again: pytest ignores `[tool.rstest]` and
`.rstest_cache/`. Each rstest feature you adopted (`@pytest.mark.serial`,
`--reruns`, `--timeout`, `--html`, `RSTEST_*` variables) adds a step; the list
with portable alternatives is
[What ties you to rstest](../guides/migrate-from-pytest.md#what-ties-you-to-rstest).
