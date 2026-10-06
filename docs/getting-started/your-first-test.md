# Start from scratch

This is the five-minute path from nothing to a green run, no existing suite
required. If you already have a pytest project, skip to
[Run your existing suite](first-steps.md): rstest runs it as-is.

**You need:** Python 3.10+ and a terminal. That's it: no config, no prior
pytest knowledge. New to the terms below (worker, byte-exact mode, `-n`)? The
[glossary](../concepts/glossary.md) defines them.

## 1. Set up a folder

```console
$ mkdir rstest-demo && cd rstest-demo
$ python3 -m venv .venv && source .venv/bin/activate
$ # Windows: python -m venv .venv, then .venv\Scripts\activate
$ pip install rstest
```

rstest discovers the interpreter from the active virtualenv, so activating
`.venv` is all the configuration this needs.

## 2. Write a test

Create `test_first.py`; pytest's naming rules apply, so a `test_*.py` file
with `test_*` functions is collected automatically:

```python
# test_first.py
def add(a, b):
    return a + b


def test_add():
    assert add(2, 3) == 5


def test_add_negative():
    assert add(-1, -1) == -2
```

## 3. Run it

```console
$ rstest
============================= test session starts ==============================
platform darwin -- Python 3.13.13, pytest-9.1.1, pluggy-1.6.0
rootdir: /path/to/rstest-demo
collected 2 items

test_first.py ..                                                         [100%]

============================== 2 passed in 0.00s ===============================
```

That's the whole loop: no config file, no flags. rstest collected both tests,
ran them, and printed pytest's familiar output. It *is* pytest's output: this
tiny suite runs on a **single worker**, and on one worker rstest runs one
plain pytest session and lets it print its own terminal output (pass
`--output dots` to get rstest's own `dots` view instead).
It runs on one worker because `-n auto` (the default) deliberately caps
itself low on tiny suites: it never starts more workers than you have test
files, and once rstest has timings cached it also caps by how long the suite takes, since
worker startup isn't worth it for a sub-second run. On a real suite with many
files, the same command fans out across your cores; rstest is [parallel by
default](features.md). Force a worker count any time with `-n`, e.g.
`rstest -n 4`.

## 4. See a failure

Failures are where a runner earns its keep. Append a broken test to the end
of `test_first.py`:

```python
def test_add_wrong():
    assert add(2, 2) == 5
```

```console
$ rstest
============================= test session starts ==============================
platform darwin -- Python 3.13.13, pytest-9.1.1, pluggy-1.6.0
rootdir: /path/to/rstest-demo
collected 3 items

test_first.py ..F                                                        [100%]

=================================== FAILURES ===================================
________________________________ test_add_wrong ________________________________

    def test_add_wrong():
>       assert add(2, 2) == 5
E       assert 4 == 5
E        +  where 4 = add(2, 2)

test_first.py:15: AssertionError
=========================== short test summary info ============================
FAILED test_first.py::test_add_wrong - assert 4 == 5
========================= 1 failed, 2 passed in 0.01s ==========================
```

Full pytest tracebacks, assertion rewriting included: on one worker this is
exactly what pytest prints. (Across multiple workers rstest renders the
output itself, and each failure header also carries the `[gwN]` worker that
hit it.)
Rerun just the failure while you fix it:

```console
$ rstest --lf          # --last-failed: only the tests that failed last run
```

## 5. Watch it go parallel

Two tests stayed single-worker because there's nothing to parallelize. Give
rstest real work and it fans out. Drop this in `test_slow.py`:

```python
# test_slow.py
import time
import pytest


@pytest.mark.parametrize("i", range(12))
def test_sleepy(i):
    time.sleep(1)  # pretend each test does real work
```

This demo folder has only two test files, so `-n auto` would start at most
two workers: it never starts more workers than test files, and once timings
are cached it can go lower on a suite that takes only a few seconds. Ask for
four explicitly. Twelve one-second tests then finish in about
3 seconds, not 12:

```console
$ rstest -n 4 test_slow.py
rstest 0.8.0 — 4 workers (parallel by default; -n 0 for single-worker mode)
............ [100%]

12 passed in 3.16s
```

On a real suite you rarely need `-n`: with many test files, the default
`-n auto` picks a worker count from your cores and the suite's cached timings.
`rstest -v` prefixes each line with the `[gwN]` worker that ran it, and
`rstest --doctor` will tell you where a real suite's time goes.

## Go deeper

- [Run your existing suite](first-steps.md): reading the output in depth, selecting
  tests, controlling parallelism
- [Migrating from pytest](../guides/migrate-from-pytest.md): point rstest at
  a real suite; what stays identical and what changes
- [Features](features.md): `--doctor`, `--watch`, `--changed`, and the rest
