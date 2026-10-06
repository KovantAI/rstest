# Public-suite corpus

Parity + timing runs of rstest against well-known open-source pytest
suites. Every suite runs twice from the same venv (baseline `pytest`,
pinned to the vendored version, and `rstest`), and per-test outcomes
are diffed (setup/call/teardown phases plus `wasxfail`).

The plugins these suites load (and pass under rstest) are inventoried in
[docs/reference/corpus-plugins.md](../docs/reference/corpus-plugins.md);
regenerate it after a `--prepare` refresh (see that file's footer).

Plugins install unpinned, so their versions move between runs. Each run
records every suite venv's plugins (name, version, declared pytest range)
under `plugins` in `results.json`; the weekly bench uploads it with
`bench.json`. Refresh the version table in
[docs/guides/plugin-stack.md](../docs/guides/plugin-stack.md) from it:

```sh
python3 corpus/plugin_versions.py                   # from results.json
python3 corpus/plugin_versions.py --from-venvs      # probe corpus/work/*/venv
python3 corpus/plugin_versions.py --plugins all     # every plugin seen
```

## Running

```sh
python3 corpus/run.py                 # everything: prepare + execute
python3 corpus/run.py --prepare-only  # network phase only (clone, venv, install)
python3 corpus/run.py --execute-only  # offline phase only (assumes prepared)
python3 corpus/run.py --only pandas,flask
python3 corpus/run.py --skip sqlalchemy
```

Two strict phases:

1. **prepare**, all network: clone (SHA-pinned via `lock.json`), venv,
   installs, the rstest wheel (newest in `target/wheels`, or `--wheel`).
2. **execute**, fully offline: baseline run, rstest run, diff. Results
   land in `results.json` and a markdown table on stdout.

Reproducibility measures:

- checkouts pinned to `lock.json` SHAs (written on first clone),
- baseline pytest pinned to the vendored version (no version-skew diffs),
- `PYTHONHASHSEED=0` for both runs (stable set/dict-repr parametrize IDs),
- run-dependent parametrize IDs (memory addresses, `uuid4()`) are
  normalized and paired before counting missing/extra.

### Speed ramp (`bench.py`)

`run.py` measures a single `-n auto` wall per suite (parity is its job).
`bench.py` reuses the same prepared venvs to prove the *speedup curve* instead:

```sh
SUITES=python-dateutil,httpx,fastapi,anyio,langgraph
python3 corpus/run.py --prepare-only --only $SUITES
python3 corpus/bench.py --only $SUITES --sweep anyio
```

Two views: a **spectrum** across suites and a **worker sweep** on one suite
(`-n 1,2,4`, showing the ramp with core count, plateauing at the runner's
cores). The default spectrum is picked to span every regime, not just the wins:

| suite | regime | ~speedup |
|---|---|---|
| python-dateutil | struggler: tiny suite, rstest startup dominates | 0.94× |
| httpx | struggler: forced `-n 0` (session fixture, fixed port) | ~1.0× |
| fastapi | mid gain | 2.6× |
| anyio | big gain (also the sweep suite) | 3.9× |
| langgraph | monorepo: N serial per-lib pytest vs one root run | 7.3× |

Speedup tracks the *parallelizable share*, not raw size (a tiny or
serial-pinned suite can sit at or below 1×). Every point is the **median** of
`--repeat` runs with the min-max spread shown, since wall time on shared
runners is noisy. Each rstest run is still diffed against pytest; parity below
`--parity-floor` fails, so a fast-but-wrong run is never counted as a win (all
five defaults are documented at 100% parity). Report → stdout +
`$GITHUB_STEP_SUMMARY`, data → `bench.json`.

Options for the CPU-bound benchmarks (sympy, scikit-learn; see
[docs/reference/benchmarks.md](../docs/reference/benchmarks.md#cpu-bound-suites)):

```sh
# Sweep any -n values, with pytest-xdist at the same -n (needs xdist in the venv).
python3 corpus/bench.py --only sympy,scikit-learn --sweep sympy,scikit-learn \
    --sweep-workers 1,2,4,8,10,14 --xdist --repeat 5
# Peak memory per -n, and the -n x BLAS-thread grid (psutil for the tree total).
python3 corpus/bench.py --only scikit-learn --sweep '' --memory scikit-learn \
    --grid scikit-learn --grid-workers 1,2,4,10,14 --grid-threads 1,2,4,unset
```

`--xdist-dist MODE` runs the xdist series with that `--dist` mode (e.g.
`worksteal`; default: xdist's own `load`). Two standalone measurements back
specific claims on the benchmarks page: `cpu_sample.py` samples the CPU of a
run's coordinating process and its workers (the pandas controller reading),
and `watch_cycle.py` times `rstest --watch` reruns on a one-test project.

Every point gets `--warmup` untimed runs first (default 1). rstest is warm
unless `--cold` (drops `.rstest_cache` before every run). A run the machine
slept through is detected (wall clock vs monotonic clock) and re-run; on macOS,
wrap long runs in `caffeinate -ims` anyway. `bench.json` holds the latest run;
runs published in the docs are archived in `bench-results/`.

Runs weekly (+ manual) on a standard GitHub runner via
`.github/workflows/corpus-bench.yml`: a fresh measured datapoint on public
hardware, never a projection. Soft gate: the sweep suite's best speedup must
clear `--floor` (default 2x). Wall time is otherwise advisory; never gate
ordinary CI on it.

## Results (2026-06-13, M-series macOS, wheel 0.0.5)

A historical snapshot, kept as the per-suite parity record. The current
published numbers (re-measured 2026-09-30 with rstest 0.8.0) are in
[docs/reference/benchmarks.md](../docs/reference/benchmarks.md); test counts
and walls there differ from this table.

The table covers 31 of the 33 parity suites: langgraph is measured
separately [below](#monorepo-mono-mode), and langchain joined the
corpus after this snapshot. 25/31 suites at 100% per-test outcome parity; every non-100% suite is
explained below (permanent by-design diffs or upstream flakes that hit
plain pytest equally).

| suite | tests | parity | pytest | rstest |
|---|---|---|---|---|
| pandas | 193,627 | 100% | 187.1s | 40.5s |
| packaging | 61,570 | 100% | 19.9s | 10.1s |
| sqlalchemy | 25,300 | 99.97% | 524.5s | 52.6s (`-n auto`, 7 serial-baseline skips) |
| pydantic | 12,733 | 99.97% | 14.2s | 14.1s (`-n 0`, sys.path param) |
| jsonschema | 8,337 | 100% | 4.2s | 2.7s |
| aiohttp | 4,469 | 100%† | 199.1s | 67.3s (†this run; the 2026-09-30 benchmark runs measured 99.91-99.98%, socket-leak flake) |
| anyio | 3,814 | 100% | 103.4s | 26.5s |
| fastapi | 3,179 | 100% | 25.9s | 10.0s |
| urllib3 | 2,299 | 100% | 54.6s | 39.4s |
| python-dateutil | 2,096 | 100% | 1.5s | 1.6s |
| django-allauth | 2,050 | 100% | 22.5s | 7.5s |
| arrow | 1,902 | 100% | 3.5s | 3.4s |
| click | 1,697 | 100% | 2.6s | 2.6s |
| httpx | 1,418 | 100% | 3.3s | 3.1s (`-n 0`, fixed port) |
| attrs | 1,391 | 100% | 4.2s | 2.6s |
| typer | 1,374 | 99.93% | 8.9s | 2.9s (1 isolation-defect test) |
| marshmallow | 1,178 | 100% | 0.6s | 0.6s |
| werkzeug | 992 | 99.9% | 5.8s | 2.4s (1 unix-socket test) |
| rich | 981 | 99.8%* | 3.9s | 2.4s (*upstream flake, hits pytest too) |
| starlette | 959 | 100% | 3.1s | 1.7s |
| structlog | 920 | 100% | 0.9s | 0.8s |
| jinja2 | 911 | 100% | 1.0s | 1.0s |
| trio | 896 | 100% | 6.1s | 2.8s |
| more-itertools | 725 | 100% | 5.9s | 3.3s |
| requests | 635 | 99.69% | 74.2s | 13.4s (pytest.`__file__` param) |
| flask | 491 | 100% | 0.9s | 1.1s |
| itsdangerous | 297 | 100% | 0.4s | 0.3s |
| tenacity | 161 | 100% | 2.1s | 2.0s |
| freezegun | 149 | 100% | 0.7s | 0.7s |
| pluggy | 139 | 100% | 0.2s | 0.2s |
| markupsafe | 80 | 100% | 0.6s | 0.2s |

Totals: ~337k tests. Headline walls: pandas 4.6×, aiohttp 3.0×,
anyio 3.9×, allauth 3.0×, requests 5.5×, typer 3.1×.

### Monorepo (mono mode)

Measured separately from the table above (2026-10-06, rstest 0.8.0, pytest
9.1.1 pin, `bench.py --only langgraph --sweep ''`: median of 5 after one
warm-up, min-max in parentheses; result in
`bench-results/2026-10-06-langgraph.json`). langgraph is a monorepo: N
per-lib pytest configs the baseline runs serially vs one root rstest
pass. The measured subset is the five DB-free, service-free libs
(`libs/langgraph` itself is excluded: its live-app/service tests hang
the plain-pytest baseline in a service-less env; see `suites.toml`).

| suite | tests | parity | pytest | rstest | speedup |
|---|---|---|---|---|---|
| langgraph (5 libs) | 838 | 100% | 190.0s (185.7-201.8) | 26.1s (25.0-27.2) | 7.3× |

`libs/sdk-py` collects no tests under either runner (its test modules import
`starlette`, which the prepared venv lacks; both runners report the same 10
collection errors), so the 838 tests come from the other four libs.

## Per-suite policies

Flags live in `suites.toml` as `rstest_args`, with a comment explaining
each. The classes:

| Class | Suites | Policy |
|---|---|---|
| Fixed network port in session fixture | httpx | `-n 0` |
| Run-dependent parametrize IDs (`now()`) | marshmallow, arrow | see below |
| Per-process parametrize IDs (memory addresses) | pydantic | `-n 0` |
| Load-sensitive timing tests | werkzeug, urllib3, typer, anyio | `-n 4` |
| Wall-clock-sensitive whole suite | allauth | `-n 4` |

### xdist master-side hooks (`pytest_configure_node` → `workerinput`)

rstest has no master Python process, so each worker plays master for
itself: it builds a shim `WorkerController` and runs every plugin's
`pytest_configure_node` against it (`pytest_plugin_registered` re-fires it
for plugins that register mid-`configure`). This covers hooks whose
injected value is **self-derivable** (a `uuid4`, or a `workerid` suffix):

- **sqlalchemy** (`follower_ident`) runs at full **`-n auto`**: 4.6×
  (552.4s→119.8s), 99.96% in the latest `results.json` (the table above,
  an older snapshot, has 10× and 99.97%). xdist installed → its `XDistHooks` registers → the
  emulation fires `configure_node` → each worker self-assigns
  `follower_ident=uuid4()` and provisions its own follower DB. The 9-test
  gap is serial-baseline-vs-parallel (those IMV/RETURNING tests skip in the
  serial baseline but pass under real xdist *and* rstest), not a follower
  bug.

A value that needs **single-allocator cross-worker coordination** (one
counter or registry shared by all workers) can't be emulated this way.
pytest-retry's `server_port` looked like that case but isn't: rstest keeps
`numprocesses` visible (it stops xdist's session with `dist="no"` instead),
so with xdist installed the plugin's controller branch runs in each worker
and provisions its own `ReportServer`; without xdist, rstest starts that
server in each worker and seeds `workerinput["server_port"]`. The langgraph
`checkpoint-sqlite` lib runs at full worker count with no per-lib policy.
Details: [xdist hook emulation](../docs/concepts/xdist-hooks.md).

### Test-order plugins (pytest-randomly / reverse / ordering)

No impact, no policy needed. The collection-mismatch guard hashes each
worker's `session.items` in collection order (order-sensitive), so a
plugin that shuffles per process *would* diverge, but rstest syncs the
randomly seed across workers (via its emulated `workerinput`), so every
worker collects the same shuffled order, hashes match, and dispatch is
safe. **structlog** (pytest-randomly) runs at full `-n auto` with no
policy, 100% parity, verified stable across runs. Deterministic
reorderers (reverse/ordering) never diverge in the first place.

### Run-dependent parametrize IDs (`now()`/`uuid4`/addresses)

Full collect dispatches **by index**, so every worker must agree on the
*ordered* id list (count + order-sensitive hash). Whether a run-dependent id
breaks that depends on **where** the nondeterminism lives:

- **positionally stable across workers** (a `now()` timestamp each worker
  evaluates at the same collection position) → hashes match, no bail, ids pair
  1:1 against the baseline. **marshmallow** runs at full `-n auto`, **100%**,
  *as long as collection order is preserved*. (Do **not** use `--collect lazy`
  here: its file-affine reorder breaks the positional pairing → 99.66%.)
- **per-worker-process values** (object **memory addresses** `0x…`, generator
  reprs) → each worker's collection hashes differently → the guard bails
  ("workers collected different test sets") and the suite can't dispatch under
  full collect. **pydantic** hits this; `--collect lazy` also errors here
  (rc=4), so it stays **`-n 0`**.

`--collect lazy` (one worker per file, no cross-worker hash compare) is the
escape hatch when the divergence is real but the per-file order is stable:

- **arrow**: was `-n 0`, now `--collect lazy` at full `-n auto` (100%).

httpx `-n 0` has no flag fix: its session fixture binds a fixed port
(port-using tests span 9 files, so even `--dist loadfile` can't confine
them to one worker; `-n 4` deadlocks on the double bind, and there are no
`xdist_group` markers for `--dist loadgroup` to use).

## Known permanent diffs

Each entry below (root cause plus the concrete upstream change that would
remove it) is catalogued in
[Parity divergences & upstream fixes](../docs/reference/parity-divergences.md).

- **requests**: one test parametrizes on `pytest.__file__`, which
  resolves to the vendored core inside workers. Visible vendoring,
  by design (decision D7).
- **pydantic**: one test parametrizes on `sys.path`, which inside
  workers contains the vendored-core entry. Same class as requests.
- **werkzeug**: one unix-socket server test is parallel-unsafe and
  flaky under any runner at load.
- **rich**: `test_syntax.py` lexer-guess tests flake under plain
  sequential pytest too (~1 in 5 full-suite runs; verified
  pytest=failed/rstest=passed): upstream test pollution, both runners
  affected equally. Installing ipywidgets (unused by the tests) makes
  it worse: its IPython dependency registers a pygments plugin lexer
  with nondeterministic tie-breaking.
- **typer**: one warning-assertion test has an isolation defect: it misses
  its `pytest.warns` when a sibling that touched the warnings state runs
  first in the same worker (see
  [parity divergences](../docs/reference/parity-divergences.md)). The `-n 4`
  policy is for the separate, load-sensitive progressbar timing tests.

## Corpus-found bugs (fixed)

The corpus exists to find real-world gaps; the notable catches:

- pool mode ran tests past collection errors (pytest's abort guard
  lives in the `pytest_runtestloop` that item dispatch replaces),
  caught by jsonschema,
- `multiprocessing` spawn / `anyio.to_process` children re-import the
  worker's `__main__` without package context (relative import,
  unguarded `main()`, non-idempotent sys.path bootstrap), caught by
  anyio, including `test_identical_sys_path`,
- broken-pipe tracebacks from workers after a collection-mismatch
  refusal, and the refusal message itself not naming the common causes.
