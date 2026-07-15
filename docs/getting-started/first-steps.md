# First steps

Run rstest from your project root, exactly where you would run pytest:

```console
$ rstest
rstest 0.2.0 — 8 workers (parallel by default; -n 0 for single-worker mode)
........................................................................ [ 48%]
.....................................................s.................. [ 97%]
....                                                                     [100%]

2048 passed, 2 skipped in 7.9s
```

That `dots` output is what you get in CI and in these docs. **On your own
terminal you'll instead see the `bar` view** — a `✓`/`✗` line per test,
inline failures, and a live progress bar — because rstest auto-detects the
TTY:

```
 ✓ tests/test_align.py::test_repr
 ✓ tests/test_bar.py::test_pulse
 ...
Results (7.90s):
  ██████████████████████████████ 2048/2050   2 skipped
```

Both are the same run, different renderer (details in [Reading the
output](#reading-the-output)). This page's examples use `dots` for
stability.

No arguments needed: rstest honors your project's pytest configuration —
`pyproject.toml` / `pytest.ini` / `setup.cfg` / `tox.ini`, including
`testpaths`, `addopts`, `python_files`, and markers — because collection
runs through a vendored pytest core.

## Reading the output

On an interactive terminal the default style is **`bar`** — a
pytest-sugar-style view (a `✓`/`✗` line per test, inline failures, a live
progress bar). When output is piped or running in CI it falls back to the
compact **`dots`** style shown below, so logs stay stable. Pick any style
explicitly with [`--output dots|verbose|bar|github|json`](../reference/cli.md#-output-dotsverbosebargithubjson)
— the rest of this page describes `dots`.

- The **header line** states the worker count. rstest is parallel by
  default; this line is the visible reminder.
- **Dots** stream live as tests finish across all workers: `.` pass, `F`
  fail, `s` skip, `x` xfail, `X` xpass, `E` error — pytest's vocabulary.
- On a terminal, a **live status footer** shows overall progress with an
  ETA and, per worker, exactly which test is running and for how long —
  long-running tests are visible the moment they start, not after they
  finish. (Disabled automatically when output is piped or in CI.)
- **Failures** print with full pytest-style tracebacks (assertion rewriting
  included) and captured stdout/stderr/log sections.
- The **summary line** uses pytest's accounting: the counts match what
  pytest would print for the same run, including warnings.

Add `-v` for one line per test:

```console
$ rstest -v
tests/test_align.py::test_repr PASSED [  0%]
tests/test_bar.py::test_pulse PASSED [  0%]
...
```

In parallel mode each line is prefixed with the worker that ran it
(`gw0`, `gw1`, ...), and lines interleave as workers finish:

```console
$ rstest -n 4 -v
[gw0] tests/test_align.py::test_repr PASSED [  1%]
[gw2] tests/test_bar.py::test_pulse PASSED [  2%]
[gw1] tests/test_login.py::test_session FAILED [  3%]
[gw3] tests/test_align.py::test_wrap PASSED [  4%]
...
```

The same `[gwN]` attribution appears in the failure summary, so you can
see which worker hit each failure:

```console
--- FAILED [gw1] tests/test_login.py::test_session ---
    def test_session():
>       assert resp.status == 200
E       assert 401 == 200
```

At `-n 0`/`-n 1` there is no worker, so the prefix is omitted.

## Selecting tests

Everything you know from pytest works unchanged:

```console
$ rstest tests/test_login.py            # one file
$ rstest tests/test_login.py::TestLogin # one class
$ rstest -k "login and not slow"        # keyword filter
$ rstest -m integration                 # marker filter
$ rstest --lf                           # only last failures
$ rstest -x                             # stop at first failure (globally)
$ rstest --changed                      # only tests affected by your edits
```

`--changed` uses the import graph to run just the tests a change can reach —
see [Watch mode](../guides/watch-mode.md) for the on-save version.

## Controlling parallelism

```console
$ rstest -n 4      # four workers
$ rstest -n auto   # logical cores (the default)
$ rstest -n 0      # byte-exact pytest session (same as -n 1)
$ rstest -n 1      # identical to -n 0
```

`-n 0` and `-n 1` are the compatibility escape hatch — one in-process
pytest session, pytest's own behavior in every detail. See
[Byte-exact mode](../concepts/glossary.md#byte-exact-mode) for what that
guarantees and how it differs from pytest-xdist's `-n 1`.

Commit your defaults so you don't retype flags — `[tool.rstest]` in
`pyproject.toml`:

```toml
[tool.rstest]
numprocesses = "auto"   # -n auto
reruns = 0
worker-timeout = 300
```

Command-line flags override these; full key list in
[CLI → Configuration file](../reference/cli.md#configuration-file).

!!! tip "When to drop to `-n 0`"
    Under ~10 seconds of serial runtime, parallelism rarely pays — worker
    startup amortizes poorly and `-n auto` already caps itself low on small
    suites. Reach for `-n 0` deliberately when you want byte-exact pytest
    behavior: reproducing a difference from pytest, or running an
    order-dependent suite. Above ~10s, let `-n auto` parallelize. The win
    grows with suite size and is largest for wait-heavy suites — run
    [`--doctor`](../guides/doctor.md) if you're unsure where your time goes.

## Two runs make it faster

rstest records per-test durations in `.rstest_cache/`. From the second run
on, the scheduler starts your slowest tests first, which is what keeps
workers busy at the end of the run instead of waiting on one long test.
On wait-heavy suites this is dramatic — aiohttp's suite more than halves
between its cold and warm runs (see [Benchmarks](../reference/benchmarks.md)).

## When something fails

```console
$ rstest --lf        # rerun just the failures
$ rstest --doctor    # and if the suite feels slow, ask why
```

!!! tip "Coming from pytest or pytest-xdist?"
    If tests fail *only* under parallelism on a freshly migrated suite, run
    [`rstest --migrate-check`](../reference/cli.md#-migrate-check) first — it
    classifies each parallel-only failure (order dependency, isolation leak,
    wall-clock timing, unstable id) and names the fix, so you don't triage by
    hand. See [Migrating from pytest](../guides/migrate-from-pytest.md#the-migrate-check-preflight).
