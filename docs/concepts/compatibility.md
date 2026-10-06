# Compatibility

What rstest promises about matching pytest's behavior, how that promise is measured against real suites, and the known gaps.

## The contract

1. **At `-n 0`: pytest's exact outcomes.** [Single-worker mode](#single-worker-mode)
   runs one vendored-pytest session over your arguments. Any difference in
   per-test outcomes at `-n 0` is a bug in rstest. The named exceptions are
   the [flags rstest shares with pytest or a plugin](../reference/cli.md#shadowed-flags)
   (`--junitxml`, `--html`, `--timeout`, `--reruns`, `--debug`, ...): rstest
   handles them itself at every worker count, so they behave the same at
   `-n 0` as in parallel rather than as in pytest. `@pytest.mark.timeout` is
   rstest's native timeout too (its SIGALRM timer is armed for marked tests
   even without `--timeout`). To give one of these flags to pytest instead,
   pass it after `--`.
2. **In parallel modes: outcomes preserved for parallel-safe tests.**
   Identical per-test outcomes (setup/call/teardown, skips, xfails) for
   tests without hidden timing/ordering/shared-state assumptions. Tests
   *with* such assumptions can flake under concurrency (the same class of
   flake pytest-xdist produces) and the
   [parallel safety](../guides/parallel-safety.md) rails exist for them.

## Single-worker mode

Single-worker mode is what `-n 0` and `-n 1` run, and what `-n auto` runs
when it resolves to one worker. The two flags are identical: one Python
process in your interpreter runs a single pytest session over your
arguments, with no scheduling, no dispatch and no `[gwN]` worker identity.
It is the compatibility anchor, and its output is byte-exact:

- With no `--output` set, the terminal output is pytest's own, with
  rstest's extras (doctor report, coverage report, quarantined failures,
  gate messages) appended after pytest's summary line. An explicit
  [`--output`](../reference/cli.md#output)
  switches back to rstest's renderer.
- `--junitxml` is pytest's own document, plus rstest's `flaky` /
  `quarantined` properties (true at every worker count; see
  [`--junitxml`](../reference/cli.md#-junitxml-path)).

The flags that need pytest's own terminal or stdin switch any run to this
mode automatically, whatever `-n` says (rstest calls this **passthrough**):

- `--co` / `--collect-only`
- `-s` (also clustered, as in `-sv`) and `--capture=...`
- `--pdb` and `--trace`
- `--sw` / `--stepwise`, `--sw-skip` / `--stepwise-skip`,
  `--sw-reset` / `--stepwise-reset`
- rstest's own `--debug`

There is no worker identity below `-n 2`, unlike pytest-xdist, whose `-n 1`
spawns a `gw0` worker (see [xdist migration](../guides/migrate-from-xdist.md)).

One opt-in exception: passing [`--reruns`](../reference/cli.md#-reruns-n)
runs `-n 0`/`-n 1` as a one-worker pool instead (worker `gw0`, rstest's
renderer) so retries fire, trading byte-exact output for the reruns you
asked for. Under a passthrough flag `--reruns` has no effect, and rstest
warns about it. [Architecture](architecture.md#single-worker-mode) shows
where this mode sits in the run.

## What "verified" means

Compatibility is measured, not asserted: rstest's battery runs four real
suites, pandas (193,843 tests), aiohttp, django-allauth and rich, under
pytest and under rstest, and diffs **per-test outcomes** (every phase,
every skip reason class, xfail flags). Their real plugins are loaded:
pytest-django, pytest-asyncio, pytest-aiohttp, hypothesis, pytest-mock,
pytest-cov. The recorded runs in
[benchmarks](../reference/benchmarks.md) use `-n 8` for all four
(django-allauth's recommended count is `-n 4`, because its
rate-limit-window tests can flake at high worker counts).

pandas, django-allauth and rich measured 100% parity on the recorded runs;
aiohttp measured 99.91-99.98%, from a socket-leak warning flake that moves
under any parallel runner, xdist included. rich and django-allauth also
contain tests that flake *under plain pytest itself*, so an individual run
can land at about 99.x% when the baseline and rstest draw different flakes.
Every such case is catalogued in
[Parity divergences](../reference/parity-divergences.md).

Summary-line accounting (passed/failed/skipped/xfailed/warnings counts)
matches pytest's numbers on the same suites.

## Vendored pytest version

rstest currently vendors **pytest 9.1.1**, unmodified. Policy:

- The vendored version is pinned per rstest release and stated in
  [License](../reference/license.md) and `rstest_worker._vendor`.
- Upstream pytest minor releases are adopted by re-vendoring verbatim and
  rerunning the full compatibility battery.
- **Security fixes**: when upstream pytest ships a security fix affecting
  the vendored code, an rstest release with the re-vendored core is
  expected **within two weeks** of the upstream release. Because the
  vendored tree is verbatim, re-vendoring is mechanical; the two-week
  budget covers the compatibility battery, not the patch.
- Local modifications to the vendored tree are forbidden; integration
  lives in `rstest_worker` around it.

!!! note "If your suite is pinned to an older pytest"
    Adopting rstest adopts pytest 9 whatever pytest is installed (see
    [Your suite runs on pytest 9](../getting-started/installation.md#your-suite-runs-on-pytest-9)).
    There is currently no older-core build, and none is planned: one
    vendored core, tracked forward. When a new pytest **major** ships, the
    core is re-vendored after the early point releases stabilize: the
    same timing a cautious team upgrades pytest itself. Minor releases
    are folded in routinely; security fixes within two weeks.

### Why 9, not 8

The 8→9 gap is unusually small for a major bump, which is why rstest
vendors 9. pytest 9 is a **cleanup major**, not a redesign: it removes
APIs that already emitted deprecation warnings throughout the 8.x line and
keeps the same collection model, fixture engine, `_pytest.*` import paths,
and plugin/`pluggy` hook contract. The runtime requirements are
effectively the same as 8.x (same supported-CPython line, same core
dependencies), so vendoring 9 doesn't raise the bar to adopt rstest beyond
what running pytest 8 already required.

For what that means for your suite (warning-clean on 8.x is almost always
9-clean), see
[Your suite runs on pytest 9](../getting-started/installation.md#your-suite-runs-on-pytest-9).
Vendoring 8 would buy almost nothing (the same suites pass on both) while
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
inside it resolves to vendored pytest 9.1.1, not to whatever pytest is installed
in your environment. Two consequences:

- **The plugin's own code must support pytest 9.** Its compatibility with
  rstest is exactly its compatibility with pytest 9: if it calls an API that
  pytest 9 removed, it breaks under rstest just as it would under a real
  pytest-9 upgrade. A plugin's entry in the [top-100 matrix](../reference/top-100-plugins.md)
  reflects a version that already supports pytest 9.
- **A `pytest<9` pin is inert at runtime.** Such a pin is a packaging
  constraint that pip enforces at install time only. It does not change which
  pytest the plugin sees once a worker is running, so a plugin pinned to
  `pytest<9` still executes against vendored pytest 9.1.1. rstest reads each loaded
  plugin's `Requires-Dist` on pytest and, when it excludes the running pytest,
  prints one `rstest: warning: <plugin> <version> requires pytest<9, ...` line
  per plugin to stderr, once per run, telling you the pin is not enforced and
  to upgrade the plugin if it misbehaves. Requirements gated behind an extra
  (such as hypothesis's `[pytest]`) are ignored. The run itself is unaffected.

rstest does not maintain a per-plugin minimum-version table. Instead,
`rstest -n 0` runs your installed plugins against the vendored core in one
session and surfaces any pytest-9 incompatibility exactly as a real upgrade
would. Clear it there before scaling to workers. For the common stack see
[Your plugin stack](../guides/plugin-stack.md); for the deprecation audit see
[Upgrading to pytest 9](../guides/upgrade-to-pytest9.md).

## Measured at scale

Beyond the four-suite battery, the public-suite corpus runs rstest
against 33 well-known projects (the parity suites in
`corpus/suites.toml`; its other two entries, sympy and scikit-learn, are
benchmark-only). The one that matters for advanced
xdist users: **SQLAlchemy** (about 25,300 tests) runs at `-n auto` with its
controller-side hooks exercised end-to-end: `pytest_configure_node`
filling `follower_ident`, follower databases provisioned per worker,
`pytest_testnodedown` dropping them. Parity against its serial pytest run
is about 99.97%: 7 IMV/RETURNING tests skip in the serial baseline (they
depend on full-suite order) but pass under any parallel runner, real
pytest-xdist included; see
[Parity divergences §9](../reference/parity-divergences.md#9-order-dependent-serial-baseline).
Scope: the default **SQLite** backend, crash-free; Postgres/MySQL backends and
crash-during-provisioning behavior are not yet in the battery (tests
requiring live services or absent optional dependencies fail
identically under vanilla pytest).

## Unstable parametrize ids { #unstable-parametrize-ids }

The most common thing that blocks a parallel run is a test id that differs
from one collection to the next. Under full collection (every cold-cache run,
and any suite below the [lazy](lazy-collection.md) auto threshold) each
worker collects the whole suite and reports a count and hash of its nodeids.
If any worker disagrees, rstest refuses to dispatch rather than misattribute
results, and exits with:

```text
workers collected different test sets (N vs M items); cannot dispatch safely.
```

There is no automatic fallback; pytest-xdist has the same constraint. The
usual sources:

- **Per-process values in a `parametrize` id**: a `repr()` fallback that
  embeds a memory address (`0x...`), a `uuid4()`, a random value, or a
  sub-second timestamp. These differ in every worker, so the pool never
  starts.
- **Second-resolution timestamps** (`now()` in the parameter list). Workers
  collect within the same second, so these usually match and the run works,
  but a collection that straddles a second boundary fails intermittently
  (marshmallow runs 100% at `-n auto`; see
  [parity divergences §3](../reference/parity-divergences.md#3-run-dependent-nodeids-now-resolved)).
- **Order, not content**: a `parametrize` over a `set` of strings yields the
  same ids in a different order per process, because string hashing follows
  `PYTHONHASHSEED`, which is random per process.
- A randomizing plugin, such as pytest-randomly without a fixed seed.

Lazy collection collects each file once and compares nothing, so a large
warm-cache run may pass, but the next cold-cache run collects in full and
refuses: fix the ids anyway.

**Fix:** give the `parametrize` a stable `ids=` (for example
`ids=[c.name for c in cases]`), iterate a list or `sorted(...)` instead of a
set (or pin `PYTHONHASHSEED` for the whole run), seed or disable the
randomizing plugin (`-p no:randomly`), or run `-n 0`.
[`rstest migrate-check`](../reference/cli-commands.md#migrate-check) collects
twice before your first parallel run and names each unstable site, with its
class.

## Known gaps

Maintained as things close:

| Gap | Status | Notes |
|---|---|---|
| Windows at corpus scale | supported, lighter validation | [1](#gap-windows) |
| Terminal-rendering plugins (pytest-sugar, pytest-rich UIs) | by design at `-n ≥ 2` | [2](#gap-terminal) |
| hypothesis's shared `.hypothesis` database under many workers | untested above `-n 8` | [3](#gap-hypothesis) |
| `--sw` / stepwise flags | single process only | [4](#gap-stepwise) |
| xdist controller-side hooks (`pytest_configure_node` and friends) | emulated per worker | [5](#gap-controller-hooks) |
| Plugins needing one controller-side service for the whole pool | not emulated; known cases handled per worker | [6](#gap-shared-service) |
| Time-derived or random parametrize ids | pool refuses to dispatch | [7](#gap-parametrize-ids) |
| Plugins that aggregate worker output into one artifact (pytest-html) | writes nothing at `-n ≥ 2` | [8](#gap-pytest-html) |

1. **Windows at corpus scale.**{ #gap-windows } The full gate runs on
   `windows-latest` in CI on every commit and wheels are smoke-tested there,
   but the 33-suite public corpus runs only on macOS/Linux, so
   large-real-world-suite validation on Windows is lighter than on the other
   platforms.
2. **Terminal-rendering plugins.**{ #gap-terminal } At `-n ≥ 2` rstest owns
   the terminal, so plugin-drawn UIs don't paint. Data-level plugin behavior
   is unaffected.
3. **hypothesis example database.**{ #gap-hypothesis } hypothesis handles
   concurrent access to its database itself, but rstest has not verified it
   beyond `-n 8`. If you hit contention, give each worker its own database in
   a `settings` profile
   (`database=DirectoryBasedExampleDatabase(f".hypothesis/{os.environ.get('PYTEST_XDIST_WORKER', 'master')}")`),
   or set `database=None` in CI to disable it.
4. **Stepwise.**{ #gap-stepwise } `--sw`, `--stepwise-skip` and
   `--stepwise-reset` run in a single pytest session automatically (like
   `--pdb`, `-s` or `--co`): the vendored stepwise plugin owns resume and
   stop, and its `cache/stepwise` round-trips exactly as upstream. Stopping at
   the first failure and resuming from one cursor has no meaning under split,
   duration-ordered parallel dispatch, so it does not run at `-n ≥ 2`. Same
   constraint as xdist.
5. **Controller-side hooks.**{ #gap-controller-hooks } Emulated for hooks
   that are per-node-stateless (read `gateway.id`, fill `node.workerinput`:
   SQLAlchemy's pattern, measured). Structural differences from a single
   xdist controller: the hooks run N times concurrently in N processes, so
   controller-side shared state needs rework, and a crashed node's
   `pytest_testnodedown` runs on a survivor without the dead node's
   configure-time state. Details: [xdist hook emulation](xdist-hooks.md).
6. **Shared controller-side services.**{ #gap-shared-service } rstest runs no
   central controller, so a plugin that needs one service for the whole pool
   isn't emulated. The known cases are handled per worker:
   pytest-retry's branch self-provisions its own report server per worker
   (if that fails, rstest unregisters the plugin and falls back to native
   `--reruns`), and pytest-rerunfailures is neutralized in favor of native
   `--reruns`. Both work at `-n ≥ 2`. See
   [parity divergences §8](../reference/parity-divergences.md#8-plugin-controller-hook-gating-rstest-side-fixed).
7. **Parametrize ids.**{ #gap-parametrize-ids } See
   [Unstable parametrize ids](#unstable-parametrize-ids).
8. **pytest-html.**{ #gap-pytest-html } pytest-html registers its report
   writer only on a node without `workerinput` (its xdist controller check).
   Every rstest worker has one, so at `-n ≥ 2` an `--html` that reaches the
   plugin (via `addopts` or after `--`) silently produces nothing. A
   command-line `--html` is rstest's native merged report at every worker
   count; for pytest-html's own report, run `rstest -n 0 -- --html=...`. Full
   per-plugin table in [Plugins](../guides/plugins.md#tested-compatibility).

Found a difference not listed here? That's a bug report we want.
