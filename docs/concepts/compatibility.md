# Compatibility

## The contract

1. **At `-n 0`: byte-exact.** One vendored-pytest session over your
   arguments. Any behavioral difference at `-n 0` is a bug in rstest.
2. **In parallel modes: outcomes preserved for parallel-safe tests.**
   Identical per-test outcomes (setup/call/teardown, skips, xfails) for
   tests without hidden timing/ordering/shared-state assumptions. Tests
   *with* such assumptions can flake under concurrency — the same class of
   flake pytest-xdist produces — and the
   [parallel safety](../guides/parallel-safety.md) rails exist for them.

## What "verified" means

Compatibility is measured, not asserted: rstest's battery runs four real
suites — pandas (193,627 tests), aiohttp, django-allauth, rich — under
pytest and under rstest, and diffs **per-test outcomes** (every phase,
every skip reason class, xfail flags). All four measure 100% parity per run
at the worker counts in [benchmarks](../reference/benchmarks.md) (`-n 8` for
the big suites, `-n 4` for the small ones), re-run on every release, with
their real plugins loaded: pytest-django, pytest-asyncio, pytest-aiohttp,
hypothesis, pytest-mock, pytest-cov installed. (Two suites contain tests
that flake *under plain pytest itself* — rich, django-allauth — so on some
runs the baseline and rstest disagree at ~99.x%; every such case is
catalogued in [Parity divergences](../reference/parity-divergences.md).)

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
    core is re-vendored after the early point releases stabilize — the
    same timing a cautious team upgrades pytest itself. Minor releases
    are folded in routinely; security fixes within two weeks.
    Run `rstest -n 0` first — it
    surfaces version incompatibilities exactly as a pytest upgrade would.

### Why 9, not 8

The 8→9 gap is unusually small for a major bump, which is why rstest
vendors 9. pytest 9 is a **cleanup major**, not a redesign: it removes
APIs that already emitted deprecation warnings throughout the 8.x line and
keeps the same collection model, fixture engine, `_pytest.*` import paths,
and plugin/`pluggy` hook contract. The runtime requirements are
effectively the same as 8.x — same supported-CPython line, same core
dependencies — so vendoring 9 doesn't raise the bar to adopt rstest beyond
what running pytest 8 already required.

What that means in practice:

- A suite that runs clean on a recent pytest 8.x with **no deprecation
  warnings** is almost always already pytest-9-clean — the removed APIs are
  exactly the ones 8.x was warning you about.
- The realistic migration cost is auditing those warnings, not rewriting
  tests. `rstest -n 0` (or `pytest -W error::DeprecationWarning` on your
  current pytest first) surfaces them.
- Vendoring 8 would buy almost nothing — the same suites pass on both — while
  immediately leaving rstest a major version behind upstream. Tracking 9
  forward keeps the vendored core current for the same near-zero cost.

If your suite is *not* yet warning-clean on pytest 8.x, treat the rstest
switch as "clear pytest deprecations first, then change one command" — the
same upgrade you'd owe pytest itself within a release or two anyway.

## Measured at scale

Beyond the four-suite battery, the public-suite corpus runs rstest
against 31 well-known projects. The one that matters for advanced
xdist users: **SQLAlchemy** (25,300 tests) runs at `-n 4` with its
master-side hooks exercised end-to-end — `pytest_configure_node`
filling `follower_ident`, follower databases provisioned per worker,
`pytest_testnodedown` dropping them — with outcomes identical to its
serial pytest run. Scope honestly stated: the default **SQLite**
backend at `-n 4`, crash-free; Postgres/MySQL backends and
crash-during-provisioning behavior are not yet in the battery (tests
requiring live services or absent optional dependencies fail
identically under vanilla pytest).

## Known gaps

Honest list, maintained as things close:

| Gap | Status |
|---|---|
| Windows at corpus scale | supported — the full gate runs on `windows-latest` in CI every commit and wheels are smoke-tested there; the 31-suite public corpus, however, is run only on macOS/Linux, so large-real-world-suite validation on Windows is lighter than on the other platforms |
| Terminal-rendering plugins (pytest-sugar, pytest-rich UIs) | by design at `-n ≥ 2` — rstest owns the terminal; data-level plugin behavior unaffected |
| hypothesis's shared `.hypothesis` example database under many workers | untested at high worker counts; hypothesis itself handles concurrent DB access, but rstest has not verified it beyond `-n 8`. Mitigation if you hit contention: in a `settings` profile give each worker its own DB — `database=DirectoryBasedExampleDatabase(f".hypothesis/{os.environ.get('RSTEST_WORKER_ID', 'main')}")` — or set `database=None` in CI to disable it entirely |
| `--sw` (stepwise, `--stepwise-skip`, `--stepwise-reset`) | runs in a single pytest session automatically (like `--pdb`/`-s`/`--co`) — the vendored stepwise plugin owns resume/stop and its `cache/stepwise` round-trips exactly as upstream. Sequential by nature: stop-at-first-failure + resume-from-a-single-cursor has no meaning under split, duration-ordered parallel dispatch, so it does not run at `-n ≥ 2`. Same constraint as xdist. |
| xdist master-side hooks (`pytest_configure_node` and friends) | emulated for hooks that are per-node-stateless (read `gateway.id`, fill `node.workerinput` — SQLAlchemy's pattern, measured). Structural divergences from a single xdist controller: the hooks run N times concurrently in N processes (controller-side shared state needs rework), and crashed-node `pytest_testnodedown` runs on a survivor without the dead node's configure-time state. Details: [xdist hook emulation](xdist-hooks.md). |
| Plugins needing a controller-side service the worker connects to (e.g. pytest-retry's report server: workers read `workerinput["server_port"]`) | not emulated — rstest fills `workerinput` but runs no central controller process, so the key is absent and the plugin raises at configure under `-n ≥ 2`. Same root as the master-side-hook gap. Run such a project at `-n 0` (the plugin takes its non-xdist path); measured on langgraph's `checkpoint-sqlite`, see [Benchmarks](../reference/benchmarks.md#monorepo). |
| Time-derived parametrize IDs (`now()` in `@pytest.mark.parametrize`) | collection runs once per worker, so time-dependent IDs differ between workers; rstest detects the mismatch and refuses to dispatch rather than misattribute results — use stable IDs or `-n 0` (same constraint as xdist) |
| Plugins that crash at `-n ≥ 2` (pytest-rerunfailures with xdist installed, pytest-html) | they read xdist-master-injected `workerinput` keys backed by a controller-side service that rstest doesn't run — use rstest's native equivalents or run at `-n 0`. (pytest-randomly's `randomly_seed` is derivable, so rstest now synthesizes it — that plugin works under the pool.) Full per-plugin table in [Plugins](../guides/plugins.md#tested-compatibility) |

Found a difference not listed here? That's a bug report we want.
