# Public-suite corpus

Parity + timing runs of rstest against well-known open-source pytest
suites. Every suite runs twice from the same venv — baseline `pytest`
(pinned to the vendored version) and `rstest` — and per-test outcomes
are diffed (setup/call/teardown phases plus `wasxfail`).

## Running

```sh
python3 corpus/run.py                 # everything: prepare + execute
python3 corpus/run.py --prepare-only  # network phase only (clone, venv, install)
python3 corpus/run.py --execute-only  # offline phase only (assumes prepared)
python3 corpus/run.py --only pandas,flask
python3 corpus/run.py --skip sqlalchemy
```

Two strict phases:

1. **prepare** — all network: clone (SHA-pinned via `lock.json`), venv,
   installs, the rstest wheel (newest in `target/wheels`, or `--wheel`).
2. **execute** — fully offline: baseline run, rstest run, diff. Results
   land in `results.json` and a markdown table on stdout.

Reproducibility measures:

- checkouts pinned to `lock.json` SHAs (written on first clone),
- baseline pytest pinned to the vendored version (no version-skew diffs),
- `PYTHONHASHSEED=0` for both runs (stable set/dict-repr parametrize IDs),
- run-dependent parametrize IDs (memory addresses, `uuid4()`) are
  normalized and paired before counting missing/extra.

## Results (2026-06-13, M-series macOS, wheel 0.0.5)

26/31 suites at 100% per-test outcome parity; every non-100% suite is
explained below (permanent by-design diffs or upstream flakes that hit
plain pytest equally).

| suite | tests | parity | pytest | rstest |
|---|---|---|---|---|
| pandas | 193,627 | 100% | 187.1s | 40.5s |
| packaging | 61,570 | 100% | 19.9s | 10.1s |
| sqlalchemy | 25,300 | 99.97% | 524.5s | 52.6s (`-n auto`, 7 serial-baseline skips) |
| pydantic | 12,733 | 99.97% | 14.2s | 14.1s (`-n 0`, sys.path param) |
| jsonschema | 8,337 | 100% | 4.2s | 2.7s |
| aiohttp | 4,469 | 100% | 199.1s | 67.3s |
| anyio | 3,814 | 100% | 103.4s | 26.5s |
| fastapi | 3,179 | 100% | 25.9s | 10.0s |
| urllib3 | 2,299 | 100% | 54.6s | 39.4s |
| python-dateutil | 2,096 | 100% | 1.5s | 1.6s |
| django-allauth | 2,050 | 100% | 22.5s | 7.5s |
| arrow | 1,902 | 100% | 3.5s | 3.4s |
| click | 1,697 | 100% | 2.6s | 2.6s |
| httpx | 1,418 | 100% | 3.3s | 3.1s (`-n 0`, fixed port) |
| attrs | 1,391 | 100% | 4.2s | 2.6s |
| typer | 1,374 | 99.93% | 8.9s | 2.9s (1 load-sensitive test) |
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

Totals: ~341k tests. Headline walls: pandas 4.6×, aiohttp 3.0×,
anyio 3.9×, allauth 3.0×, requests 5.5×, typer 3.1×.

## Per-suite policies

Flags live in `suites.toml` as `rstest_args`, with a comment explaining
each. The classes:

| Class | Suites | Policy |
|---|---|---|
| Fixed network port in session fixture | httpx | `-n 0` |
| Run-dependent parametrize IDs (`now()`) | marshmallow, arrow, pydantic | see below |
| Load-sensitive timing tests | werkzeug, urllib3, typer, anyio | `-n 4` |
| Wall-clock-sensitive whole suite | allauth | `-n 4` |

### xdist master-side hooks (`pytest_configure_node` → `workerinput`)

rstest has no master Python process, so each worker plays master for
itself: it builds a shim `WorkerController` and runs every plugin's
`pytest_configure_node` against it (`pytest_plugin_registered` re-fires it
for plugins that register mid-`configure`). This covers hooks whose
injected value is **self-derivable** (a `uuid4`, or a `workerid` suffix):

- **sqlalchemy** (`follower_ident`) — runs at full **`-n auto`, 10×**
  (533s→53s), 99.97%. xdist installed → its `XDistHooks` registers → the
  emulation fires `configure_node` → each worker self-assigns
  `follower_ident=uuid4()` and provisions its own follower DB. The 7-test
  gap is serial-baseline-vs-parallel (those IMV/RETURNING tests skip in the
  serial baseline but pass under real xdist *and* rstest) — not a follower
  bug.

The model can't substitute for a real master when the injected value needs
**single-allocator cross-worker coordination** — e.g. pytest-retry's
`server_port`, one `ReportServer` all workers dial. Worse, its master
branch is gated on `numprocesses`, which rstest nulls to stop xdist's
session — so the server never starts and the worker `KeyError`s on the
missing port. That's the langgraph `checkpoint-sqlite` serial pin, and the
one case that would need a controller-side configure pass (or letting
plugin master-detection see a truthy `numprocesses` without engaging
xdist's session).

### Test-order plugins (pytest-randomly / reverse / ordering)

No impact, no policy needed. The collection-mismatch guard hashes each
worker's `session.items` in collection order (order-sensitive), so a
plugin that shuffles per process *would* diverge — but rstest syncs the
randomly seed across workers (via its emulated `workerinput`), so every
worker collects the same shuffled order, hashes match, and dispatch is
safe. **structlog** (pytest-randomly) runs at full `-n auto` with no
policy, 100% parity — verified stable across runs. Deterministic
reorderers (reverse/ordering) never diverge in the first place.

### Run-dependent parametrize IDs (`now()`/`uuid4`/addresses)

Full collect dispatches **by index**, so every worker must agree on the
*ordered* id list (count + order-sensitive hash). Whether a run-dependent id
breaks that depends on **where** the nondeterminism lives:

- **positionally stable across workers** (a `now()` timestamp each worker
  evaluates at the same collection position) → hashes match, no bail, ids pair
  1:1 against the baseline. **marshmallow** runs at full `-n auto`, **100%** —
  *as long as collection order is preserved*. (Do **not** use `--collect lazy`
  here: its file-affine reorder breaks the positional pairing → 99.66%.)
- **per-worker-process values** (object **memory addresses** `0x…`, generator
  reprs) → each worker's collection hashes differently → the guard bails
  ("workers collected different test sets") and the suite can't dispatch under
  full collect. **pydantic** hits this; `--collect lazy` also errors here
  (rc=4), so it stays **`-n 0`**.

`--collect lazy` (one worker per file, no cross-worker hash compare) is the
escape hatch when the divergence is real but the per-file order is stable:

- **arrow** — was `-n 0`, now `--collect lazy` at full `-n auto` (100%).

httpx `-n 0` has no flag fix: its session fixture binds a fixed port
(port-using tests span 9 files, so even `--dist loadfile` can't confine
them to one worker — `-n 4` deadlocks on the double bind, and there are no
`xdist_group` markers for `--dist loadgroup` to use).

## Known permanent diffs

Each entry below — root cause plus the concrete upstream change that would
remove it — is catalogued in
[Parity divergences & upstream fixes](../docs/reference/parity-divergences.md).

- **requests** — one test parametrizes on `pytest.__file__`, which
  resolves to the vendored core inside workers. Visible vendoring,
  by design (decision D7).
- **pydantic** — one test parametrizes on `sys.path`, which inside
  workers contains the vendored-core entry. Same class as requests.
- **werkzeug** — one unix-socket server test is parallel-unsafe and
  flaky under any runner at load.
- **rich** — `test_syntax.py` lexer-guess tests flake under plain
  sequential pytest too (~1 in 5 full-suite runs; verified
  pytest=failed/rstest=passed): upstream test pollution, both runners
  affected equally. Installing ipywidgets (unused by the tests) makes
  it worse — its IPython dependency registers a pygments plugin lexer
  with nondeterministic tie-breaking.
- **typer** — one warning-assertion test is load-sensitive; flakes at
  high worker counts (hence the `-n 4` policy).

## Corpus-found bugs (fixed)

The corpus exists to find real-world gaps; the notable catches:

- pool mode ran tests past collection errors (pytest's abort guard
  lives in the `pytest_runtestloop` that item dispatch replaces) —
  caught by jsonschema,
- `multiprocessing` spawn / `anyio.to_process` children re-import the
  worker's `__main__` without package context (relative import,
  unguarded `main()`, non-idempotent sys.path bootstrap) — caught by
  anyio, including `test_identical_sys_path`,
- broken-pipe tracebacks from workers after a collection-mismatch
  refusal, and the refusal message itself not naming the common causes.
