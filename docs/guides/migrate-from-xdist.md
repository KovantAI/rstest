# Migrating from pytest-xdist

rstest replaces pytest-xdist rather than wrapping it: parallelism is
native, and the worker environment is xdist-shaped on purpose so plugins
keep working.

## Flag map

Most xdist flags carry over unchanged. The ones people actually touch:

- **`-n 4` / `-n auto`**: same, and `auto` is the default. `auto` is capped by
  test-file count and cached suite time, so pass an explicit `-n` when you
  need a fixed count (for example with `--shard`).
- **`--dist load` / `loadfile` / `loadscope` / `loadgroup`**: same names and
  semantics, including `@pytest.mark.xdist_group`. `load` (the default) adds
  duration-aware slowest-first scheduling.
- **`-n 1`**: differs. xdist's `-n 1` is one `gw0` worker with `workerinput`;
  rstest's `-n 1`, like `-n 0`, is single-worker mode with no worker identity.
- **`--dist no`**: rejected (exit 1). Use `-n 0` for a single worker.

Everything else (`--tx`, `--rsync*`, `-d`, `--maxprocesses`,
`--max-worker-restart`, `--dist each`, `--looponfail`) is covered row by row,
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
`--reruns` are rejected under `--dist each` (that mode exists to expose
per-worker outcome differences, so retrying failures would defeat it; see
[`--dist each`](../reference/cli.md#-dist-loadloadfileloadscopeloadgroupeach)).

## What your plugins see

rstest workers announce themselves exactly like xdist workers.
`config.workerinput` carries: `workerid` (`gw0`, `gw1`, ...),
`workercount`, `testrun_uid` (one uid per run, shared by all workers),
`mainargv`, and the `cov_master_*` keys pytest-cov expects. The
`PYTEST_XDIST_WORKER` and `PYTEST_XDIST_WORKER_COUNT` environment
variables are set too, so plugins and conftests that grep the
environment keep working as-is. Plugins
keying per-worker resources on worker identity work unchanged. The canonical
case is pytest-django's per-worker test database (`test_<name>_gw0`, ...),
which follows from the `workerid` above; note that rstest's corpus only
exercises pytest-django on SQLite `:memory:`, so check a server-backed
database (Postgres, MySQL) on your own suite.

`RSTEST_WORKER_ID` (same `gwN` values) is also set if you want to
detect rstest specifically.

The `worker_id` and `testrun_uid` **fixtures** are provided natively, with
xdist-identical semantics, so `def test(worker_id): ...` resolves whether or
not pytest-xdist is installed. Removing pytest-xdist from your config keeps
them working (`worker_id` is `"master"` below `-n 2`, `gwN` in the pool). See
the [xdist support matrix](../reference/xdist-support.md#fixtures-worker-identity).

## Master-side hooks

xdist's controller-side hooks (`pytest_configure_node`,
`pytest_testnodeready`, `pytest_testnodedown`) are emulated: each worker
plays controller for itself, calling your implementations against a node shim
with its own `workerinput`, `gateway.id`, and `config`. Hooks that are pure
functions of the node (read `gateway.id`, fill `workerinput`, provision a
resource from them, as in SQLAlchemy's `follower_ident` pattern) produce the same
observable result as xdist.

Two things to know if you rely on these hooks: they run **N times
concurrently in N processes** (controller-side shared state needs rework:
derive from `gateway.id` or a uuid), and a crashed worker's
`pytest_testnodedown` runs on a *surviving* worker, so teardown must be a
function of `node.workerinput` alone. Full semantics, timing, and the crash
race: [xdist hook emulation](../concepts/xdist-hooks.md).

## If xdist is still in your ini

`addopts = -n 4` with pytest-xdist installed is neutralized inside rstest
workers automatically: options parse, the xdist session never engages, no
nested workers. Keep it while a pytest-xdist job is still your fallback, then
remove it and pass `-n` to rstest.

## What improves

- **Single collection authority**: xdist aborts runs when workers collect
  differently ("Different tests were collected..."); rstest verifies by
  hash and refuses BEFORE misassigning, and its error names the cause
  (usually a randomizing plugin without a fixed seed). `rstest
  migrate-check` finds this *before* the first run: it collects twice,
  diffs the id sets, and names the exact `parametrize` site with the
  unstable id (memory address / uuid); see
  [migrate-check](migrate-from-pytest.md#the-migrate-check-preflight).
- **Crash attribution**: xdist infers the culprit of a crashed worker;
  rstest knows exactly which test was running, reports it failed, and
  finishes the run on a replacement worker.
- **Long-pole splitting**: xdist's schedulers keep whole files together;
  rstest's default mode splits slow files across workers. On wait-heavy
  suites this more than halves the wall time vs xdist (see
  [Benchmarks](../reference/benchmarks.md)).
- **One merged output**: summary, `--lf` cache, junitxml, coverage: no
  per-worker stitching.
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
stay busy, no single long test gating the run), the honest answer is: **rstest
lands at parity on raw speed, not a win.** pandas (193,627 tests) runs 61s under
xdist `-n 8` and 63s under rstest `-n 8`; the [benchmarks](../reference/benchmarks.md)
file this under *"parity, not victory."* Don't switch for wall-clock alone.

Why: the gains are capped at core count. rstest's headline wins come from
test-granular dispatch splitting a slow file that xdist's file-affinity
scheduler pins to one worker: a *wait-bound* pattern. A CPU-bound suite that
already spreads evenly has no such slack; both runners saturate your cores,
neither exceeds them. From [Benchmarks](../reference/benchmarks.md): wait-bound
suites gain most, CPU-bound suites gain up to core count, one-long-test suites
gain nothing beyond that test.

!!! note "Documentation gap"
    The "up to core count" claim isn't yet demonstrated with a real
    *compute-bound* benchmark: the corpus's CPU-bound data point (pandas) is
    collection-bound. Measure your own suite with `rstest try` rather than
    relying on a published ratio.

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

**Oversubscription (numpy/BLAS).** The one place a CPU-bound numerics suite can
get slower *or* flakier under naive parallelism, under xdist too. numpy/torch/
BLAS spin their own thread pools; at `-n auto` you get *workers × library-threads*
competing for cores, which both shifts reduction order (a tight `assert x ==
expected` can flip at `-n 8`) and fights for cores. Pin one thread per worker and
let rstest own parallelism:

```bash
OMP_NUM_THREADS=1 MKL_NUM_THREADS=1 OPENBLAS_NUM_THREADS=1 rstest -n auto
```

(Or cap `-n`.) See [Numeric determinism](parallel-safety.md#numeric-determinism-ml-numerics-suites).

**Memory.** Each worker is a full OS process, so peak memory scales roughly
linearly: N workers ≈ N × your serial peak RSS. A suite that holds large
arrays or models per worker can OOM at `-n auto` where xdist was tuned lower.
As a first cut, cap `-n ≈ available RAM ÷ per-worker peak RSS`. Note `--doctor`
does not measure memory (it instruments wall/CPU time and fixture cost), so
watch actual RSS (e.g. `/usr/bin/time -v`, `psutil`, or your CI's memory graph)
when sizing.

!!! note "Documentation gap"
    The linear-RSS rule above is a first-order estimate; there is no measured
    per-worker memory model, and no concrete worker×thread sweet-spot formula.
    Measure on your own hardware to tune precisely.

**How to decide for real.** [`rstest try`](migrate-from-pytest.md) runs your own
suite under plain pytest and under `rstest -n auto`, reporting parity and speed
before you change any config. Confirm the parity-not-a-win call on your tests
and cores, not on pandas'.
