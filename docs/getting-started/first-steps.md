# Run your existing suite

Run rstest from your project root, exactly where you would run pytest. This
sample is django-allauth's 2,050-test suite at `-n 4`, the run recorded in
[Benchmarks](../reference/benchmarks.md) (the middle lines are elided):

```console
$ rstest -n 4
rstest 0.7.0 — 4 workers (parallel by default; -n 0 for single-worker mode)
........................................................................ [  3%]
..............................s......................................... [  7%]
[... 25 more lines ...]
..................................................s..................... [ 98%]
.................................. [100%]

2048 passed, 2 skipped in 8.02s
```

That `dots` output is what you get in CI and in these docs. **On your own
terminal you'll instead see the `bar` view** because rstest auto-detects the
TTY: a `✓`/`✗` line per test, inline failures, and a live footer showing
overall progress with an ETA and, per worker, which test is running and for
how long (`idle` when it has nothing). Here is a small four-test file at
`-n 2`, caught mid-run:

```text
rstest 0.7.0 — 2 workers (parallel by default; -n 0 for single-worker mode)
[gw0] ✓ tests/test_first.py::test_add  0.20s [ 25%]
[gw1] ✓ tests/test_first.py::test_add_zero  0.21s [ 50%]
[gw1] s tests/test_first.py::test_skipped [ 75%]
██████████████████████░░░░░░░░  75% (3/4) ~0s left
gw0    0.0s tests/test_first.py::test_add_negative
gw1    idle
```

When the run ends the footer is replaced by the result bar (green, red, and
yellow segments for passed, failed, and skipped or xfail) and pytest's
summary line:

```text
[gw0] ✓ tests/test_first.py::test_add_negative  0.21s [100%]

Results (0.61s):
  ██████████████████████████████ 4/4
3 passed, 1 skipped in 0.61s
```

Both views render the same run (details in [Reading the
output](#reading-the-output)). This page's examples use `dots` for
stability.

No other arguments needed: rstest honors your project's pytest configuration
(`pyproject.toml` / `pytest.ini` / `setup.cfg` / `tox.ini`, including
`testpaths`, `addopts`, `python_files`, and markers) because collection
runs through a vendored pytest core.

## Reading the output

On an interactive terminal the default style is **`bar`**: a
pytest-sugar-style view (a `✓`/`✗` line per test, inline failures, a live
progress bar). When output is piped or running in CI it falls back to the
compact **`dots`** style shown above, so logs stay stable. Pick any style
explicitly with [`--output dots|verbose|bar|github|json`](../reference/cli.md#-output-dotsverbosebargithubjson):
the rest of this page describes `dots`.

- The **header line** states the worker count. rstest is parallel by
  default; this line is the visible reminder.
- **Dots** stream live as tests finish across all workers: `.` pass, `F`
  fail, `s` skip, `x` xfail, `X` xpass, `E` error, pytest's vocabulary.
- **Failures** print with full pytest-style tracebacks (assertion rewriting
  included) and captured stdout/stderr/log sections.
- The **summary line** uses pytest's accounting: the counts match what
  pytest would print for the same run, including warnings.

The live footer described above belongs to the `bar` view on a terminal;
it is disabled automatically when output is piped or in CI. It is what
makes long-running tests visible the moment they start, not after they
finish.

Add `-v` for one line per test. In parallel mode each line is prefixed with
the worker that ran it (`gw0`, `gw1`, ...), and lines interleave as workers
finish:

```console
$ rstest -n 2 -v
rstest 0.7.0 — 2 workers (parallel by default; -n 0 for single-worker mode)
[gw0] tests/test_first.py::test_add PASSED [ 16%]
[gw1] tests/test_first.py::test_add_zero PASSED [ 33%]
[gw1] tests/test_first.py::test_skipped SKIPPED [ 50%]
[gw0] tests/test_first.py::test_add_negative PASSED [ 66%]
[gw1] tests/test_login.py::test_logout PASSED [ 83%]
[gw0] tests/test_login.py::test_session FAILED [100%]
```

The same `[gwN]` attribution appears in the failure summary, so you can
see which worker hit each failure:

```console
--- FAILED [gw0] tests/test_login.py::test_session ---
def test_session():
        status = 401
>       assert status == 200
E       assert 401 == 200

tests/test_login.py:3: AssertionError
```

At `-n 0`/`-n 1` there is no worker, so the prefix is omitted, and `-v`
lines carry no percentage:

```console
$ rstest -n 0 -v
rstest 0.7.0 — single worker (pytest-exact mode)
tests/test_first.py::test_add PASSED
tests/test_first.py::test_add_negative PASSED
...
```

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

`--changed` runs just the tests a change can reach, using the per-test
coverage index when it is warm and the import graph otherwise:
see [Watch mode](../guides/watch-mode.md) for the on-save version.

## Controlling parallelism

```console
$ rstest -n 4      # four workers
$ rstest -n auto   # the default: logical cores, capped for small suites
$ rstest -n 0      # byte-exact pytest session (same as -n 1)
$ rstest -n 1      # identical to -n 0
```

`-n auto` never starts more workers than you have test files, and once
timings are cached it also caps by total suite time, so a tiny suite runs
on one or two workers. Pass an explicit `-n` to override.

`-n 0` and `-n 1` are the compatibility escape hatch: one pytest session
in a single worker process, pytest's own behavior in every detail. You will
see this one mode under three names: *byte-exact* in these docs,
*pytest-exact* in its run banner, and *single-worker* in the `-n 0` hint of
the parallel banner. See
[Byte-exact mode](../concepts/glossary.md#byte-exact-mode) for what that
guarantees and how it differs from pytest-xdist's `-n 1`.

Commit your defaults so you don't retype flags, `[tool.rstest]` in
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
    Under ~10 seconds of serial runtime, parallelism rarely pays: worker
    startup amortizes poorly and `-n auto` already caps itself low on small
    suites. Reach for `-n 0` deliberately when you want byte-exact pytest
    behavior: reproducing a difference from pytest, or running a suite
    whose tests depend on order across files (if the dependency is only
    within a file, `--dist loadfile` keeps each file on one worker).
    Above ~10s, let `-n auto` parallelize. The win grows with suite size
    and is largest for wait-heavy suites: run [`--doctor`](../guides/doctor.md) if you're unsure where your time goes.

## Two runs make it faster

rstest records per-test durations in `.rstest_cache/`. From the second run
on, the scheduler starts your slowest tests first, which is what keeps
workers busy at the end of the run instead of waiting on one long test.
On wait-heavy suites this is dramatic: aiohttp's suite nearly halves
between its cold and warm runs (see [Benchmarks](../reference/benchmarks.md)).

## When something fails

```console
$ rstest --lf        # rerun just the failures
$ rstest --doctor    # and if the suite feels slow, ask why
```

!!! tip "Coming from pytest or pytest-xdist?"
    If tests fail *only* under parallelism on a freshly migrated suite, run
    [`rstest migrate-check`](../reference/cli-commands.md#migrate-check) first: it
    classifies each parallel-only failure (order dependency, isolation leak,
    wall-clock timing, unstable id) and names the fix, so you don't triage by
    hand. See [Migrating from pytest](../guides/migrate-from-pytest.md#the-migrate-check-preflight).

## Which command when?

Four commands answer four different questions:

| You want to… | Run | It tells you |
|---|---|---|
| Check if rstest is worth adopting (before you commit) | [`rstest try`](../reference/cli-commands.md#try) | Runs your suite under pytest **and** rstest, diffs outcomes, reports the speedup: zero risk |
| Fix tests that fail **only** in parallel after switching | [`rstest migrate-check`](../reference/cli-commands.md#migrate-check) | Onboarding preflight: finds unstable test ids first, then classifies each parallel-only failure (order dependency / isolation leak / timing / unstable id) and names the fix |
| Quarantine the parallel-unsafe tests in one step (**Unreleased**) | [`rstest audit`](../reference/cli-commands.md#audit) | Focused fix loop: same classification, repeatable to catch intermittent races, plus a ready-to-paste `conftest.py` block marking exactly the serial-fixable tests `@pytest.mark.serial` |
| Understand why a passing suite is **slow** | [`rstest --doctor`](../guides/doctor.md) | Plain-English breakdown of where test time goes (wait-bound, a long-pole test, poor parallel balance) |

## A test that isn't parallel-safe

The one thing that can fail after switching to rstest is a test that quietly
depended on running alone. Concretely, two tests writing the **same file**:

```python
# Both tests use the same hard-coded path. Serially they take turns;
# in parallel they clobber each other and one fails intermittently.
def test_writes_config():
    Path("output.json").write_text('{"a": 1}')
    assert json.loads(Path("output.json").read_text())["a"] == 1


def test_writes_other_config():
    Path("output.json").write_text('{"b": 2}')  # same file!
    assert json.loads(Path("output.json").read_text())["b"] == 2
```

Two ways out. **Best**: make them independent with `tmp_path`, pytest's
per-test temp directory, so they never share a file:

```python
def test_writes_config(tmp_path):
    p = tmp_path / "output.json"  # unique dir per test
    p.write_text('{"a": 1}')
    assert json.loads(p.read_text())["a"] == 1
```

**Quick fix**: if you can't fix it right now, mark the offending tests
`serial` so rstest never runs them at the same time as anything else:

```python
import pytest


@pytest.mark.serial  # runs alone, after the parallel tests
def test_writes_config():
    Path("output.json").write_text('{"a": 1}')
    ...
```

`serial` is the pressure valve, not the goal: it removes the speed win for
those tests, so fix the sharing when you can. Not sure which tests are
affected? [`rstest migrate-check`](../reference/cli-commands.md#migrate-check) finds
and classifies them for you. See [Parallel safety](../guides/parallel-safety.md)
for the full catalogue of sharing patterns and fixes.
