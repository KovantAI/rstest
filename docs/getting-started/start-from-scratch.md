# Start from scratch

This is the five-minute path from nothing to a green run, no existing suite
required. If you already have a pytest project, skip to
[Run your existing suite](run-your-suite.md): rstest runs it as-is.

**You need:** Python 3.10+ and a terminal. That's it: no config, no prior
pytest knowledge. New to the terms below (worker, single-worker mode, `-n`)? The
[glossary](../concepts/glossary.md) defines them.

## 1. Set up a folder

```console
$ mkdir rstest-demo && cd rstest-demo
$ python3 -m venv .venv && source .venv/bin/activate
$ pip install rstest
```

On Windows, activate with `.venv\Scripts\activate` instead
([Running on Windows](../guides/windows.md#install)).

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

A one-file suite runs as a single plain pytest session, so the output looks
exactly like pytest's:

```console
$ rstest
============================= test session starts ==============================
platform darwin -- Python 3.13.13, pytest-9.1.1, pluggy-1.6.0
rootdir: /path/to/rstest-demo
collected 2 items

test_first.py ..                                                         [100%]

============================== 2 passed in 0.00s ===============================
```

That's the whole loop: no config file, no flags. `-n auto` (the default)
sized the pool to one worker for one file; a real suite fans out across your
cores ([how `-n auto` sizes the pool](run-your-suite.md#controlling-parallelism)).

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

So far everything ran on one worker: one small file has nothing to
parallelize. Give
rstest real work and it fans out. Drop this in `test_slow.py`:

```python
# test_slow.py
import time
import pytest


@pytest.mark.parametrize("i", range(12))
def test_sleepy(i):
    time.sleep(1)  # pretend each test does real work
```

With one selected file and no timing data yet, `-n auto` would start a
single worker, so ask for four explicitly. Twelve one-second tests then finish in about 3 seconds,
not 12:

```console
$ rstest -n 4 test_slow.py
rstest 0.9.0 — 4 workers (parallel by default; -n 0 for single-worker mode)
............ [100%]

12 passed in 3.16s
```

On a real suite you rarely need `-n`: with many test files, the default
`-n auto` picks a worker count from your cores and the suite's cached timings.
`rstest -v` prefixes each line with the `[gwN]` worker that ran it, and
`rstest --doctor` will tell you where a real suite's time goes.

## Go deeper

- [Run your existing suite](run-your-suite.md): reading the output in depth, selecting
  tests, controlling parallelism
- [Migrating from pytest](../guides/migrate-from-pytest.md): point rstest at
  a real suite; what stays identical and what changes
- [Features](features.md): `--doctor`, `--watch`, `--changed`, and the rest
