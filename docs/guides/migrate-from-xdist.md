# Migrating from pytest-xdist

rstest replaces pytest-xdist rather than wrapping it: parallelism is
native, and the worker environment is xdist-shaped on purpose so plugins
keep working.

## Flag map

| pytest-xdist | rstest | Notes |
|---|---|---|
| `-n 4` / `-n auto` | same | `auto` is logical cores, capped for small suites; it is the default |
| `-n 1` | differs | xdist's `-n 1` runs one `gw0` worker WITH `workerinput`; rstest's `-n 1` (like `-n 0`) is plain byte-exact mode with no worker identity |
| `--dist load` | same (default) | plus duration-aware long-pole-first scheduling |
| `--dist loadfile` | same | file affinity, in-file order |
| `--dist loadscope` / `loadgroup` | same | incl. `@pytest.mark.xdist_group` |
| `--dist each` | partial | full suite per worker, but every worker uses the SAME interpreter — xdist's heterogeneous `--tx` gateways have no equivalent |
| `--maxprocesses` | — | use `-n` |
| `-p xdist.looponfail` / `--looponfail` | `--watch` | with import-graph selection |

pytest-rerunfailures maps: `--reruns N`,
`@pytest.mark.flaky(reruns=N)`, and `--only-rerun REGEX` work natively
in parallel modes (and crash-aware — a test that kills its worker
retries on the replacement). The plugin itself is neutralized inside
pool workers so nothing double-reruns; at `-n 0` the plugin keeps its
native behavior and handles reruns itself. Two scope notes: rstest's
`--reruns` requires `-n ≥ 2`, and is rejected under `--dist each`.

## What your plugins see

rstest workers announce themselves exactly like xdist workers.
`config.workerinput` carries: `workerid` (`gw0`, `gw1`, ...),
`workercount`, `testrun_uid` (one uid per run, shared by all workers),
`mainargv`, and the `cov_master_*` keys pytest-cov expects. The
`PYTEST_XDIST_WORKER` and `PYTEST_XDIST_WORKER_COUNT` environment
variables are set too, so plugins and conftests that grep the
environment keep working as-is. Plugins
keying per-worker resources on worker identity — pytest-django's
per-worker test databases being the canonical case — work unchanged.

`RSTEST_WORKER_ID` (same `gwN` values) is also set if you want to
detect rstest specifically.

## Master-side hooks

xdist's controller-side hooks — `pytest_configure_node`,
`pytest_testnodeready`, `pytest_testnodedown` — are emulated: each worker
plays controller for itself, calling your implementations against a node shim
with its own `workerinput`, `gateway.id`, and `config`. Hooks that are pure
functions of the node (read `gateway.id`, fill `workerinput`, provision a
resource from them — SQLAlchemy's `follower_ident` pattern) produce the same
observable result as xdist.

Two things to know if you rely on these hooks: they run **N times
concurrently in N processes** (controller-side shared state needs rework —
derive from `gateway.id` or a uuid), and a crashed worker's
`pytest_testnodedown` runs on a *surviving* worker, so teardown must be a
function of `node.workerinput` alone. Full semantics, timing, and the crash
race: [xdist hook emulation](../concepts/xdist-hooks.md).

## If xdist is still in your ini

`addopts = -n 4` with pytest-xdist installed is neutralized inside rstest
workers automatically — options parse, the xdist session never engages, no
nested workers. Remove it at your convenience and pass `-n` to rstest.

## What improves

- **Single collection authority**: xdist aborts runs when workers collect
  differently ("Different tests were collected..."); rstest verifies by
  hash and refuses BEFORE misassigning — and its error names the cause
  (usually a randomizing plugin without a fixed seed). `rstest
  --migrate-check` finds this *before* the first run: it collects twice,
  diffs the id sets, and names the exact `parametrize` site with the
  unstable id (memory address / uuid) — see
  [migrate-check](migrate-from-pytest.md#the-migrate-check-preflight).
- **Crash attribution**: xdist infers the culprit of a crashed worker;
  rstest knows exactly which test was running, reports it failed, and
  finishes the run on a replacement worker.
- **Long-pole splitting**: xdist's schedulers keep whole files together;
  rstest's default mode splits slow files across workers — on wait-heavy
  suites this more than halves the wall time vs xdist (see
  [Benchmarks](../reference/benchmarks.md)).
- **One merged output**: summary, `--lf` cache, junitxml, coverage — no
  per-worker stitching.
- **Pretty parallel output**: `--output bar` gives a pytest-sugar-style per-test
  view (result lines, inline failures, progress bar) *under the pool* —
  pytest-sugar is disabled under xdist because workers can't share the
  terminal; rstest renders it orchestrator-side instead.
