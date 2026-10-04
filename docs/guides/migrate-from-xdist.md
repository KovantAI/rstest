# Migrating from pytest-xdist

rstest replaces pytest-xdist rather than wrapping it: parallelism is
native, and the worker environment is xdist-shaped on purpose so plugins
keep working.

## Flag map

Most xdist flags carry over unchanged. The ones people actually touch:

- **`-n 4` / `-n auto`**: same, and `auto` is the default. `auto` is capped by
  test-file count and cached suite time, so pass an explicit `-n` when you
  need a fixed count (for example with `--shard`). `auto` also counts
  differently: rstest starts from **logical** cores, while xdist's `auto`
  counts **physical** cores when psutil is installed (logical otherwise). On
  a machine with SMT or hyperthreading, rstest's `auto` can start twice as
  many workers. Pin `-n` to the count your xdist job actually used while you
  shadow-run both, so timing and load differences are not a worker-count
  difference.
- **`-n logical`**: not supported (exit 1). Use `-n auto` or an explicit `-n N`.
- **`--dist load` / `loadfile` / `loadscope` / `loadgroup`**: same names and
  semantics, including `@pytest.mark.xdist_group`. `load` (the default) adds
  duration-aware slowest-first scheduling. Pass the mode on the rstest command
  line or set `[tool.rstest] dist`: rstest does not read `--dist` from
  `addopts` (see [below](#if-xdist-is-still-in-your-ini)).
- **Collection** (no xdist equivalent): xdist always has every worker collect
  the whole suite. rstest does the same by default, but on a large suite with
  a warm cache (at least 2000 cached tests and `tests × workers` of at least
  16 000, under `--dist load` or `loadfile` only; see the
  [full rules](../concepts/lazy-collection.md#auto-default)) it switches to
  [lazy collection](../concepts/lazy-collection.md):
  each file is collected once, on one worker, and runs whole there. Conftest
  collection hooks then see only that worker's files. Pin `--collect full`
  (or `[tool.rstest] collect = "full"`) to keep xdist's collection model
  exactly.
- **`-n 1`**: differs. xdist's `-n 1` is one `gw0` worker with `workerinput`;
  rstest's `-n 1`, like `-n 0`, is
  [byte-exact mode](../concepts/glossary.md#byte-exact-mode), with no worker identity.
- **`--dist no`**: rejected (exit 1). Use `-n 0` for a single worker.
- **`--dist worksteal`**: rejected (exit 1, `unknown --dist mode`). Use
  `load`, the default: it already dispatches slowest-first from the duration
  cache.
- **`--dist each`**: supported, partially. Every worker runs the full suite,
  all on the same interpreter (xdist's heterogeneous `--tx` gateways have no
  equivalent), and rstest's `--reruns` is rejected in this mode. See the
  [xdist support matrix](../reference/xdist-support.md#flag-matrix).
- **`--looponfail` / `-f`**: use [`--watch`](watch-mode.md) instead. rstest
  does not handle `--looponfail`; with pytest-xdist installed it reaches every
  worker session, xdist's loop-on-fail mode takes it over, and the run hangs.
  Remove it from `addopts` before switching.
- **`-p no:xdist`**: forwarded to pytest in every worker. rstest does not
  need pytest-xdist, so the run still parallelizes. A leftover
  `addopts = -n 4` then fails: pytest has no `-n` option once xdist is
  disabled, so every run stops with a usage error (exit 4,
  `unrecognized arguments: -n`), the same as uninstalling pytest-xdist.

Everything else (`--tx`, `--rsync*`, `-d`, `--maxprocesses`,
`--max-worker-restart`) is covered row by row,
including what happens to a flag rstest doesn't act on, in the
[xdist support matrix](../reference/xdist-support.md#flag-matrix). In short:
those flags parse but do nothing while pytest-xdist is installed, and are a
pytest usage error (exit 4) once it isn't. Remove them from `addopts` only
once nothing runs pytest-xdist any more: while an old xdist CI job is still
your fallback, it needs them (see
[the staged rollout](migrate-from-pytest.md#rolling-out-in-stages-and-rolling-back)).

pytest-rerunfailures maps: `--reruns N`,
`@pytest.mark.flaky(reruns=N)`, and `--only-rerun REGEX` work natively
in parallel modes (and crash-aware: a test that kills its worker
retries on the replacement). The plugin itself is unregistered inside
pool workers so nothing double-reruns. A command-line `--reruns` is always
rstest's own, at every worker count: at `-n 0/1` it switches to a one-worker
rerun pool, so the plugin is unregistered there too. Only at `-n 0` *without*
rstest's `--reruns` (for example with `--reruns` in `addopts`, or after `--`)
does the plugin keep its native behavior. In the pool, a `--reruns` in
`addopts` does nothing, silently; pass it on the rstest command line instead
([why](migrate-from-pytest.md#addopts-and-pytest_addopts)). rstest's
`--reruns` are rejected under `--dist each` (rstest reruns a failure on
another worker, which has no meaning when every worker runs the full suite;
see
[`--dist each`](../reference/cli.md#-dist-loadloadfileloadscopeloadgroupeach)).

Not carried over from pytest-rerunfailures when rstest owns the retry (the
pool, or any run with rstest's own `--reruns`):

- **Mark keywords `reruns_delay`, `only_rerun`, `rerun_except`**: ignored. A
  test with a non-matching `only_rerun` is still retried. Use the global
  `--only-rerun` to filter by error. (The budget, `reruns=` or positional
  `flaky(3)`, and `condition=` are read as the plugin reads them.)
- **`--reruns-delay` and `--rerun-except`**: not rstest flags, so they are
  forwarded to pytest. With pytest-rerunfailures installed they parse and do
  nothing (no delay, no exception filter); without it they are a pytest usage
  error (exit 4).

Details: [`@pytest.mark.flaky`](../reference/markers.md#pytestmarkflaky).

## What your plugins see

rstest workers announce themselves exactly like xdist workers.
`config.workerinput` carries: `workerid` (`gw0`, `gw1`, ...),
`workercount`, the run uid as `testrunuid` (xdist's key) and `testrun_uid`
(one uid per run, shared by all workers), `mainargv`, and the `cov_master_*`
keys pytest-cov expects. The `PYTEST_XDIST_WORKER`,
`PYTEST_XDIST_WORKER_COUNT` and `PYTEST_XDIST_TESTRUNUID` environment
variables are set too, so plugins and conftests that grep the
environment keep working as-is. Plugins keying per-worker resources on
worker identity work unchanged. The canonical
case is pytest-django's per-worker test database (`test_<name>_gw0`, ...),
which follows from the `workerid` above; note that rstest's corpus only
exercises pytest-django on SQLite `:memory:`, so check a server-backed
database (Postgres, MySQL) on your own suite.

`RSTEST_WORKER_ID` (same `gwN` values) is also set if you want to
detect rstest specifically.

The `worker_id` and `testrun_uid` **fixtures** are provided natively, with
xdist's semantics, so `def test(worker_id): ...` resolves whether or not
pytest-xdist is installed. Removing pytest-xdist from your config keeps them
working (`worker_id` is `"master"` below `-n 2`, `gwN` in the pool). With
pytest-xdist installed, rstest's definitions take precedence over xdist's (a
conftest override still wins), and in the pool both return the same values.
One caveat: `--reruns` at `-n 0/1` runs a one-worker pool where
`config.workerinput` and `PYTEST_XDIST_WORKER=gw0` exist, so xdist's
`get_xdist_worker_id()` / `is_xdist_worker()` report `gw0` while the fixtures
report `"master"`. See the
[xdist support matrix](../reference/xdist-support.md#fixtures-worker-identity).

## Controller-side hooks

xdist's controller-side hooks (`pytest_configure_node`,
`pytest_testnodeready`, `pytest_testnodedown`) are emulated: each worker
plays controller for itself, calling your implementations against a node shim
with its own `workerinput`, `gateway.id`, and `config`. Hooks that are pure
functions of the node (read `gateway.id`, fill `workerinput`, provision a
resource from them, as in SQLAlchemy's `follower_ident` pattern) produce the same
observable result as xdist.

Two things to know if you rely on these hooks: they run **N times
concurrently in N processes** (controller-side shared state needs rework;
derive from `gateway.id` or a uuid), and a crashed worker's
`pytest_testnodedown` runs on a *surviving* worker, so teardown must be a
function of `node.workerinput` alone. Full semantics, timing, and the crash
race: [xdist hook emulation](../concepts/xdist-hooks.md).

These hooks are declared by pytest-xdist. Uninstall it while a conftest or
plugin still implements one (`pytest_configure_node`, `pytest_testnodeready`,
`pytest_testnodedown`, `pytest_xdist_*`) and every run, at any `-n`, stops
with an internal error: `PluginValidationError: unknown hook
'pytest_configure_node'`. Either keep pytest-xdist installed (rstest leaves
its session inert), or mark each implementation optional, which also keeps
it working under rstest's emulation without xdist:

```python
import pytest


@pytest.hookimpl(optionalhook=True)
def pytest_configure_node(node): ...
```

## Controller-only conftest code

xdist runs a controller process next to its workers, and conftest code often
targets it with `if not hasattr(config, "workerinput"):` or
`xdist.is_xdist_controller(session)`. rstest has no controller process: at
`-n ≥ 2` every pytest session is a worker with a `workerinput`, so that
branch **never runs**. (At `-n 0/1` without `--reruns` the single session
has no `workerinput`, so the branch runs there, once.)

Move once-per-run setup into a session fixture that the workers coordinate
through a file lock, the pattern
[xdist documents](https://pytest-xdist.readthedocs.io/en/stable/how-to.html#making-session-scoped-fixtures-execute-only-once)
for the same problem. `tmp_path_factory.getbasetemp().parent` is shared by
every worker in a run, and `testrun_uid` is the same in every worker, so the
first worker produces the data and the rest reuse it:

```python
import json

import pytest
from filelock import FileLock


@pytest.fixture(scope="session")
def session_data(tmp_path_factory, worker_id, testrun_uid):
    if worker_id == "master":  # below -n 2: one session, no coordination
        return produce_expensive_data()
    root = tmp_path_factory.getbasetemp().parent
    fn = root / f"data-{testrun_uid}.json"
    with FileLock(str(fn) + ".lock"):
        if fn.is_file():
            return json.loads(fn.read_text())
        data = produce_expensive_data()
        fn.write_text(json.dumps(data))
    return data
```

There is no equivalent for the other half of the controller's job: nothing
runs once after **all** workers finish. `pytest_sessionfinish` and
`pytest_testnodedown` fire in each worker, for that worker. Make cleanup
per-worker (each worker drops what it created), or run it as a CI step after
rstest exits.

## If xdist is still in your ini

`addopts = -n 4` with pytest-xdist installed is neutralized inside rstest
workers automatically: options parse, the xdist session never engages, no
nested workers. One exception: `rstest --pdb` fails with xdist's
`--pdb is incompatible with distributing tests`, because xdist checks before
rstest neutralizes it, so remove `-n` from `addopts` before debugging with
`--pdb`. Keep it while a pytest-xdist job is still your fallback, then
remove it and pass `-n` to rstest. Uninstall pytest-xdist only after that,
and only once no conftest or plugin implements its hooks (see
[Controller-side hooks](#controller-side-hooks)).
[`rstest xdist-removal-check`](#removing-pytest-xdist) checks all of this for
you.

rstest reads neither `-n` nor `--dist` from `addopts` (or `PYTEST_ADDOPTS`):
its worker count comes from the command line or `[tool.rstest] numprocesses`
(default `auto`), and its mode from `--dist` or `[tool.rstest] dist` (default
`load`). So `addopts = -n 4 --dist loadgroup` gives an `auto`-sized `load`
run, and `@pytest.mark.xdist_group` tests are no longer kept together, with no
warning. Copy the mode when you switch:

```toml
[tool.rstest]
dist = "loadgroup"
```

### Audit the rest of `addopts`

rstest reads its own flags from the command line or `[tool.rstest]`, never
from `addopts` or `PYTEST_ADDOPTS`. Besides `-n` and `--dist` above, check
these before you switch:

- **`--reruns N`**: does nothing in the pool, silently. Pass it on the rstest
  command line or set `[tool.rstest] reruns = N`
  ([why](migrate-from-pytest.md#addopts-and-pytest_addopts)).
- **`--cov=...` / `--cov-report=...`**: a parallel run measures coverage in
  every worker but never combines or reports it, and still exits 0. Move the
  flags to the command line
  ([Coverage](coverage.md)).
- **`--timeout N`**: works only while pytest-timeout is installed, and then
  the plugin and rstest both arm a timer. Once you remove the plugin it is a
  usage error (exit 4). Pass `rstest --timeout N` on the command line; there
  is no `[tool.rstest]` key for it
  ([pytest-timeout](plugins.md)).
- **`--looponfail` / `-f`**: remove it before switching; with pytest-xdist
  installed it hangs the run (see the [flag map](#flag-map) above).
- **`--tx`, `--rsync*`, `-d`, `--maxprocesses`, `--max-worker-restart`**: see
  the [flag map](#flag-map) above; drop them once no pytest-xdist job still
  needs them.

## Removing pytest-xdist

The last step of the migration is uninstalling pytest-xdist. Once no
pytest-xdist CI job is left as a fallback, check that nothing still depends
on it:

```console
$ rstest xdist-removal-check --xdist-trial
```

It scans the pytest config, `PYTEST_ADDOPTS`, your conftests, tests and
local plugins, and the installed pytest plugins for every edge above: xdist
flags in `addopts`, `-n` / `--dist` that rstest never read, `pytest-xdist` in
`required_plugins`, `import xdist` sites (yours and installed plugins'),
xdist hook implementations not marked `optionalhook=True`, and
`hasplugin("xdist")` gates. Each finding comes with its fix. `--xdist-trial`
then runs the suite with pytest-xdist hidden (`-p no:xdist`, which drops its
options and hook specs like an uninstall) and names any test that only passes
with it installed. When it prints `ready`, uninstall pytest-xdist
and drop it from your dependency lists. See
[`xdist-removal-check`](../reference/cli-commands.md#xdist-removal-check).

## What improves

- **Single collection authority**: xdist aborts runs when workers collect
  differently ("Different tests were collected..."). Under full collection
  rstest verifies by hash and refuses **before** misassigning, and its error
  names the cause (usually an unstable `parametrize` id). Lazy collection
  collects each file once, so there is nothing to disagree about. `rstest
  migrate-check` finds this *before* the first run: it collects twice,
  diffs the nodeid sets, and names the exact `parametrize` site with the
  unstable id (memory address / uuid); see
  [migrate-check](migrate-from-pytest.md#the-migrate-check-preflight).
- **Crash attribution**: xdist infers the culprit of a crashed worker;
  rstest knows exactly which test was running, reports it failed, and
  finishes the run on a replacement worker.
- **Long-pole splitting**: xdist's default `load` scheduler has no duration
  data. It hands each worker batches of consecutive tests in collection
  order, so a slow file's tests tend to travel together, and a slow file
  late in collection order starts late (`loadfile` / `loadscope` pin files
  outright). rstest's default mode dispatches slowest-first from the
  duration cache and splits slow files across workers (when auto picks lazy
  collection it dispatches whole files, slowest first, and only does so when
  no file would be the long pole). On wait-heavy
  suites this more than halves the wall time vs xdist (see
  [Benchmarks](../reference/benchmarks.md)).
- **One merged output**: summary, `--lf` cache, junitxml, and coverage, with
  no per-worker stitching.
- **Pretty parallel output**: `--output bar` gives a pytest-sugar-style per-test
  view (result lines, inline failures, progress bar) *under the pool*.
  pytest-sugar is disabled under xdist because workers can't share the
  terminal; rstest renders it orchestrator-side instead.

Numerics/ML suites carrying over from xdist: the same per-worker RNG seeding and
BLAS/`OMP_NUM_THREADS` oversubscription concerns apply here as under xdist; see
[Numeric determinism](parallel-safety.md#numeric-determinism-ml-numerics-suites).

## Already fast under xdist? (CPU-bound / parity suites) { #already-fast-cpu-bound }

If your suite is **CPU-bound** (real compute per test, no sleeps or socket
waits) and it already splits cleanly under xdist (`-n 8` ≈ 8× serial, workers
stay busy, no single long test gating the run), **rstest
lands at parity on raw speed, not a win.** sympy (3,061 pure-Python compute
tests) runs 16.1s under rstest `-n 8` and 14.8s under xdist `-n 8`, within
noise at every worker count from 1 to 14 (see the
[CPU-bound benchmarks](../reference/benchmarks.md#cpu-bound-suites)). Don't
switch for wall-clock alone.

Why: the gains are capped by your cores. rstest's headline wins come from
slowest-first, test-granular dispatch splitting a slow file that xdist's
collection-order batches leave to one worker or start late: a *wait-bound*
pattern. A CPU-bound suite that
already spreads evenly has no such slack; both runners saturate your cores,
neither exceeds them. Measured on an M4 Max (10 performance + 4 efficiency
cores), sympy scales to 6.3x at `-n 10` and drops to 5.7x at `-n 14`: expect gains
up to your performance-core count, then a flat or falling curve.

**What's still worth it anyway** (beyond the improvements above):

- [`rstest --doctor`](doctor.md): fixture hotspots (a function-scoped fixture
  run hundreds of times is a widen-scope candidate), resource leaks (a leaked
  thread/fd is the leading cause of order-dependent flakiness; gate with
  [`--fail-on-leak`](../reference/cli.md#-fail-on-leak)), realized parallel
  efficiency, and a PR-vs-main health trend from `--doctor-json`.
- [`rstest --changed`](changed.md): narrows *which* tests run (xdist only
  speeds the full run). Its coverage index maps changed *lines* to the
  individual tests that hit them, tighter than the whole-file import graph, and
  a bigger inner-loop win than any scheduler tweak when a full run costs
  cores × time.

**BLAS threads (numpy, scikit-learn, torch).** numpy and friends run their own
thread pools, so at `-n N` you get N workers, each with its own pool. Measured
on numpy with Accelerate (macOS;
[cpu-bench grid](https://github.com/KovantAI/rstest/tree/main/examples/cpu-bench#worker-x-blas-thread-grid)):

- On a realistic suite (scikit-learn `linear_model`, small ops) the thread cap
  made no difference from `-n 2` up (at `-n 1`, uncapped was about 7% slower).
- On heavy matmul/solve tests, library threads **help** while `-n` is below
  the core count (one worker: 13.4s capped at one thread, 7.7s uncapped), and
  the cap moves the wall by 3% at most once `-n` reaches the core count.

So one thread per worker is not a free default. Pin it when you see
oversubscription on your stack (OpenBLAS and MKL spin their own threads and
can behave differently from Accelerate), or when you need bit-stable
reductions (a tight `assert x == expected` can flip when the reduction order
changes):

```console
$ OMP_NUM_THREADS=1 OPENBLAS_NUM_THREADS=1 MKL_NUM_THREADS=1 \
    VECLIB_MAXIMUM_THREADS=1 rstest -n auto
```

`VECLIB_MAXIMUM_THREADS` is the one Accelerate reads (numpy's macOS arm64
wheels use it). Measure your own stack with `examples/cpu-bench/measure.py
--grid`. See [Numeric determinism](parallel-safety.md#numeric-determinism-ml-numerics-suites).

**Memory.** Each worker is a full OS process. Measured
([cpu-bench](https://github.com/KovantAI/rstest/tree/main/examples/cpu-bench#memory),
[scikit-learn](../reference/benchmarks.md#memory-and-blas-threads)):

```text
peak ≈ orchestrator + N × (worker baseline + your suite's working set)
```

- **Worker baseline:** 38 MiB (interpreter, pytest, rstest's worker), the same
  as an xdist worker.
- **Orchestrator:** about 10 MiB for rstest (Rust). xdist's controller is a
  Python process: about 38 MiB.
- **Working set:** your serial run's peak RSS minus the baseline. A suite that
  holds about 400 MiB per worker (the cpu-bench `blas` tests) peaks at 3.3 GiB
  at `-n 8`: 8 × its single-worker peak.

That total is the upper bound, reached when every worker is at its peak at
once (normal for a long suite; a short one comes in under it). Size `-n` as
available RAM ÷ your serial peak RSS, and leave headroom for processes your
tests start themselves (scikit-learn's joblib pools add about 500 MiB on its
own suite). `--doctor` does not measure memory; use `/usr/bin/time -v`,
`psutil`, or your CI's memory graph.

**How to decide for real.** [`rstest try`](../reference/cli-commands.md#try) runs your own
suite under plain pytest and under `rstest -n auto`, reporting parity and speed
before you change any config. Confirm the parity-not-a-win call on your tests
and cores, not on sympy's.
