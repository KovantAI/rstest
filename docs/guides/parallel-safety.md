# Parallel safety

Most test suites were written under a serial runner and contain hidden
assumptions: a fixed port, a shared temp file, a rate-limit window, an
order dependency. Under any parallel runner (rstest or pytest-xdist)
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
other worker's session has fully finished, fixtures torn down, ports and
databases released. The marker is registered automatically (no
`--strict-markers` complaints).

Use it for: tests binding fixed ports, tests asserting on global process
state, tests measuring wall-clock timing tightly.

Serial tests run in the **designated worker's own session**, not a fresh one:
they reuse whatever session/module-scoped fixtures that worker already built
during its parallel phase (one instance, on that worker, not a merge of all
workers' fixtures). So a serial test depending on a session fixture gets a
normally-constructed one; just don't expect it to see state another worker's
copy of that fixture accumulated.

## File affinity

```console
$ rstest --dist loadfile
```

`loadfile` keeps each file's tests on one worker, in file order: the
standard remedy for suites where tests within a file depend on each other.
`--dist loadscope` (class/module affinity) and `--dist loadgroup`
(`@pytest.mark.xdist_group` affinity across files) are finer-grained
variants for expensive shared fixtures. All are xdist-compatible.
The default (`--dist load`) distributes at test granularity, which
balances better and splits slow files across workers. On a large suite with
a warm cache, rstest may pick [lazy collection](../concepts/lazy-collection.md)
automatically (under `--dist load` or `loadfile` only, for a whole-suite run;
see the [full rules](../concepts/lazy-collection.md#auto-default)); files then
run whole on one worker, in file order, and auto only makes that pick when no
single file would hold up the run. Pass
`--collect full` to keep test-granular dispatch.

## Choosing the worker count

`-n auto` is the safe default. It never exceeds your logical cores, and caps
further by file count and cached runtime, so it will not oversubscribe. Two
reasons to override it with an explicit `-n`, in opposite directions:

- **Wait-bound suites want *more* workers than cores.** When tests mostly
  wait (IO, network, sleeps, timeouts) a worker holds no core while it waits,
  so running `-n` above the core count overlaps more waits and cuts wall time.
  `auto` will not do this for you. The [wait-bound playbook](wait-bound.md)
  covers how to find the sweet spot.
- **Load-sensitive suites want fewer.** Tests asserting on timing or shared
  machine state (see [below](#time-sensitive-tests-at-high-concurrency)) can
  need `-n` capped to stay green under a busy machine.

An explicit `-n <k>` is exact: the `auto` caps do not apply.

## Session-scoped fixtures duplicate

A session-scoped fixture runs **once per worker**, not once per run: N
workers means N databases, N servers, N expensive setups. This is identical
to xdist semantics. Two consequences:

- The fixture must be safe to duplicate: unique ports (bind port 0),
  per-worker database names, per-worker directories.
- For pytest-django users this already works: rstest announces itself
  exactly like an xdist worker (`gw0`, `gw1`, ...), so the test database
  per worker is suffixed automatically.

`rstest --doctor` flags a session fixture that ran more than once with this
exact caveat, but only on rows of its FIXTURE HOTSPOTS table (fixtures with at
least 0.5s of total setup time, top 8). A cheap session fixture that runs per
worker gets no warning, so audit those by hand.

### Teardown timing and `--setup-show` / `--setup-plan`

Each worker finalizes its own session-scoped fixtures, but not as soon as
it runs out of tests. Idle workers stay connected until the **whole parallel
phase** is resolved (every test has a final outcome, including any
`--reruns` retries, which may land on any worker). rstest then sends every
worker the end-of-session signal together, and each runs its session
teardown. So a session fixture (a DB connection pool, a server) stays alive
on an idle worker until the slowest worker finishes. Ordering within a worker
is pytest's usual reverse-of-setup; there is no ordering across workers.
If the run has `@pytest.mark.serial` tests, the other workers tear down
first, and the designated worker keeps its session open to run the serial
phase afterwards. rstest has no hook that runs once after *all* workers
finish: [`pytest_testnodedown`](../concepts/xdist-hooks.md) fires once per
worker, inside that worker. So make every resource per-worker (keyed on
`node.workerinput` or the worker id) and let each worker clean up its own,
in a session fixture's teardown or in `pytest_testnodedown`.

`--setup-show` and `--setup-plan` are **not** passthrough-IO flags, so at
`-n ≥ 2` they run in the parallel pool, where they don't work: `--setup-show`
prints no setup/teardown trace at all, and `--setup-plan` reports every test
as an error while still exiting `0`. Run them at `-n 0`, where they behave
exactly as under pytest:

```console
$ rstest -n 0 --setup-plan        # one worker, one readable plan
```

## Worker identity

Tests and fixtures can read the worker they run on:

```python
import os

worker = os.environ.get("RSTEST_WORKER_ID")  # "gw0", ... ; unset at -n 0 or -n 1
```

Exception: `-n 0/1` with `--reruns` runs a one-worker pool, so there the
variable is set to `gw0`. The xdist-compatible `worker_id` fixture does not
have this wrinkle: it returns `gw0`, `gw1`, ... at `-n ≥ 2` and `"master"`
below `-n 2` (with or without `--reruns`), so prefer it inside fixtures:

```python
import pytest


@pytest.fixture(scope="session")
def worker_suffix(worker_id):
    return worker_id  # "gw0", "gw1", ... or "master" below -n 2
```

Plugins that check xdist's `workerinput` get the same answer: the
attribute is provided for compatibility.

### A worked non-Django example

pytest-django gets per-worker databases for free. The same pattern works for
any backend: derive the resource name or port from the worker id in a
session-scoped fixture, which runs once per worker. Raw SQLAlchemy / psycopg
against a per-worker database:

```python
import pytest
from sqlalchemy import create_engine


@pytest.fixture(scope="session")
def db_engine(worker_id):
    # gw0, gw1, ...; "master" below -n 2, where there is a single session
    url = f"postgresql+psycopg://ci:ci@localhost:5432/app_{worker_id}"
    # create the database `app_{worker}` if it does not exist, then:
    engine = create_engine(url)
    yield engine
    engine.dispose()
```

Each worker gets its own `app_gw0`, `app_gw1`, ... database, so nothing
collides (`app_master` below `-n 2`). rstest exercises exactly this shape in its own battery with
pytest-postgresql, where each worker spins up its own server on an
OS-assigned free port.

Testcontainers follows the same rule, one container per worker:

```python
import pytest
from testcontainers.postgres import PostgresContainer


@pytest.fixture(scope="session")
def pg_url():
    # Session scope runs this once per worker, so each worker gets its own
    # container on its own random host port. Nothing needs the worker id here
    # because the container assigns the port; reach for RSTEST_WORKER_ID only
    # when you name a shared external resource that would otherwise collide.
    with PostgresContainer("postgres:16") as container:
        yield container.get_connection_url()
```

The rule generalizes: any fixed name or port a serial suite hard-coded (a
database, a schema, a bound port, a temp directory) must become per-worker
under parallelism. Bind port `0` to let the OS assign one, or key the name on
`RSTEST_WORKER_ID`.

## Time-sensitive tests at high concurrency

A class of tests passes at `-n 4` and flakes at `-n 16`: anything
asserting on rate-limit windows, token expiries, or elapsed time degrades
when the machine is oversubscribed. This is load, not ordering: `--dist
loadfile` will not fix it.

Containment options, in order of preference:

1. Fix the test (mock the clock; widen the window).
2. Mark it `@pytest.mark.serial`.
3. Cap concurrency for the suite: `rstest -n 4`.
4. As a stopgap, `--reruns 2` (works at any `-n`; at `-n 0/1` it runs a
   one-worker pool):
   failures that pass on retry are reported flaky (visible, counted, but
   not red). Prefer fixing: reruns hide real intermittent bugs as easily
   as test smells.

## Numeric determinism (ML / numerics suites)

Numerics suites assert on exact (or tightly-toleranced) float values, so "same
inputs → same bits, every run" has to survive parallelism. rstest does not make
your numbers nondeterministic, but parallel execution can *expose* four things a
serial run hides. All four are your test's contract to hold: rstest gives you
the tools to hold it.

**The floor: `-n 0` is bit-for-bit pytest.** Single-worker
[byte-exact mode](../concepts/glossary.md#byte-exact-mode) is one pytest
session in one worker process, running the same vendored pytest code as a
plain pytest run. If a value matches under `pytest`
it matches under `rstest -n 0`. Any divergence there is a bug. Use it as the
determinism baseline to diff against when a parallel run disagrees.

**1. RNG seeding is per-worker, and yours to set.** Each worker is a separate
process; a seed set in one does not reach another. rstest does **not** seed
`numpy`/`torch`/`random` for you (neither does pytest), but it gives every
worker a stable identity to derive a reproducible seed from
([Worker identity](#worker-identity)), plus one run-level uid all workers agree
on. Seed deterministically in a fixture:

```python
import numpy as np
import pytest


@pytest.fixture(autouse=True)
def seed_numpy(worker_id):
    # Same seed every run; distinct per worker so workers don't draw identical
    # streams. Drop the worker offset if you want every worker identical.
    worker = 0 if worker_id == "master" else int(worker_id.removeprefix("gw"))
    np.random.seed(1234 + worker)
```

A test that depends on a seed set by an *earlier* test in the same process is
order-dependent (see point 4), not seeded: fix it to seed itself.

**2. Thread oversubscription can change the last bits.** Float addition isn't
associative, so a reduction's bit pattern depends on how it's split across
threads. numpy/torch/BLAS spin their **own** thread pools; rstest does **not**
pin them. At `-n auto` you get *workers × library-threads* threads competing for
the cores, and the changed reduction order can shift low bits: a tight
`assert x == expected` passes at `-n 0` and flips at `-n 8`. Pin the math
libraries to one thread per worker and let rstest own the parallelism:

```console
$ OMP_NUM_THREADS=1 MKL_NUM_THREADS=1 OPENBLAS_NUM_THREADS=1 \
    VECLIB_MAXIMUM_THREADS=1 rstest -n auto
```

(Or cap `-n` to leave headroom for the internal threads.) Pin for
determinism, not for speed: on numpy with Accelerate the cap made no
difference on a realistic suite, and on heavy linear algebra it was slower
below the core count
([measured](https://github.com/KovantAI/rstest/tree/main/examples/cpu-bench#worker-x-blas-thread-grid)).
`VECLIB_MAXIMUM_THREADS` is for macOS: Accelerate ignores the other three.

**3. Reset global numeric state per test.** `np.seterr`, `torch.set_default_dtype`,
the global RNG, `np.set_printoptions`: a test that mutates one and a test that
assumes the default pass in serial order and disagree when reordered or split
across workers. Contain state in fixtures (set-and-restore) so each test starts
from a known configuration; this is the general isolation
rule behind [the serial escape hatch](#the-serial-escape-hatch), but for numerics the symptom is a
*wrong number*, not a crash.

**4. Order sensitivity.** Duration-aware scheduling runs tests in timing order,
not file order. A numeric group that only holds when run in sequence (shared
warmup, incremental fixtures) needs its order pinned: keep it on one worker with
`--dist loadfile` / `loadscope`, mark it `@pytest.mark.serial`, or run the whole
suite at `-n 0`. If your results also depend on hash ordering, note rstest does
not set `PYTHONHASHSEED`: pin it yourself (`PYTHONHASHSEED=0`) as you would
under pytest.

If a value differs between `-n 0` and a parallel run, it is one of the four
above: start by diffing against the `-n 0` baseline, then check thread pinning
(2) and per-test state (3) first, as those are the usual culprits for numerics.

## Diagnosing a parallel-only failure

```console
$ rstest -n 0 path/to/test.py::test_flaky   # passes? not the test itself
$ rstest --dist loadfile                    # passes? order dependency
$ rstest -n 2                               # passes? load sensitivity
```

Three runs usually classify the failure. Order dependencies want
`loadfile` or a refactor; load sensitivity wants `serial` or a clock mock;
anything failing at `-n 0` too is a plain bug.

`rstest migrate-check` runs equivalent discriminators **for you**, over the
whole suite and scoped to the files that actually fail: serial runs (twice)
and a `--dist loadfile` run, with load sensitivity inferred from wall time
far exceeding CPU time rather than from a separate `-n 2` run. It classifies
each failure into the classes above, and bisects the polluting file for order /
isolation defects (the first 3; use `rstest bisect` for the rest). Reach for it instead of running the three commands by hand;
see [The migrate-check preflight](migrate-from-pytest.md#the-migrate-check-preflight).

For an order-dependent suite, three more tools go from "it flakes sometimes"
to a fix:

- [`--shuffle`](../reference/cli.md#-shuffleseed) runs the suite in a seeded
  random order to flush order dependence out on demand; the seed is printed,
  and `--shuffle=SEED` replays a failing order.
- [`rstest bisect <nodeid>`](../reference/cli-commands.md#bisect-nodeid) finds
  the test that pollutes a victim and prints a minimal repro command.
- [`rstest audit`](../reference/cli-commands.md#audit) runs the suite in
  parallel against a serial baseline and classifies each parallel-only
  failure. The ones serial fixes (isolation, wall-clock) come out as a
  ready-to-paste `@pytest.mark.serial` list; order-dependent tests are pointed
  at `--dist loadfile`, and intrinsic flakes and inconclusive ones are listed
  separately.

If the failure only shows up on CI, don't try to recreate the schedule by
hand: upload the run's replay journal and re-run that exact schedule locally
with `rstest replay`. See
[Replaying a CI-only failure locally](ci-quickstart.md#replaying-a-ci-only-failure-locally).

## Worked examples

[Parity divergences & upstream fixes](../reference/parity-divergences.md)
catalogues every real divergence found running rstest against well-known
public suites (requests, pydantic, typer, rich, httpx, werkzeug, …), each with
its root cause and the concrete upstream change that removes it: a practical
checklist for reaching exact parity under any parallel runner.
