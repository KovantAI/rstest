# Your first test

This is the five-minute path from nothing to a green run — no existing suite
required. If you already have a pytest project, skip to
[First steps](first-steps.md): rstest runs it as-is.

**You need:** Python 3.10+ and a terminal. That's it — no config, no prior
pytest knowledge. New to the terms below (worker, byte-exact, `-n`)? The
[glossary](../concepts/glossary.md) defines them.

## 1. Set up a folder

```console
$ mkdir rstest-demo && cd rstest-demo
$ python -m venv .venv && source .venv/bin/activate
$ pip install rstest
```

rstest discovers the interpreter from the active virtualenv, so activating
`.venv` is all the configuration this needs. (Windows: `.venv\Scripts\activate`)

## 2. Write a test

Create `test_first.py` — pytest's naming rules apply, so a `test_*.py` file
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
rstest 0.4.0 — single worker (pytest-exact mode)
..

2 passed in 0.11s
```

That's the whole loop: no config file, no flags. rstest collected both tests,
ran them, and printed pytest's familiar summary. The header says **single
worker** here because `-n auto` (the default) deliberately caps itself low on
tiny suites — worker startup isn't worth it for two tests. On a real suite the
same command fans out across every core; rstest is [parallel by
default](features.md). Force a worker count any time with `-n`, e.g.
`rstest -n 4`.

## 4. See a failure

Failures are where a runner earns its keep. Add a broken test:

```python
def test_add_wrong():
    assert add(2, 2) == 5
```

```console
$ rstest
rstest 0.4.0 — single worker (pytest-exact mode)
..F

--- FAILED test_first.py::test_add_wrong ---
def test_add_wrong():
>       assert add(2, 2) == 5
E       assert 4 == 5
E        +  where 4 = add(2, 2)

test_first.py:15: AssertionError

1 failed, 2 passed in 0.14s
```

Full pytest-style tracebacks, assertion rewriting included — identical to what
pytest prints. (Across multiple workers each failure header also carries the
`[gwN]` worker that hit it; with one worker there's nothing to attribute.)
Rerun just the failure while you fix it:

```console
$ rstest --lf          # --last-failed: only the tests that failed last run
```

## 5. Watch it go parallel

Two tests stayed single-worker — there's nothing to parallelize. Give rstest
real work and it fans out across your cores. Drop this in `test_slow.py`:

```python
# test_slow.py
import time
import pytest


@pytest.mark.parametrize("i", range(12))
def test_sleepy(i):
    time.sleep(1)  # pretend each test does real work
```

Run it. Twelve one-second tests finish in about `12 / cores` seconds, not 12:

```console
$ rstest test_slow.py
rstest 0.4.0 — 8 workers (parallel by default; -n 0 for single-worker mode)
............

12 passed in 1.68s
```

That's the whole point: the same `rstest` command that ran two tests serially
just spread twelve across every core, no flags changed. `rstest -v` prefixes
each line with the `[gwN]` worker that ran it, and `rstest --doctor` will tell
you where a real suite's time goes.

## Where to next

- [First steps](first-steps.md) — reading the output in depth, selecting
  tests, controlling parallelism
- [Migrating from pytest](../guides/migrate-from-pytest.md) — point rstest at
  a real suite; what stays identical and what changes
- [Features](features.md) — `--doctor`, `--watch`, `--changed`, and the rest
