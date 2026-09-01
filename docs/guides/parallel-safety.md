# Parallel safety

Most test suites were written under a serial runner and contain hidden
assumptions: a fixed port, a shared temp file, a rate-limit window, an
order dependency. Under any parallel runner — rstest or pytest-xdist —
those assumptions surface as confusing failures. rstest ships rails for
every class of them.

## The serial escape hatch

```python
import pytest


@pytest.mark.serial
def test_rebinds_the_global_port(): ...
```

`@pytest.mark.serial` tests are excluded from the parallel phase entirely.
They run **exclusively**: on a single designated worker, only after every
other worker's session has fully finished — fixtures torn down, ports and
databases released. The marker is registered automatically (no
`--strict-markers` complaints).

Use it for: tests binding fixed ports, tests asserting on global process
state, tests measuring wall-clock timing tightly.

Serial tests run in the **designated worker's own session**, not a fresh one:
they reuse whatever session/module-scoped fixtures that worker already built
during its parallel phase (one instance, on that worker — not a merge of all
workers' fixtures). So a serial test depending on a session fixture gets a
normally-constructed one; just don't expect it to see state another worker's
copy of that fixture accumulated.

## File affinity

```console
$ rstest --dist loadfile
```

`loadfile` keeps each file's tests on one worker, in file order — the
standard remedy for suites where tests within a file depend on each other.
`--dist loadscope` (class/module affinity) and `--dist loadgroup`
(`@pytest.mark.xdist_group` affinity across files) are finer-grained
variants for expensive shared fixtures. All are xdist-compatible.
The default (`--dist load`) distributes at test granularity, which
balances better and splits slow files across workers.

## Session-scoped fixtures duplicate

A session-scoped fixture runs **once per worker**, not once per run — N
workers means N databases, N servers, N expensive setups. This is identical
to xdist semantics. Two consequences:

- The fixture must be safe to duplicate: unique ports (bind port 0),
  per-worker database names, per-worker directories.
- For pytest-django users this already works: rstest announces itself
  exactly like an xdist worker (`gw0`, `gw1`, ...), so the test database
  per worker is suffixed automatically.

`rstest --doctor` prints a warning for every session fixture that ran more
than once, with this exact caveat.

### Teardown timing and `--setup-show` / `--setup-plan`

Each worker finalizes its own fixtures at **its own** session end — a
session-scoped fixture's teardown runs once that worker has finished its
last test, not when the whole run ends. Ordering within a worker is
pytest's usual reverse-of-setup; there is no cross-worker teardown
ordering, since workers finish independently. (Cleanup that must run after
*every* worker — dropping a shared DB — belongs in a
[`pytest_testnodedown`-style hook](../concepts/xdist-hooks.md), not a
session fixture.)

`--setup-show` and `--setup-plan` are **not** passthrough-IO flags, so they
run in the parallel pool: each worker prints its own setup/teardown trace,
so the output is duplicated and interleaved across workers. For a single
clean fixture plan, run them at `-n 0`:

```console
$ rstest -n 0 --setup-plan        # one worker, one readable plan
```

## Worker identity

Tests and fixtures can read the worker they run on:

```python
import os

worker = os.environ.get("RSTEST_WORKER_ID")  # "gw0", ... ; unset at -n 0 or -n 1
```

Plugins that check xdist's `workerinput` get the same answer — the
attribute is provided for compatibility.

## Time-sensitive tests at high concurrency

A class of tests passes at `-n 4` and flakes at `-n 16`: anything
asserting on rate-limit windows, token expiries, or elapsed time degrades
when the machine is oversubscribed. This is load, not ordering — `--dist
loadfile` will not fix it.

Containment options, in order of preference:

1. Fix the test (mock the clock; widen the window).
2. Mark it `@pytest.mark.serial`.
3. Cap concurrency for the suite: `rstest -n 4`.
4. As a stopgap, `--reruns 2` (needs `-n ≥ 2`; ignored at `-n 0/1`):
   failures that pass on retry are reported flaky (visible, counted, but
   not red). Prefer fixing — reruns hide real intermittent bugs as easily
   as test smells.

## Diagnosing a parallel-only failure

```console
$ rstest -n 0 path/to/test.py::test_flaky   # passes? not the test itself
$ rstest --dist loadfile                    # passes? order dependency
$ rstest -n 2                               # passes? load sensitivity
```

Three runs usually classify the failure. Order dependencies want
`loadfile` or a refactor; load sensitivity wants `serial` or a clock mock;
anything failing at `-n 0` too is a plain bug.

`rstest --migrate-check` runs exactly these discriminators **for you** — over
the whole suite, scoped to the files that actually fail — classifies each
failure into the classes above, and bisects the polluting file for order /
isolation defects. Reach for it instead of running the three commands by hand;
see [The migrate-check preflight](migrate-from-pytest.md#the-migrate-check-preflight).

## Worked examples

[Parity divergences & upstream fixes](../reference/parity-divergences.md)
catalogues every real divergence found running rstest against well-known
public suites (requests, pydantic, typer, rich, httpx, werkzeug, …), each with
its root cause and the concrete upstream change that removes it — a practical
checklist for making a suite byte-exact under any parallel runner.
