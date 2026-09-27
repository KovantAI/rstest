# Compatibility

What rstest promises about matching pytest's behavior, how that promise is measured against real suites, and the known gaps.

## The contract

1. **At `-n 0`: pytest's exact outcomes.** One vendored-pytest session
   over your arguments. Any difference in per-test outcomes at `-n 0` is a
   bug in rstest. The named exceptions are the flags rstest owns and handles
   itself at every worker count, so they behave the same at `-n 0` as in
   parallel rather than as in pytest:
    - `--junitxml` and `--html`: rstest writes the file from merged results.
    - `--timeout` and `@pytest.mark.timeout`: rstest's native timeout (its
      SIGALRM timer is armed for marked tests even without `--timeout`).
    - `--reruns` / `--only-rerun`: rstest's rerun pool, not
      pytest-rerunfailures.
    - `--debug`: starts debugpy instead of pytest's debug log.

    To give one of these to pytest instead, pass it after `--`. See
    [Flags rstest owns](../guides/migrate-from-pytest.md#flags-rstest-owns).
2. **In parallel modes: outcomes preserved for parallel-safe tests.**
   Identical per-test outcomes (setup/call/teardown, skips, xfails) for
   tests without hidden timing/ordering/shared-state assumptions. Tests
   *with* such assumptions can flake under concurrency (the same class of
   flake pytest-xdist produces) and the
   [parallel safety](../guides/parallel-safety.md) rails exist for them.

## What "verified" means

Compatibility is measured, not asserted: rstest's battery runs four real
suites, pandas (193,627 tests), aiohttp, django-allauth and rich, under
pytest and under rstest, and diffs **per-test outcomes** (every phase,
every skip reason class, xfail flags). Their real plugins are loaded:
pytest-django, pytest-asyncio, pytest-aiohttp, hypothesis, pytest-mock,
pytest-cov. The worker counts are the ones in
[benchmarks](../reference/benchmarks.md): `-n 8` for pandas and aiohttp,
`-n 4` for rich and django-allauth (django-allauth is pinned to `-n 4`
because its rate-limit-window tests flake at high worker counts).

pandas and aiohttp measure 100% parity. rich and django-allauth measured
100% on the recorded run, but each contains tests that flake *under plain
pytest itself*, so an individual run can land at about 99.x% when the
baseline and rstest draw different flakes. Every such case is catalogued in
[Parity divergences](../reference/parity-divergences.md).

Summary-line accounting (passed/failed/skipped/xfailed/warnings counts)
matches pytest's numbers on the same suites.

## Vendored pytest version

rstest currently vendors **pytest 9.1.1**, unmodified. Policy:

- The vendored version is pinned per rstest release and stated in
  [License](../reference/license.md) and `rstest_worker._vendor`.
- Upstream pytest minor releases are adopted by re-vendoring verbatim and
  re-running the full compatibility battery.
- **Security fixes**: when upstream pytest ships a security fix affecting
  the vendored code, an rstest release with the re-vendored core is
  expected **within two weeks** of the upstream release. Because the
  vendored tree is verbatim, re-vendoring is mechanical; the two weeks
  budget the compatibility battery, not the patch.
- Local modifications to the vendored tree are forbidden; integration
  lives in `rstest_worker` around it.

!!! note "If your suite is pinned to an older pytest"
    Adopting rstest implicitly adopts the vendored pytest's major version:
    a suite (or plugin set) that isn't pytest-9-clean will see pytest 9
    behavior inside rstest workers, whatever pytest version is installed.
    There is currently no older-core build, and none is planned: one
    vendored core, tracked forward. When a new pytest MAJOR ships, the
    core is re-vendored after the early point releases stabilize: the
    same timing a cautious team upgrades pytest itself. Minor releases
    are folded in routinely; security fixes within two weeks.
    Run `rstest -n 0` first: it
    surfaces version incompatibilities exactly as a pytest upgrade would.

### Why 9, not 8

The 8→9 gap is unusually small for a major bump, which is why rstest
vendors 9. pytest 9 is a **cleanup major**, not a redesign: it removes
APIs that already emitted deprecation warnings throughout the 8.x line and
keeps the same collection model, fixture engine, `_pytest.*` import paths,
and plugin/`pluggy` hook contract. The runtime requirements are
effectively the same as 8.x (same supported-CPython line, same core
dependencies), so vendoring 9 doesn't raise the bar to adopt rstest beyond
what running pytest 8 already required.

What that means in practice:

- A suite that runs clean on a recent pytest 8.x with **no deprecation
  warnings** is almost always already pytest-9-clean: the removed APIs are
  exactly the ones 8.x was warning you about.
- The realistic migration cost is auditing those warnings, not rewriting
  tests. `rstest -n 0` (or `pytest -W error::pytest.PytestDeprecationWarning`
  on your current pytest first) surfaces them.
- Vendoring 8 would buy almost nothing (the same suites pass on both) while
  immediately leaving rstest a major version behind upstream. Tracking 9
  forward keeps the vendored core current for the same near-zero cost.

If your suite is *not* yet warning-clean on pytest 8.x, treat the rstest
switch as "clear pytest deprecations first, then change one command": the
same upgrade you'd owe pytest itself within a release or two anyway.
[Upgrading to pytest 9](../guides/upgrade-to-pytest9.md) is the
step-by-step for clearing them, including the tiny 9.0.x → 9.1.1 delta.

### Plugin versions vs the vendored core

The same rule applies to your **plugins**, and it is the most common source
of confusion. A plugin loads *into* the vendored core, so `import pytest`
inside it resolves to the vendored 9.1.1, not to whatever pytest is installed
in your environment. Two consequences:

- **The plugin's own code must support pytest 9.** Its compatibility with
  rstest is exactly its compatibility with pytest 9: if it calls an API that
  pytest 9 removed, it breaks under rstest just as it would under a real
  pytest-9 upgrade. A plugin's entry in the [top-100 matrix](../reference/top-100-plugins.md)
  reflects a version that already supports pytest 9.
- **A `pytest<9` pin is inert at runtime.** Such a pin is a packaging
  constraint that pip enforces at install time only. It does not change which
  pytest the plugin sees once a worker is running, and rstest never consults
  it, so a plugin pinned to `pytest<9` still executes against the vendored 9.

rstest does not maintain a per-plugin minimum-version table. Instead,
`rstest -n 0` runs your installed plugins against the vendored core in one
session and surfaces any pytest-9 incompatibility exactly as a real upgrade
would. Clear it there before scaling to workers. For the common stack see
[Your plugin stack](../guides/plugin-stack.md); for the deprecation audit see
[Upgrading to pytest 9](../guides/upgrade-to-pytest9.md).

## Measured at scale

Beyond the four-suite battery, the public-suite corpus runs rstest
against 33 well-known projects. The one that matters for advanced
xdist users: **SQLAlchemy** (about 25,300 tests) runs at `-n auto` with its
master-side hooks exercised end-to-end: `pytest_configure_node`
filling `follower_ident`, follower databases provisioned per worker,
`pytest_testnodedown` dropping them. Parity against its serial pytest run
is about 99.97%: 7 IMV/RETURNING tests skip in the serial baseline (they
depend on full-suite order) but pass under any parallel runner, real
pytest-xdist included; see
[Parity divergences §9](../reference/parity-divergences.md#9-order-dependent-serial-baseline).
Scope honestly stated: the default **SQLite** backend, crash-free; Postgres/MySQL backends and
crash-during-provisioning behavior are not yet in the battery (tests
requiring live services or absent optional dependencies fail
identically under vanilla pytest).

## Known gaps

Honest list, maintained as things close:

| Gap | Status |
|---|---|
| Windows at corpus scale | supported: the full gate runs on `windows-latest` in CI every commit and wheels are smoke-tested there; the 33-suite public corpus, however, is run only on macOS/Linux, so large-real-world-suite validation on Windows is lighter than on the other platforms |
| Terminal-rendering plugins (pytest-sugar, pytest-rich UIs) | by design at `-n ≥ 2`: rstest owns the terminal; data-level plugin behavior unaffected |
| hypothesis's shared `.hypothesis` example database under many workers | untested at high worker counts; hypothesis itself handles concurrent DB access, but rstest has not verified it beyond `-n 8`. Mitigation if you hit contention: in a `settings` profile give each worker its own DB (`database=DirectoryBasedExampleDatabase(f".hypothesis/{os.environ.get('RSTEST_WORKER_ID', 'main')}")`) or set `database=None` in CI to disable it entirely |
| `--sw` (stepwise, `--stepwise-skip`, `--stepwise-reset`) | runs in a single pytest session automatically (like `--pdb`/`-s`/`--co`): the vendored stepwise plugin owns resume/stop and its `cache/stepwise` round-trips exactly as upstream. Sequential by nature: stop-at-first-failure + resume-from-a-single-cursor has no meaning under split, duration-ordered parallel dispatch, so it does not run at `-n ≥ 2`. Same constraint as xdist. |
| xdist master-side hooks (`pytest_configure_node` and friends) | emulated for hooks that are per-node-stateless (read `gateway.id`, fill `node.workerinput`: SQLAlchemy's pattern, measured). Structural divergences from a single xdist controller: the hooks run N times concurrently in N processes (controller-side shared state needs rework), and crashed-node `pytest_testnodedown` runs on a survivor without the dead node's configure-time state. Details: [xdist hook emulation](xdist-hooks.md). |
| Plugins needing a controller-side service *shared* across all workers | rstest runs no central controller, so a plugin that needs one shared service for the whole pool isn't emulated. The known ecosystem cases are instead handled per worker: pytest-retry's branch self-provisions its own report server per worker (its `server_port` is set locally, no master needed) and pytest-rerunfailures is neutralized in favor of native `--reruns`, both work at `-n ≥ 2`. See [parity divergences §8](../reference/parity-divergences.md#8-plugin-master-hook-gating-rstest-side-fixed). |
| Time-derived parametrize IDs (`now()` in `@pytest.mark.parametrize`) | collection runs once per worker, so time-dependent IDs differ between workers; rstest detects the mismatch and refuses to dispatch rather than misattribute results: use stable IDs or `-n 0` (same constraint as xdist) |
| Plugins that need a single master process to aggregate worker output into one artifact (pytest-html) | pytest-html registers its report writer only on a node *without* `workerinput` (its xdist master check); every rstest worker has one, so at `-n ≥ 2` no writer is registered and an `--html` that reaches the plugin (via `addopts` or after `--`) silently produces nothing (no crash). Merging all workers into one file needs a master process rstest doesn't run. A command-line `--html` is rstest's native merged report at every worker count; for pytest-html's own report, run `rstest -n 0 -- --html=...`. (Formerly this row also listed pytest-rerunfailures/`sock_port` and pytest-retry/`server_port`, both now handled, and claimed a pytest-html `TypeError`: that path is fixed by signature-aware node-hook dispatch; pytest-randomly's derivable `randomly_seed` is synthesized.) Full per-plugin table in [Plugins](../guides/plugins.md#tested-compatibility) |

Found a difference not listed here? That's a bug report we want.
