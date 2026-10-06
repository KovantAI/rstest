# Run your existing suite

New to rstest and only evaluating? [`rstest try`](evaluating.md#try-it-first)
compares a pytest run and an rstest run of your suite in one command.

Run rstest from your project root, exactly where you would run pytest. This
sample is one run of django-allauth's 2,050-test suite at `-n 4`, the suite
measured in [Benchmarks](../reference/benchmarks.md), which reports 8.4s at
`-n 4`; single runs vary around that (the middle lines are elided):

```console
$ rstest -n 4
rstest 0.8.0 — 4 workers (parallel by default; -n 0 for single-worker mode)
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
rstest 0.8.0 — 2 workers (parallel by default; -n 0 for single-worker mode)
[gw0] ✓ tests/test_math.py::test_add  0.20s [ 25%]
[gw1] ✓ tests/test_math.py::test_add_zero  0.21s [ 50%]
[gw1] s tests/test_math.py::test_skipped [ 75%]
██████████████████████░░░░░░░░  75% (3/4) ~0s left
gw0    0.0s tests/test_math.py::test_add_negative
gw1    idle
```

When the run ends the footer is replaced by the result bar (green, red, and
yellow segments for passed, failed, and skipped or xfail) and pytest's
summary line:

```text
[gw0] ✓ tests/test_math.py::test_add_negative  0.21s [100%]

Results (0.61s):
  ██████████████████████████████ 4/4
3 passed, 1 skipped in 0.61s
```

Both views render the same run (details in [Reading the
output](#reading-the-output)). This page's examples use `dots` for
stability.

No other arguments needed: rstest honors your project's pytest configuration
(`pytest.toml`, `.pytest.toml`, `pytest.ini`, `.pytest.ini`, `pyproject.toml`,
`tox.ini`, or `setup.cfg`, including
`testpaths`, `addopts`, `python_files`, and markers) because collection
runs through a vendored pytest core.

## Reading the output

On an interactive terminal the default style is **`bar`**: a
pytest-sugar-style view (a `✓`/`✗` line per test, inline failures, a live
progress bar). When output is piped or running in CI it falls back to the
compact **`dots`** style shown above, so logs stay stable. Pick a style
explicitly with `--output`: `dots`, `verbose` or `bar` for terminals,
`github`, `gitlab`, `buildkite`, `teamcity` or `azure` for CI annotations,
`tap` or `json` for machine-readable streams (see the
[CLI reference](../reference/cli.md)). The rest of this page describes `dots`. On a single worker with no
`--output` set, rstest prints pytest's own terminal output instead
(see [below](#controlling-parallelism)).

- The **header line** states the worker count. rstest is parallel by
  default; this line is the visible reminder.
- **Dots** stream live as tests finish across all workers: `.` pass, `F`
  fail, `s` skip, `x` xfail, `X` xpass, `E` error, pytest's vocabulary.
- **Failures** print with full pytest-style tracebacks (assertion rewriting
  included) and captured stdout/stderr/log sections, in the `--tb` style you
  pass: `--tb=line` prints pytest's one `path:line: message` line per failure
  (after its captured output, as pytest does) and `--tb=no` prints no
  failure block at all.
- The **summary line** uses pytest's accounting: the counts match what
  pytest would print for the same run, in pytest's order, including
  warnings, the `deselected` count, and a teardown error counted as an error
  on top of the test's call outcome.

The live footer described above belongs to the `bar` view on a terminal;
it is disabled automatically when output is piped, in CI (a `CI` variable,
even on a pty), and when color is off (`NO_COLOR`, `--color=no`,
`TERM=dumb`), which then means no escape sequences at all. It is what
makes long-running tests visible the moment they start, not after they
finish.

Add `-v` for pytest's classic `PASSED`/`FAILED` lines. In parallel mode each line is prefixed with
the worker that ran it (`gw0`, `gw1`, ...), and lines interleave as workers
finish:

```console
$ rstest -n 2 -v
rstest 0.8.0 — 2 workers (parallel by default; -n 0 for single-worker mode)
[gw0] tests/test_math.py::test_add PASSED [ 16%]
[gw1] tests/test_math.py::test_add_zero PASSED [ 33%]
[gw1] tests/test_math.py::test_skipped SKIPPED [ 50%]
[gw0] tests/test_math.py::test_add_negative PASSED [ 66%]
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

At `-n 0`/`-n 1` there is no worker, so there is no prefix: the single
pytest session prints its own `-v` output, exactly as pytest does
(`--output verbose` gives you rstest's own `verbose` view instead):

```console
$ rstest -n 0 -v
============================= test session starts ==============================
platform darwin -- Python 3.13.13, pytest-9.1.1, pluggy-1.6.0 -- /path/to/.venv/bin/python
cachedir: .pytest_cache
rootdir: /path/to/project
collecting ... collected 6 items

tests/test_math.py::test_add PASSED                                      [ 16%]
tests/test_math.py::test_add_zero PASSED                                 [ 33%]
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
```

rstest adds selectors of its own (not pytest flags):

```console
$ rstest --changed                      # only tests affected by your edits
$ rstest --since-green                  # only what changed since the last all-green run
$ rstest --incremental                  # skip tests that passed and whose code is unchanged
```

`--changed` runs just the tests a change can reach, using the per-test
coverage index when it is warm and the import graph otherwise; for gating CI
use [`--changed-strict`](../reference/cli.md#-changed-strict), which runs
everything when it can't connect a change. `--since-green` does the same
against the commit of the last all-passing run. Both need a git checkout.
[`--incremental`](../reference/cli.md#-incremental) needs no git but a warm
coverage index. More in [Selecting changed tests](../guides/changed.md);
see [Watch mode](../guides/watch-mode.md) for the on-save version.

## Controlling parallelism

```console
$ rstest -n 4      # four workers
$ rstest -n auto   # the default: logical cores, capped for small suites
$ rstest -n 0      # single-worker mode: one pytest session (same as -n 1)
$ rstest -n 1      # identical to -n 0
```

How `-n auto` sizes the pool: it starts from your logical core count and
never starts more workers than the work you selected. On a cold cache that
means one worker per selected test file (one selected test runs one
session). Once timings are cached it counts tests instead of files, and also
caps at about one worker per 2 seconds of cached test time, since worker
startup isn't worth it for a sub-second run. So a tiny suite runs on one or
two workers while a slow single file still spreads across workers. It only
ever caps downward; pass an explicit `-n` to override.

`-n 0` and `-n 1` are the compatibility escape hatch, **single-worker mode**:
one pytest session with pytest's own behavior and output
([what it guarantees](../guides/migrate-from-pytest.md#the-escape-hatch)).

Commit your defaults to `[tool.rstest]` in `pyproject.toml` so you don't
retype flags:

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
    suites. Reach for `-n 0` deliberately when you want pytest's exact
    behavior: reproducing a difference from pytest, or running a suite
    whose tests depend on order across files (if the dependency is only
    within a file, `--dist loadfile` keeps each file on one worker).
    Above ~10s, let `-n auto` parallelize. The win grows with suite size
    and is largest for wait-heavy suites: run [`--doctor`](../guides/doctor.md) if you're unsure where your time goes.

## Two runs make it faster

rstest records per-test durations in `.rstest_cache/`. From the second run
on, the scheduler starts your slowest tests first, which is what keeps
workers busy at the end of the run instead of waiting on one long test.
On wait-heavy suites this is dramatic: aiohttp's suite more than halves
between its cold and warm runs (150s to 67s; see [Benchmarks](../reference/benchmarks.md)).

## When something fails

```console
$ rstest --lf        # rerun just the failures
$ rstest --doctor    # and if the suite feels slow, ask why
```

!!! tip "Coming from pytest or pytest-xdist?"
    If tests fail *only* under parallelism on a freshly migrated suite, run
    [`rstest migrate-check`](../reference/cli-commands.md#migrate-check) first: it
    classifies each parallel-only failure (for example order dependency,
    wall-clock timing, or an isolation leak, where one test leaves global
    state behind that breaks a later test on the same worker) and names the
    fix, so you don't triage by hand. See [Migrating from pytest](../guides/migrate-from-pytest.md#the-migrate-check-preflight).

## A test that isn't parallel-safe

The one thing that can fail after switching to rstest is a test that quietly
depended on running alone. Concretely, two tests writing the **same file**:

```python
import json
from pathlib import Path


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
import json


def test_writes_config(tmp_path):
    p = tmp_path / "output.json"  # unique dir per test
    p.write_text('{"a": 1}')
    assert json.loads(p.read_text())["a"] == 1
```

**Quick fix**: if you can't fix it right now, mark the offending tests
`serial` so rstest never runs them at the same time as anything else:

```python
from pathlib import Path

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

## Session fixtures run once per worker

The other common surprise: a `scope="session"` fixture runs once **per
worker**, not once per run (the same as pytest-xdist). A session fixture
that creates a fixed file collides across workers:

```python
import sqlite3

import pytest


@pytest.fixture(scope="session")
def db():
    conn = sqlite3.connect("test.db")  # every worker opens the same file
    yield conn
    conn.close()
```

Give each worker its own copy. `tmp_path_factory` is already per worker
under rstest, so this is the simplest fix:

```python
@pytest.fixture(scope="session")
def db(tmp_path_factory):
    conn = sqlite3.connect(tmp_path_factory.mktemp("db") / "test.db")
    yield conn
    conn.close()
```

For a resource outside the temp directory (a database name, a port), key it
on the `worker_id` fixture: `gw0`, `gw1`, ... at `-n ≥ 2`, and `"master"`
below `-n 2`, where there is only one process. See
[Parallel safety](../guides/parallel-safety.md#session-scoped-fixtures-duplicate).
