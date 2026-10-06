# CLI flags

rstest owns the flags listed on this page, grouped by topic below. Two
forwarded pytest flags also get their own sections because rstest adds
orchestration to them (`--doctest-modules` and `--durations` /
`--durations-min`, each marked *forwarded*); `-x` / `--maxfail`, `--lf` /
`--ff` and `-v` are covered under [Forwarded pytest flags](#forwarded-pytest-flags).
**Everything else forwards to the test session verbatim**, so the rest of the
pytest flag surface, including flags added by your plugins, works without
translation.

```text
rstest [RSTEST FLAGS] [PATHS] [PYTEST FLAGS]
rstest <COMMAND> [OPTIONS]
```

!!! warning "Owned flags that shadow plugin or pytest flags"
    A few rstest-owned flags share a name with a pytest or plugin flag. rstest
    consumes them, so the plugin never sees them (put one after `--` to hand
    it to pytest or the plugin instead):
    { #shadowed-flags }

    --8<-- "docs/_snippets/shadowed-flags.md"

    **Owned flags are read only from the command line and
    [`[tool.rstest]`](#configuration-file).** An owned flag placed in pytest's
    `addopts` (ini / `[tool.pytest.ini_options]`) or in `PYTEST_ADDOPTS` never
    reaches rstest: it goes to the pytest session inside each worker. For
    example `addopts = --reruns 2` gives no reruns under the pool, and
    `addopts = --junitxml=x.xml` writes no file at `-n 2` or more. Move these
    flags to the command line or `[tool.rstest]`.

Subcommands (`try`, `migrate-check`, `xdist-removal-check`, `audit`,
`bisect`, `replay`, `shard-verify`, `explain`, `cache-compact`,
`verify-vendor`, `install-skills`) and their flags are on
[CLI subcommands](cli-commands.md).

At a [monorepo](../concepts/monorepo.md) root, rstest forwards most of these
flags to every project, gives output files a per-project name, and refuses the
few that need one project's session. The per-flag table is in
[Flags at a monorepo root](../concepts/monorepo.md#flags-at-a-monorepo-root).

## Flag summary

Every rstest-owned flag, with its default and the config key or environment
variable that also sets it. The command line wins over both. A blank cell
means none.

| Flag | Purpose | Default | `[tool.rstest]` key | Env var |
|---|---|---|---|---|
| [`-n, --numprocesses`](#-n-numprocesses-nauto) | worker count | `auto` | `numprocesses` | |
| [`--fork-pool`](#-fork-pool) | fork workers off a prewarmed zygote (Unix) | off | | |
| [`--dist`](#-dist-loadloadfileloadscopeloadgroupeach) | distribution mode | `load` | `dist` | |
| [`--order`](#-order-throughputfail-fast) | dispatch order under `--dist load` | `throughput` (`fail-fast` under `--watch`) | `order` | |
| [`--shuffle[=SEED]`](#-shuffleseed) | seeded random dispatch order | off | | |
| [`--shard K/N`](#-shard-kn) | run one of N CI shards | off | | |
| [`--collect`](#-collect-fulllazy) | collection strategy (`full` / `lazy`) | auto | `collect` | |
| [`--changed[=REV]`](#-changedrev) | run only tests affected by changes | off | | |
| [`--changed-strict`](#-changed-strict) | `--changed` hardened for gating CI | off | | |
| [`--since-green`](#-since-green) | `--changed` against the last green commit | off | | |
| [`--incremental`](#-incremental) | skip unchanged tests that passed last run | off | | |
| [`--reruns N`](#-reruns-n) | rerun failed tests | `0` | `reruns` | |
| [`--only-rerun REGEX`](#-only-rerun-regex) | rerun only matching failures | every failure | | |
| [`--reruns-only-known-flaky`](#-reruns-only-known-flaky) | rerun only tests with flaky history | off | `reruns-only-known-flaky` | |
| [`--quarantine FILE`](#-quarantine-file) | make listed tests' failures non-fatal | none | | |
| [`--timeout SECS`](#-timeout-secs) | per-test call-phase deadline | none | | |
| [`--worker-timeout SECS`](#-worker-timeout-secs) | fixed per-test hang backstop | none (per-test watchdog when a test has a timeout) | `worker-timeout` | |
| [`--durations-regress RATIO`](#-durations-regress-ratio) | fail on per-test duration regressions | off | | |
| [`--require-baseline`](#-require-baseline) | missing duration baseline is an error | off | | |
| [`--cache-remote URL`](#-cache-remote-urldir-cache-pull-cache-push) | shared cache location | none | | `RSTEST_CACHE_REMOTE` |
| [`--cache-pull` / `--cache-push`](#-cache-remote-urldir-cache-pull-cache-push) | merge / publish the shared cache | off | | |
| [`--cache-compact-threshold N`](#-cache-compact-threshold-n) | compact the remote inline after a push | off | | `RSTEST_CACHE_COMPACT_THRESHOLD` |
| [`--doctor`](#-doctor) | print a post-run diagnosis | off | | |
| [`--doctor-json PATH`](#-doctor-json-path) | write the diagnosis as JSON | none | | |
| [`--doctor-md PATH`](#-doctor-md-path) | write the diagnosis as markdown | none | | |
| [`--doctor-fail-on COND`](#-doctor-fail-on-cond) | fail on a doctor metric threshold | none | | |
| [`--fail-on-leak`](#-fail-on-leak) | fail on a leaked thread or fd | off | | |
| [`--cov-diff-fail-under PCT`](#-cov-diff-fail-under-pct) | fail on low diff coverage | none | | |
| [`--cov-diff-json PATH`](#-cov-diff-json-path) | write diff coverage as JSON | none | | |
| [`--output STYLE`](#output) | terminal output style | `bar` on a TTY, else `dots`; pytest's own in single-worker mode | `output` | |
| [`--junitxml PATH`](#-junitxml-path) | merged JUnit XML | none | | |
| [`--html PATH`](#-html-path) | self-contained HTML report | none | | |
| [`--report-json PATH`](#-report-json-path) | per-test outcome snapshot | none | | |
| [`--stream-json FILE`](#-stream-json-file) | live NDJSON events to a side file | none | | |
| [`--python`](#-python-path-or-version) | worker interpreter | discovered | | `VIRTUAL_ENV` (first in discovery) |
| [`--watch`](#-watch) | rerun on file change | off | | |
| [`--debug[=PORT]`](#-debugport) | run under debugpy | off (port `5678` when bare) | | |

`[tool.rstest] projects` (monorepo subprojects) is a config key with no flag;
see [Configuration file](#configuration-file). Flags of the subcommands
(`--migrate-check-json`, `--audit-json`, ...) are on [CLI subcommands](cli-commands.md).

## Parallelism and scheduling

### `-n, --numprocesses <N|auto>`

Worker count. Default `auto`.

- `-n auto`: one worker per available logical core, capped by what the
  selection (the paths and nodeids you pass, else the invocation directory or
  `testpaths` from the rootdir) can use. With no cached timings, at most one
  worker per selected test file or nodeid, so one selected test runs in
  single-worker mode. With a warm duration cache, at most the cached test
  count (`--dist load` splits a file across workers) and one worker per ~2 s
  of cached time. On Linux the core count honors the CPU affinity mask and
  cgroup quota, so a CPU-limited container sees its allocation.
- `-n 4`: four workers.
- `-n 0` or `-n 1`: **single-worker mode**, one pytest session with
  byte-exact pytest semantics and no worker identity; `-n auto` runs this
  mode when it resolves to one worker. With `--reruns`, `-n 0/1` runs a
  one-worker pool instead (worker `gw0`, not byte-exact); see
  [`--reruns`](#-reruns-n). With no `--output` set, the session prints
  pytest's own terminal output (see [`--output`](#output)). See
  [Single-worker mode](../concepts/compatibility.md#single-worker-mode) (and, for
  migrators, how it differs from pytest-xdist's `-n 1`).

**An explicit `-n <k>` is not capped by core count**; only `auto` caps. That
is the knob for **wait-bound suites** (IO, sleeps, network, timeouts): a
waiting worker holds no core, so `-n 16` on 8 cores overlaps more waits,
which `auto` never does (see the
[wait-bound playbook](../guides/wait-bound.md#2-tune-the-worker-count)).
Load-sensitive suites (tight timing assertions) may need `-n` *below* cores;
see [Parallel safety](../guides/parallel-safety.md#choosing-the-worker-count).
Because `auto` can resolve to one worker (one test file, or a warm cache under
~2 s) and [`--shard`](#-shard-kn) refuses one worker, pass `-n 2` or higher
with `--shard`.

`-s`, `--capture=…`, `--pdb`, `--trace`, `--debug` and the stepwise flags
override `-n` and run one process (rstest warns when you set `-n` above 1);
see [Passthrough-IO flags](#passthrough-io-flags).

### `--fork-pool`

Fork-prewarm the worker pool. **Unix only**, off by default.

Normally each worker is a fresh `python` process that re-imports rstest's
vendored pytest core, so at high `-n` that import is paid once per worker.
With `--fork-pool` a single zygote imports the core **once** and `fork()`s the
workers off it; only the app/conftest imports (per-worker anyway, at
collection) are paid N times.

- Only the vendored core is shared. Each child still sets its own `gwN`
  identity, collects independently, and owns its coverage file and temp dir.
- Only the **initial** pool is forked; a crash-respawned worker uses the
  normal spawn path.
- Accepted everywhere, but a no-op on Windows, in single-worker runs
  (`-n 0`/`-n 1`), and under `-s`/`--pdb`/`--co` passthrough.

The saving grows with `-n` and core contention: largest on short or cold
suites, negligible on compute-heavy ones. The `--doctor` `startup:` line shows
whether it's worth it.

### `--dist <load|loadfile|loadscope|loadgroup|each>`

Distribution mode. Default `load`. All five are pytest-xdist mode names.

- `load`: test-granular, dynamic and duration-aware. Cached slow tests
  dispatch first and individually; the rest flows in contiguous chunks that
  preserve module-fixture locality.
- `loadfile`: whole files stay on one worker, in file order. For
  order-dependent suites.
- `loadscope`: fixture-scope affinity. A class's tests stay together,
  module-level functions stay with their module. For expensive class/module
  fixtures that must not duplicate.
- `loadgroup`: `@pytest.mark.xdist_group("name")` affinity, across files;
  unmarked tests distribute individually.
- `each`: every worker runs the **full** suite. Counts are per-worker totals
  and outcomes are keyed `nodeid [gwN]`; `--reruns` is rejected (rerunning
  would mask the per-worker differences `each` exists to expose); the
  duration cache is not updated. Every worker uses the same interpreter, so
  this validates isolation and shakes out flakiness; xdist's
  heterogeneous-environment use (`--tx` gateways) has no rstest equivalent.

### `--order <throughput|fail-fast>`

Dispatch **ordering** within `--dist load` (the other dist modes keep their
affinity order).

- `throughput` (default): slowest cached tests first, individually, to pack
  workers for the best wall-clock time.
- `fail-fast`: earliest **red** signal first. Tests that hard-failed lead
  (most recent failure first), then flaky tests (most recent flake first),
  both read from `.rstest_cache/flakes.json`, each dispatched alone so they
  run in parallel. At most 128 lead, and tests matched by
  [`--quarantine`](#-quarantine-file) are never pulled forward. The rest
  follow in `throughput` order. Pair with
  [`--maxfail`/`-x`](#forwarded-pytest-flags) to stop at the first red. With
  a cold `flakes.json` it matches `throughput`.

**Auto:** with neither the flag nor `[tool.rstest] order` set, rstest picks
`fail-fast` under [`--watch`](#-watch) and `throughput` otherwise. Where an
explicit `fail-fast` has no effect (and warns), and why it can't combine with
`--shuffle`:
[Fail-fast ordering](../concepts/scheduling.md#fail-fast-ordering-order-fail-fast).
`failfast` and `fail_fast` are accepted spellings. Monorepo runs forward
`--order` to every project.

### `--shuffle[=SEED]`

Run tests in a seeded random order (the pytest-randomly idea, applied to the
orchestrator's dispatch queue) to flush out order dependence on demand. Without a value the seed is chosen per run
and printed. `--shuffle=SEED` reproduces the dispatch order but not which
worker runs which test (that depends on timing), so a polluter and its victim
can pair on one run and split on the next; for an exact repro use
[`rstest replay`](cli-commands.md#replay). Attach the seed with `=`:
`--shuffle 42` is a bare `--shuffle` plus a test path `42` (see
[Optional-value flags](#argument-splitting)).

Affinity modes (`loadfile`/`loadscope`/`loadgroup`) shuffle the group order
and keep in-group order. In `load` mode the shuffle replaces duration-aware
sequencing. Requires the parallel pool with full collection: single-worker
mode, `--collect lazy` and `--dist each` are refused, not silently ignored. At
a monorepo root one seed is shared by every project, so one `--shuffle=SEED`
reproduces the whole run.

### `--shard <K/N>`

Split the suite across `N` independent CI jobs and run only shard `K`
(1-based: `1/4` … `4/4`). Each job partitions the collected tests into `N`
buckets balanced by the duration cache (`.rstest_cache/durations.json`,
longest-processing-time-first; a cold cache splits by count). Jobs that see
the same collection and the same duration cache get disjoint buckets that
cover the whole suite, so the merged per-job JUnit is the full run. Jobs with
different caches (a shared cache that changes mid-matrix) can disagree;
verify with [`shard-verify`](cli-commands.md#shard-verify).

Orthogonal to `-n`: each shard still fans out across local workers. Requires
`-n 2` or more; rejected with `--shuffle` (a per-run shuffle breaks the
identical-partition guarantee) and `--dist each`. Under `--collect lazy` it
shards by file. Composes with `--changed` (select, then partition). Restore
the **same** duration cache on every job: see the
[Sharding guide](../guides/sharding.md).

## Test selection and collection

### `--collect <full|lazy>`

Collection strategy. `full`: every worker collects the whole suite
(identical sessions, hash-verified). `lazy`: each test file is collected
once, on one worker, on demand. Lazy wins on narrow `-k`/`-m` selections of
large suites; a full run of a suite with a few giant files prefers `full`, or
`lazy` with an explicit `--dist load` (flag or config), which enables
work-stealing (the implicit default keeps strict file affinity). Semantics and
the compatibility trade: [Lazy collection](../concepts/lazy-collection.md).
Config `[tool.rstest] collect`.

An explicit `--collect lazy` accepts only `--dist load` and `loadfile`;
`loadscope`, `loadgroup` and `each` exit 1:

```text
--collect lazy is file-affine and cannot honor --dist loadscope (only load/loadfile: loadscope/loadgroup need a global id list and each runs the full suite on every worker; use --collect full)
```

Nodeid and `--pyargs` arguments fall back to full collection.

**Default: auto.** With neither the flag nor the config key, rstest picks
`lazy` for large warm-cache parallel runs and `full` otherwise, and prints a
banner line when it picks `lazy`; auto never errors. The full rules:
[Lazy collection: Auto-default](../concepts/lazy-collection.md#auto-default).
Pass `--collect full` or `--collect lazy` to force either.

### `--doctest-modules`

*Forwarded pytest flag.* Works as in pytest: forwarded to the vendored core, which collects
doctest items from all modules; they dispatch across workers like any
other test. `--doctest-glob` and friends forward the same way.

### `--changed[=REV]`

Run only tests affected by changed files. Changes come from git: working tree
+ untracked vs `HEAD`, or vs `REV` (e.g. `--changed=origin/main` in CI). The
whole project's changes count (everything under its rootdir, even below the
git root), whichever subdirectory you start from. Attach `REV` with `=`:
`--changed origin/main` is a bare `--changed` plus a test path `origin/main`
(see [Optional-value flags](#argument-splitting)). How selection works,
keeping the index warm, and CI layouts: [Selecting changed tests](../guides/changed.md).

Selection, per changed file:

- **Coverage index, when warm.** If `.rstest_cache/coverage_index.json`
  exists (written by any
  [`--cov-context=test`](../guides/coverage.md#per-test-contexts-cov-contexttest)
  run), changed *lines* map to the tests whose recorded coverage executed
  them. Automatic, no flag.
- **Import graph** for what the index can't vouch for: new lines, files it
  never measured, untracked files, and everything when the cache is cold
  (then byte-identical to plain import-graph selection). Conservative:
  ambiguous module names select every match, function-local imports count,
  and a changed `conftest.py` (or any module it imports, directly or
  transitively) selects its whole subtree.
- A changed test file always runs its own tests; any config or non-Python
  change runs everything.

Over-selection is safe; the fallbacks never under-select. Known gap: dynamic
imports (`importlib.import_module`) produce no graph edges; for
correctness-critical runs use [`--changed-strict`](#-changed-strict).

The banner reports `N changed file(s) -> M of K mapped test(s) affected`
(`+ F whole-file target(s)` for import-graph and test-file targets). A cold
map with a changed non-test `.py` file prints a one-line hint to warm it with
`--cov --cov-context=test` (never for a full run or a test-only, `conftest.py`,
config or non-Python change). With nothing affected, the run prints
`no tests affected by N changed file(s)` and exits 0 without running.

**PR-aware in CI:** on a pull-request or merge-request job (GitHub Actions,
GitLab CI, Buildkite), bare `--changed` diffs against the merge-base with the
PR base branch instead of `HEAD`. The base must be in the clone, and an
unresolvable base is an error. An explicit `REV` disables auto-targeting. The
variables probed and the push-job pitfall are in
[Selecting changed tests: CI usage](../guides/changed.md#ci-usage).

### `--changed-strict`

`--changed` hardened for gating CI (merge queues). Implies `--changed` (vs
`HEAD`) when `--changed` isn't given. Three changes:

- **A changed source file the import graph can't connect to any test forces a
  full run** (naming the file) instead of selecting nothing for it, so
  dynamic-import, unused-module and deleted-file cases stop being false skips.
- **Monorepos: undeclared cross-project imports count as dependency edges**
  (with a warning naming both projects), catching the shared-workspace-venv
  trap. Namespace packages shared by several siblings over-connect, erring
  toward running more.
- **"Nothing affected" exits 5** (pytest's nothing-collected code) instead of
  0, so a pipeline must consciously allow it.

Residual risk: imports built at runtime from strings
(`importlib.import_module(f"plugins.{name}")`) still produce no edges; import
such modules from a test file, or keep full runs on the gating path.

### `--since-green`

Incremental testing against the last green run. rstest records the git commit
of the last fully-passing run in the cache (`last_green.json`) and uses it as
the [`--changed`](#-changedrev) base. The baseline advances **only when a run
is fully green**, so a failing test keeps being selected until it passes.

- The baseline advances only from a **clean working tree** (nothing
  uncommitted or untracked, as `--changed` sees it): a green run over local
  edits keeps the old baseline and says so on stderr. Commit, then run once
  more to advance it.
- First run (no baseline yet) runs everything.
- The baseline is keyed to an environment fingerprint: the interpreter, the
  content of `uv.lock`, `poetry.lock`, `pdm.lock` and `requirements.txt`, and
  the set of installed distributions in the venv (so a `pip install -U` with
  no lockfile change counts). A change to any of them runs everything once.
- Ignored when `--changed` is given explicitly. Mutually exclusive with
  `--incremental` (`--since-green` wins, with a warning).
- Like `--changed`, it sees only git-tracked first-party source. Changes the
  fingerprint can't see (a same-version reinstall, or an interpreter with no
  discoverable site-packages) are not detected: delete
  `.rstest_cache/last_green.json` (or do one full run) after such a change.

### `--incremental`

Dispatch-level incremental testing; git is not required. rstest collects the
whole suite, then **skips** every test that passed last run and whose covered
source and imported modules are byte-identical now (content-addressed through
the coverage index). Skipped tests count as passed and carry `"cached": true`
in [`--report-json`](report-json.md).

- Needs a warm coverage index (a prior `--cov --cov-context=test` run). Pass
  `--cov=.` on incremental runs too so the index keeps advancing; rstest warns
  when `--cov` is missing or narrower than the whole tree (edits outside a
  narrowed scope can't bust the skip).
- Needs the parallel pool with full collection and `--dist load`. Under
  `-n 0/1`, another `--dist`, `--collect lazy`, `--shard` or `--shuffle`,
  rstest warns and runs everything.
- A test also reruns when any first-party module its test file imports,
  directly or transitively (package `__init__.py` included), changed: this
  catches import-time code (a module-level constant) that coverage records
  under no test.
- A config-file change (conftest, pytest config) or a change to any
  **git-tracked non-Python file** (a JSON fixture, a template, a golden file)
  disables skipping for that run. Untracked and ignored files are not watched,
  and outside a git checkout no non-Python file is: after editing a data file
  there, do one run without `--incremental`.
- Other gaps: a dynamically imported module, or a script a test runs in a
  subprocess, is invisible to both coverage and the import scan.
- Root and subdirectory runs share the rootdir's records, and a partial run
  keeps what the rest of the suite recorded. `--cov=.` from a subdirectory
  measures only that subdirectory, so prefer a package name (`--cov=app`) when
  you run `--incremental` from more than one place.
- Mutually exclusive with `--changed` (which owns selection) and
  `--since-green`.

## Reruns and flaky tests

### `--reruns <N>`

Rerun failed tests up to N times. A test that then passes is reported
**flaky**: the run stays green, and the test is counted in the summary
(`N flaky`), listed in its own section, and flagged in `--report-json`. Only
the final attempt's outcome and output are recorded. Per-test budgets:
`@pytest.mark.flaky(reruns=N)`, with or without the global flag, at any worker
count (see [Markers](markers.md#pytestmarkflaky)). Config `[tool.rstest] reruns`.

**Works at any worker count.** Retries are orchestrator-side, so `-n 0`/`-n 1`
with `--reruns` runs as a one-worker pool, outside
[single-worker mode](../concepts/compatibility.md#single-worker-mode): retries for
rate-limited suites that must run few workers. An
installed pytest-rerunfailures is neutralized inside workers, so nothing
double-reruns. Reruns stay **inert** under a passthrough-IO flag (`--pdb`,
`-s`, `--co`, …), which can't be pooled; rstest warns.

Crash-aware: **while `--reruns` (or `@pytest.mark.flaky`) budget remains**, a
test that killed its worker is retried on the replacement worker, bounded by
both the rerun and restart budgets (the segfault-loop guard). Once the budget
is spent, or with no reruns configured, the crashed test is reported FAILED
(see [crash handling](../concepts/crash-handling.md)).

Reruns rescue a flake within one run; the flake history and
[`--quarantine`](#-quarantine-file) manage it across runs: see
[Flaky tests](../guides/flaky-tests.md).

### `--only-rerun <REGEX>`

With reruns active, retry only failures whose error text matches the
pattern (repeatable; any match retries). Same semantics as
pytest-rerunfailures' flag: useful for retrying known-transient errors
(`ConnectionError`, `TimeoutError`) while letting real failures fail fast.

### `--reruns-only-known-flaky`

Spend the rerun budget only on tests with a **prior flaky history**: a
`flaky > 0` record in `.rstest_cache/flakes.json` (passed after a rerun on some
earlier run). Any other failure, including one with a hard-failure-only
history (`failed > 0`, `flaky == 0`), is reported failed without a retry. This
keeps a deterministic mass-failure (a missing migration or broken import
failing many tests identically) from spending reruns that can't recover.

- **`@pytest.mark.flaky` always bypasses it**: the marker is an explicit
  declaration, and a marker budget is unaffected by the gate.
- **Composes with [`--only-rerun`](#-only-rerun-regex)**: both gates must pass.
- **No-op unless `--reruns`** (or `[tool.rstest] reruns`) is active.
- **It consumes history but can't build it**: it suppresses the very rerun
  that would record a new flake. Seed `flakes.json` from an unflagged
  `--reruns` run (nightly or pre-merge) or a `flaky` marker, and cache
  `.rstest_cache` across CI runs. A brand-new flake fails on the flagged path
  until a learning run records it.

Config `[tool.rstest] reruns-only-known-flaky = true`.

### `--quarantine <FILE>`

Ring-fence known-flaky tests without hiding them. `FILE` lists nodeids or `*`
glob patterns (one per line, `#` comments):

```text
# tracked in JIRA-1234, remove when fixed
tests/test_api.py::test_poll_eventually
tests/test_ws.py::*
```

A failure matching the list is demoted to a **quarantined** outcome: counted
separately in the summary (`N quarantined`), printed with its traceback in its
own section, flagged as a `quarantined` testcase property in junit (no
`<failure>` element, so junit-gating CI stays green) and in `--report-json`,
and never fatal: a run whose only failures are quarantined exits 0.
**Failures outside the list still fail the run**, and a listed test that
passes is a plain pass. A quarantined failure never counts toward `-x` /
`--maxfail`.

Candidates come from the flake history every run records to
`.rstest_cache/flakes.json` (per-test flaky-pass and hard-failure counts, last
seen); the flaky and quarantined sections annotate each test with it
(`flaked 3x before, failed 1x`). Reruns paper over a flake within one run;
quarantine is cross-run policy for tests a team has decided to tolerate while
fixing. Workflow and CI surfaces: [Flaky tests](../guides/flaky-tests.md).

## Timeouts and crashes

### `--timeout <SECS>`

Per-test deadline: fail any test whose **call phase** runs longer than SECS
(fractional allowed, `--timeout 0.5`). The test is interrupted **in-process**
(a signal in the worker), so the traceback points at the line it was stuck
on: pytest-timeout's behavior, built in, no plugin, working under the parallel
pool. `@pytest.mark.timeout(N)` overrides the value per test:

```python
@pytest.mark.timeout(5)
def test_slow_path(): ...
```

A test blocked inside a C extension never returns to the interpreter, so the
signal can't fire; a per-test hang watchdog (3 × the test's timeout + 10 s)
kills its worker instead. See
[Hung tests](../concepts/crash-handling.md#hung-tests-worker-timeout).

!!! warning "Windows: no in-process interrupt"
    The interrupt is a Unix signal (SIGALRM), which Windows lacks. There a
    slow test is **not** failed at its timeout: only the hang watchdog
    applies, and it kills the worker without a traceback at the stuck line.
    For a tighter cap, set `--worker-timeout`.

### `--worker-timeout <SECS>`

Hang backstop with one fixed limit for every test: a worker stuck on **one
test** longer than SECS, in any phase (setup, call or teardown), is killed. The
test is reported failed with a timeout message, the worker's other tests
redistribute, and a replacement worker joins (the crash-recovery budgets
apply). It catches what an in-process [`--timeout`](#-timeout-secs) can't
interrupt: tests blocked inside C extensions or deadlocked threads. Under
`--reruns`, a timed-out test is retried within the budget.

Without it, only tests with a timeout get the per-test
[hang watchdog](../concepts/crash-handling.md#hung-tests-worker-timeout).
It replaces those per-test limits, so set it above your longest
`@pytest.mark.timeout`. Hangs **outside** a test (collection, session config)
are not covered. Config `[tool.rstest] worker-timeout` (whole seconds).

## Durations and regression gates

### `--durations <N>` / `--durations-min <SECS>`

*Forwarded pytest flags.* pytest's slowest-durations report, rendered by the
orchestrator after the run so the block covers all workers. `--durations=0`
shows everything; entries under `--durations-min` (default 0.005s) are hidden
with pytest's note unless `-vv`. Setup, call and teardown each get a line, as
in pytest. In single-worker mode with no `--output` set, the pytest session
prints this report itself, exactly as pytest does.

### `--durations-regress <RATIO>`

Gate CI on per-test duration regressions. After the run, each test's wall time
is compared against the duration cache (`.rstest_cache/durations.json`, the
file scheduling uses; restore it from your CI cache). Any test whose time is
at least `RATIO` × its baseline is listed and the run exits 1:

```text
=========== duration regressions (>= 2x baseline) ===========
     0.10s ->    1.21s  tests/test_api.py::test_poll
```

Jitter-floored: baselines under 50 ms and absolute growth under 0.5 s never
count, and tests absent from the baseline (new or renamed) are skipped. A
missing baseline file skips the comparison (first run, cold cache; see
[`--require-baseline`](#-require-baseline)). The comparison runs before the
cache is refreshed.

A flagged test's time is kept out of `durations.json` (and any `--cache-push`
segment), so the regression keeps failing until the test is back under the
threshold, unless the same run edited the test's file: saving then drops that
file's entries as stale, and the next run has no baseline for the test
([Catching slowdowns](../guides/slowdowns.md#where-the-baseline-comes-from)).
A failed test's duration is never recorded either, so a fail-fast
run can't shrink the baseline. To accept an intended slowdown, run once
without `--durations-regress`.

### `--require-baseline`

With `--durations-regress` active, treat an **absent** duration baseline as a
hard error instead of silently skipping the comparison, so a CI run that never
restored (or pulled) the cache can't pass regressions green. A *failed*
`--cache-pull` is always an error; this adds the case of a successful pull
that returned nothing. Not evaluated where the gate doesn't run: `--co`,
`migrate-check`, and passthrough (`-s`/`--pdb`).

## Caching and remote cache

### `--cache-remote <URL|DIR>` / `--cache-pull` / `--cache-push`

Publish and warm the `.rstest_cache` (durations, flake history, and the
`--changed` coverage index) to and from a **shared remote**, with no
hand-rolled `actions/cache` glue or dedicated refresh job. Env:
`RSTEST_CACHE_REMOTE` (the flag wins). `--cache-remote` accepts:

- a **directory** / `file://` path: local, an NFS/EFS mount, or a dir a CI step
  materializes via `download-artifact` / `aws s3 sync`;
- an **`s3://` / `gs://`** bucket URL, through the `aws` / `gcloud` (else
  `gsutil`) CLI already authenticated on the runner (no SDK, no secrets in the
  URL);
- an **`http(s)://`** endpoint: it must serve `GET <root>/segments/` as a JSON
  array of segment names (the [listing
  contract](../concepts/caching.md#http-listing-contract)) and support `GET` /
  `PUT` / `DELETE`. Bearer auth from `RSTEST_CACHE_REMOTE_TOKEN`, sent on every
  request: use `https://`, since plain `http://` sends the token in cleartext.

Any other `scheme://` is rejected rather than written to a local directory
named after the URL.

- `--cache-pull` merges the remote into the local cache **before** the run,
  warming scheduling and the regression baseline. A failed pull (unreachable
  remote, auth or listing error) **aborts with exit 1 before any test runs**
  (`Error: pulling shared cache from <remote>`); an empty or missing remote is
  not a failure. See
  [Shared cache: reliability](../guides/ci-shared-cache.md#reliability).
- `--cache-push` publishes **this run's** contribution afterward as one
  immutable, uniquely-named **segment**, so concurrent shards and PRs never
  conflict; the next pull unions all segments. A push failure warns but never
  fails an otherwise-green run.

There is no single-writer job: every shard can run `--cache-pull --cache-push`.
In a shard matrix, though, an early push changes what a later shard pulls; see
[Keep one cache snapshot across the matrix](../guides/sharding.md#keep-one-cache-snapshot-across-the-matrix)
and [Shared cache](../concepts/caching.md#shared-cache-backend).

`--cache-remote` alone (without pull, push or compact) does nothing and warns.
Pull and push are **not** supported at a
[monorepo root](../concepts/monorepo.md#flags-at-a-monorepo-root) (each
project has its own cache dir; rstest errors): run rstest per project there.

### `--cache-compact-threshold <N>`

Fold **on push** instead of in a separate job: after a `--cache-push`, if the
remote holds more than N loose segments, rstest compacts inline (honoring
`RSTEST_CACHE_KEEP_LAST` / `RSTEST_CACHE_MAX_AGE`). Env:
`RSTEST_CACHE_COMPACT_THRESHOLD`. **Best-effort**: any failure warns and never fails an
otherwise-green run; concurrent auto-compactions are safe, only redundant. Leave it unset to keep
compaction an explicit `cache-compact` step. On `s3://` / `gs://`, keep N low
(20 to 50): a pull reads each loose segment with its own `aws` / `gcloud`
process, one after another.

## Diagnostics

### `--doctor`

After the run, print a diagnosis: wait-bound tests (wall vs CPU time), the
parallel floor (tests that cap any `-n`), parallel efficiency (`-n ≥ 2` only),
fixture hotspots with scope advice, slowest files, and **resource leaks**
(threads or fds a test left open). Adds a few cheap measurements; outcomes are
unaffected. Reading each section: [Suite diagnostics](../guides/doctor.md).

### `--doctor-json <path>`

Write the doctor analysis as JSON (stable, versioned via the `schema`
field) for CI trending. Implies doctor instrumentation; combine with
`--doctor` for the human report too. Field reference:
[Doctor JSON](report-json.md#doctor-json).

### `--doctor-md <path>`

Write the doctor analysis as GitHub-flavored markdown: the terminal report's
signals as job-summary tables. Implies doctor instrumentation.

On GitHub Actions and Buildkite you rarely need it: any doctor run
(`--doctor`, `--doctor-json`, `--doctor-md` or `--doctor-fail-on`) publishes
this markdown automatically (appended to `$GITHUB_STEP_SUMMARY` on GitHub,
`buildkite-agent annotate` in info style on Buildkite). Use the flag for a
file copy, or on GitLab and TeamCity, which have no markdown job-summary
surface (publish the file as an artifact).

### `--doctor-fail-on <COND>`

Fail the run when a doctor metric breaches a threshold, turning the advisory
doctor signal into a CI gate. Repeatable; the run fails if *any* condition
fires. Implies doctor instrumentation. Walkthrough:
[Gating a PR on doctor metrics](../guides/doctor.md#gating-a-pr-on-doctor-metrics).

```console
$ rstest -n auto --doctor-fail-on 'parallel_efficiency<30' \
                 --doctor-fail-on 'wait_pct>50'
```

Grammar `metric OP value`, with operators `<`, `<=`, `>`, `>=`, `==`, `!=`.
Metrics (from the [Doctor JSON](report-json.md#doctor-json) model):

| metric | meaning |
|---|---|
| `wall_seconds` | total wall-clock time |
| `test_time_seconds` | summed test durations (setup + call + teardown) |
| `cpu_time_seconds` | summed CPU time over the same span, child processes included |
| `tests` | tests with timing data |
| `workers` | worker count (`-n`; 1 for `-n 0`) |
| `wait_pct` | % of test time spent waiting, not computing |
| `wait_seconds` | seconds spent waiting (test time minus CPU time) |
| `parallel_efficiency` / `efficiency_pct` | realized-vs-possible speedup, % |
| `realized_speedup` | test time ÷ wall time |
| `imbalance_pct` | busiest-vs-idlest worker load gap, % |
| `long_pole_seconds` | slowest single test (setup + call + teardown) |

- `wait_pct` and `wait_seconds` gate on the measured values, even when the
  report hides its WAIT-BOUND section (under 20% or 1 s of waiting).
  `long_pole_seconds` is measured at any worker count.
- A pool-only metric (`parallel_efficiency`, `efficiency_pct`,
  `realized_speedup`, `imbalance_pct`) at `-n 0` / `-n 1` is **skipped, not
  failed**, with a `not measured` note. The closing line is then
  `M condition(s) passed, K skipped (not measured for this run)` instead of
  `all N condition(s) passed`.
- An unknown metric, a malformed condition, or a non-finite threshold (`NaN`,
  `inf`) aborts before the run.
- `==` / `!=` are reliable only on the integer metrics (`tests`, `workers`);
  on a floating-point metric rstest warns.
- The failure block prints to stderr, so `--output json`/`tap` stay pure on
  stdout. Under a passthrough-IO flag (`-s`/`--pdb`/`--co`) there is no
  instrumentation, so rstest warns instead of passing green.
- Like any doctor run it publishes the full report to the CI job summary
  (GitHub Actions, Buildkite), even without `--doctor`, so a failed gate shows
  why. Pass `--doctor-md` for a file copy.

### `--fail-on-leak`

Fail the run if any test **leaked a resource**: started a thread or opened a
file descriptor during its setup, call or teardown that is still alive after
its teardown. Turns the leak signal (see [`--doctor`](#-doctor)) into a CI
gate.

```console
$ rstest -n auto --fail-on-leak
```

Needs no `--doctor`. Exits `1` when any leak is found, listing the offenders
on stderr (so `--output json`/`tap` stay pure on stdout); on a clean suite
exits `0` and prints `no thread/fd leaks detected`. Under a passthrough-IO flag
(`-s`/`--pdb`/`--co`), which has no instrumentation, it is ignored with a
warning.

The first test each worker runs is an unchecked **warm-up** (first-touch
imports are not a per-test leak), so under `-n auto` one test per worker is not
gated. What is measured, how leaks are attributed (resources a class, module
or session fixture creates are not charged to the triggering test), and how
to fix one: [Resource leaks](../guides/resource-leaks.md).

Like [`--durations-regress`](#-durations-regress-ratio), the gate sets the
**process exit code** (1 on breach), which is authoritative. The `exitstatus`
in an already-streamed `--output json`/`tap` `sessionfinish` envelope reflects
only the test outcome, so a green session that fails the gate still shows
`"exitstatus": 0` there.

`--fail-on-leak` and `--doctor-fail-on` are command-line only (neither is a
`[tool.rstest]` key): put them in the CI step that invokes rstest.

## Coverage

### `--cov-diff-fail-under <PCT>`

Diff-coverage gate: fail the run when fewer than PCT% of the **lines added or
changed in this diff** are covered by tests, computed from the run's own
coverage data (no `diff-cover`/Codecov round-trip).

```console
$ rstest -n auto --cov=. --cov-diff-fail-under=90 --changed=origin/main
```

Requires `--cov`. The diff is against the [`--changed`](#-changedrev) base when
given, else `HEAD`. Only **executable** added lines count (not blank lines,
comments, or lines coverage.py doesn't treat as statements), and the report
names the uncovered added lines per file:

```text
rstest: diff coverage 83.3% (5/6 added lines covered)
  mymod.py: uncovered added line(s) 7
rstest: --cov-diff-fail-under: diff coverage 83.3% is below 90%
```

Exits `1` below the threshold (the gate line goes to stderr, keeping
`--output json`/`tap` stdout pure). A diff with no added executable lines, or
none under `--cov`, passes. See the
[Coverage guide](../guides/coverage.md#diff-coverage-gate).

### `--cov-diff-json <PATH>`

Write the [diff-coverage](#-cov-diff-fail-under-pct) result as JSON to `PATH`:

```json
{"pct": 83.3, "covered": 5, "uncovered": 1, "files": {"mymod.py": [7]}}
```

`files` maps each file to its uncovered added lines. Same inputs as
`--cov-diff-fail-under` (requires `--cov`; diff against the `--changed` base,
else `HEAD`), but independent of it: `--cov-diff-json` alone writes the
report without gating the exit code.

## Output and reports

<a id="-output-dotsverbosebargithubjson"></a>

### `--output <dots|verbose|bar|github|gitlab|buildkite|teamcity|azure|tap|json>` { #output }

Terminal output style. Config `[tool.rstest] output`. The default is
automatic: `bar` on an interactive terminal (`verbose` with `-v`); `dots` with
no live footer off a TTY (CI, pipes) or under `NO_COLOR`, `--color=no`,
`TERM=dumb` or a `CI` variable, so logs stay byte-stable (see
[environment](environment.md#honored-from-the-environment)); and pytest's own
output in single-worker mode (see the note below).

A value that is not one of the styles above is not rstest's: `--output VALUE`
goes to the pytest session unchanged, so a plugin's own `--output`
(pytest-playwright's artifacts directory, as in
`rstest -n 4 --output test-artifacts`) keeps working. If no plugin defines
`--output`, pytest rejects it (exit 4) and rstest names the styles. An unknown
`output` in [`[tool.rstest]`](#configuration-file) warns and falls back to
`dots`.

!!! note "pytest's own output in single-worker mode"
    In [single-worker mode](../concepts/compatibility.md#single-worker-mode)
    (`-n 0`/`-n 1`, or `-n auto` capped to one worker, without `--reruns`)
    with no `--output` flag or config key, the pytest session prints pytest's
    own terminal output, on a TTY or not: header, FAILURES and ERRORS
    sections, warnings summary, `-r` and `--durations` sections, plugin lines,
    and the final `= N passed in Xs =` line. rstest adds no banner or summary;
    it appends only its extras (quarantined failures, the `--doctor` report,
    the coverage report, gate messages) and still writes `--junitxml`,
    `--report-json`, `--html` and `--stream-json` (see
    [`--junitxml`](#-junitxml-path)). An explicit `--output` style keeps
    rstest's own renderer, under a `single-worker mode` banner.
    At `-n 2` and above nothing changes.

Human styles:

- `dots`: pytest's one char per test (`.`/`F`/`s`/…) with a running
  percentage.
- `verbose`: the `-v` equivalent, one `nodeid OUTCOME` line per test. `-v`
  selects it unless `--output` says otherwise (in single-worker mode with no
  `--output`, `-v` is pytest's own).
- `bar`: pytest-sugar-style: a `✓ nodeid` / `✗ nodeid` line per finished
  test, the traceback inlined under a failing test, a live progress bar in the
  footer, and a closing results bar colored by passed/failed/skipped share
  (below). rstest renders it orchestrator-side, so unlike pytest-sugar (which
  pytest-xdist disables, since workers would fight over one terminal) it works
  under the parallel pool.

```text
Results (4.20s):
  ██████████████████████████████ 29/29
```

Without a live terminal the footer, progress bar and results bar
self-disable; the per-test lines and the `N passed … in Xs` summary remain, so
logs stay greppable. On a terminal, footer lines are cut to its width.

#### Machine-readable styles { #machine-readable-styles }

- `github`, `azure`: the `dots` log plus one inline annotation per failing
  test (a `::error` workflow command, an `##vso[task.logissue]` command);
  flaky passes and quarantined failures are warnings.
- `gitlab`, `buildkite`: the `dots` log with each failure in a collapsible
  section (GitLab) or an expanded group (Buildkite).
- `teamcity`: TeamCity service messages per test.
- `tap`: a pure TAP version 13 stream on stdout.
- `json`: a pure newline-delimited JSON stream on stdout
  ([Streaming JSON](report-json.md#streaming-json)); unlike
  [`--report-json`](#-report-json-path), which writes one end-of-run file.

Exact formats, path and line rules, and how collection errors, flaky tests
and quarantined failures appear in each: [CI output formats](../guides/ci-output.md).

### `--junitxml <path>`

Write merged results as JUnit XML. Intercepted by rstest (with pytest's alias
`--junit-xml`) because per-worker sessions would clobber a shared file.

The document is pytest's own at every worker count: each worker runs pytest's
junitxml plugin and streams every finished `<testcase>` to rstest, which
merges them in collection order (pytest's run order at `-n 0`). Suite name and
attributes, `junit_family`, `junit_logging`, `junit_suite_name`,
`--junit-prefix`, `record_property`, `record_xml_attribute` and
`record_testsuite_property` come out exactly as under pytest; only `time`,
`timestamp` and `hostname` vary between runs. rstest's own signals are
standard `<property>` extensions:

- a test that passed after `--reruns` retries is a passing `<testcase>` with
  `<property name="flaky" value="true" />` (last attempt only);
- a [quarantined](#-quarantine-file) failure loses its `<failure>` /
  `<error>` and carries `<property name="quarantined" value="true" />`, so a
  junit-gated CI agrees with rstest's exit status;
- a test no worker finished (a crash or `--worker-timeout` kill in a pool, an
  `--incremental` cached pass) gets an element synthesized in pytest's shape.

A crash at `-n 0` still ends the run before any report is written.

### `--html <path>`

Write a self-contained HTML report of the merged run: one file with inline CSS
and a small script for sort, filter, search and expand, readable with
JavaScript disabled. Rendered on the orchestrator from merged results, so it
works under the parallel pool, where pytest-html writes nothing at `-n 2` or
more (it only writes from a non-worker node). rstest owns the flag:
pytest-html never receives it, and the layout is rstest's. Not written under a
passthrough-IO flag (`--pdb`, `-s`, `--co`, ...), which has no merged run.

### `--report-json <path>`

Write a per-test outcome snapshot: every test's setup/call/teardown
outcome, duration, source line, xfail flag, and skip reason. Stable schema
intended for tooling; see [Report JSON](report-json.md).

Combined with `--collect-only` (or `--co`) it writes a **discovery**
document instead: nodeids, absolute file paths, source lines, and
markers, without running the suite. See
[Discovery JSON](report-json.md#discovery-json).

### `--stream-json <FILE>`

Write the live [streaming-JSON](report-json.md#streaming-json) event stream
(`testreport` per phase, closed by `sessionfinish`) to `FILE` as a **side
channel**: the `--output json` schema on a separate stream, so an editor can
show normal terminal output **and** drive a Test Explorer from the events.
`FILE` may be a regular file or a named pipe (fifo) the editor opened for
reading first (opening a fifo for write blocks until a reader is present).
Lines are flushed as produced. Works in every pooled and single-worker mode;
under a passthrough-IO flag (`--pdb`, `-s`, `--co`, ...) there is no merged
run, so the `testreport` lines are still written but no closing
`sessionfinish` envelope is.

## Interpreter, watch mode and debugging

### `--python <path-or-version>`

Interpreter for the workers: a path to an interpreter, a virtualenv directory
(`--python .venv` uses its `bin/python`, or `Scripts\python.exe` on Windows),
a command on `PATH` (`python3.12`), or a version request (`3.12`,
`>=3.12,<3.13`, `pypy@3.10`, `3.13t` for free-threaded). Without it, rstest
searches, in order: the active virtualenv (`$VIRTUAL_ENV`), a `.venv` found
walking up from the working directory (stopping at the repository root, the
first directory containing `.git`), versioned `python` / `pythonX.Y` names on
`PATH`, on Windows the python.org installs behind the `py` launcher, and
finally uv-managed interpreters. A `.python-version` file (or a `--python`
version request) filters those candidates rather than picking one. A
`.python-version` pin is soft (a usable `$VIRTUAL_ENV` or project `.venv` wins
over it, with a warning); a `--python` request is not. Without `--python`, a
virtualenv that runs but lacks rstest is an error, not a silent fall-through to
a `PATH` or uv-managed interpreter (see
[Troubleshooting](troubleshooting.md#found-venvbinpython-but-rstest-is-not-installed-in-it)).

### `--watch`

Run once, then watch the directory you started rstest in (recursively) and
rerun on every change to a `.py` or pytest config file. A test-file change
reruns those files, a source change reruns the tests the import graph says
are affected, and a config change reruns the full selection. Type `q` then
Enter (or press `Ctrl+C`) to stop; quitting with `q` exits `0` whatever the
last cycle's outcome, and an rstest-level error during a cycle exits `1`.
Watch reruns default to [`--order fail-fast`](#-order-throughputfail-fast).
Ignored directories, stdin handling, the rerun policy and per-cycle cost:
[Watch mode](../guides/watch-mode.md).

### `--debug[=PORT]`

Run under [debugpy](https://github.com/microsoft/debugpy) for editor
debugging (VS Code and any DAP client). Like `--pdb`, it forces single-worker
mode with inherited stdio, so exactly one Python process hosts the debugger;
rstest starts debugpy in that worker and **blocks until a client attaches**
before collecting, so breakpoints in conftest, collection and tests are all
honored. Bare `--debug` listens on `127.0.0.1:5678`; `--debug=PORT` overrides
the port.

The `--python` interpreter must have `debugpy` installed; without it the run
proceeds without a debugger and prints a hint. Attach with a DAP *attach*
configuration on the same host and port. `--reruns` and pooling are inert, as
under `--pdb`.

Once listening, the worker prints a machine-readable ready line to **stderr**
so an editor can attach without racing the port, then a human-readable
`rstest: debugpy listening on …` line, both before the wait:

```json
{"event": "debugpy", "host": "127.0.0.1", "port": 5678}
```

## Configuration file

rstest-owned defaults can live in `pyproject.toml`:

```toml
[tool.rstest]
numprocesses = 8        # or "auto"
dist = "loadfile"
reruns = 2
reruns-only-known-flaky = true
worker-timeout = 300
collect = "full"        # or "lazy"
order = "throughput"    # or "fail-fast"
output = "bar"          # dots|verbose|bar|github|gitlab|buildkite|teamcity|azure|tap|json (default: bar on a TTY, dots off-TTY; unset at -n 0/1 = pytest's own output)
projects = ["libs/*", "services/api"]   # monorepo subprojects; replaces auto-discovery
```

These are the only keys read (kebab-case); every other rstest flag
(`--timeout`, `--html`, `--junitxml`, `--fail-on-leak`, `--doctor-fail-on`,
...) is command-line only. `worker-timeout` takes whole seconds, like the flag.

Precedence: command line > `[tool.rstest]` > built-in defaults. rstest reads
the **nearest** `pyproject.toml`, walking up from the working directory, and
stops there even if it has no `[tool.rstest]` table. One that is not valid TOML
is skipped with a warning (`rstest: ignoring malformed <path>: ...`) and the
walk continues. rstest-owned flags in pytest's `addopts` or `PYTEST_ADDOPTS`
are **not** read by rstest (see the warning at the top of this page).

**Invalid entries are reported, then ignored.** An unknown key or a
wrong-typed value prints one warning per run, and that setting falls back to
its default:

```text
rstest: /repo/pyproject.toml: ignoring unknown [tool.rstest] key `worker_timeout` (did you mean `worker-timeout`?)
rstest: /repo/pyproject.toml: ignoring [tool.rstest] reruns = "2" (expected a non-negative integer)
```

`did you mean` appears only when the kebab-case spelling is a real key.
Expected types: a non-negative integer (`reruns`, `worker-timeout`), a
non-negative integer or `"auto"` (`numprocesses`), `true` or `false`
(`reruns-only-known-flaky`), a string (`dist`, `collect`, `order`, `output`),
and a list of glob strings (`projects`). `numprocesses` is checked in full: a
string other than `"auto"` or a plain digit string (`"four"`, `"4 "`) gets
the same warning and is ignored, where the same value as `-n` is a parse
error (exit 2). For the other keys only the type is checked here; an unknown
string value (`dist = "bogus"`) is rejected at run start, like the same
value passed as a flag.

**Monorepo roots read only `projects`.** At a
[monorepo](../concepts/monorepo.md) root, `[tool.rstest]` supplies only the
project list: the worker budget comes from the command-line `-n` (else
`auto`), and `dist`, `order`, `output`, `reruns` and `worker-timeout` reach
the projects only from the root command line. Each project reads its own
nearest `pyproject.toml`, so a project without one (configured by
`pytest.toml`, `pytest.ini`, `tox.ini` or `setup.cfg`) picks up the root's
`[tool.rstest]`. See [Monorepo mode](../concepts/monorepo.md#session-isolation).

## Forwarded pytest flags

Everything not listed above is passed to the vendored pytest core unchanged:
`-k`, `-m`, `-x`, `--maxfail`, `-q`, `-v`/`-vv`, `--lf`, `--ff`, `-W`, `-p`,
`--tb`, `--color`, `--basetemp`, plugin flags, ...

Two more are rstest's own and never forwarded: `-h` / `--help` (rstest's flag
and subcommand list) and `-V` / `--version` (prints `rstest <version>`). In
single-worker mode with no `--output` set, `rstest -- --help` prints the
vendored pytest's help; at `-n 2` and above or with an explicit `--output` it
does not. To list pytest's and your plugins' flags, run
`python -m pytest --help` in the test environment (it needs pytest installed
there and shows that version's flags).

Short flags combine the way pytest reads them: `-sv` is `-s -v`, `-xv` is
`-x -v`, and `--capture no` is `--capture=no`.

When rstest replaces your positional paths with its own selection
(`--changed`, `--watch` reruns), it keeps every option and its value, even one
that names a path on disk (`-k api`, `--ignore tests/slow`, `--cov src`). It
knows pytest's own options and the common plugins'; for another plugin's
option whose value is a separate token, write `--option=value` so the value
isn't mistaken for a test path.

Besides [`--doctest-modules`](#-doctest-modules) and
[`--durations`](#-durations-n-durations-min-secs) (sections above), three more
get extra orchestration on top of their per-session meaning:

- **`-x` / `--maxfail=N`**: coordinated globally, whether given on the command
  line, in ini `addopts` or in `PYTEST_ADDOPTS`. At the threshold, dispatch
  halts: running tests finish and no other test starts, including queued ones.
  The summary shows pytest's `!!!!!!!!!! stopping after 1 failures !!!!!!!!!!`
  banner, and the run always exits 1. Only failures that fail the run count:
  an attempt a rerun may still rescue ([`--reruns`](#-reruns-n),
  `@pytest.mark.flaky`) counts once its reruns are used up, and a
  [`--quarantine`](#-quarantine-file) match never counts, at any worker count.
  A rerun still queued at the stop is not run.
- **`--lf` / `--ff`**: rstest writes the last-failed cache from merged results
  (each worker sees only its own failures), so a follow-up `--lf` behaves as
  after a serial run.
- **`-v`**: in the parallel pool, rstest renders one line per test in
  completion order, prefixed with the worker that ran it (`[gw2] ...`, xdist's
  convention). Failure headers carry the same attribution, and `--report-json`
  records the worker per test. In single-worker mode with no `--output` set,
  the pytest session prints its own `-v` lines, with no worker prefix.

## Passthrough-IO flags

Flags that need pytest's own terminal (or stdin) force single-worker mode with
inherited stdio, and pytest renders its own output:

```text
--collect-only / --co     -s / --capture=...     --pdb     --trace     --debug
```

This **overrides any `-n` value or `[tool.rstest]` worker count**: `rstest -n 8
--pdb` runs one session, with no worker banner. When the worker count was set
explicitly above 1, rstest warns on stderr, naming the flag:

```text
rstest: -s runs the session in a single process with pytest's own output, so -n 8 is ignored (no parallel workers); drop -s to run in parallel
```

A `breakpoint()` (or `pdb.set_trace()`) left in a test needs this mode too:
pool workers have no terminal, so in a parallel run the call fails that test
with `breakpoint() / pdb.set_trace() needs a terminal, and parallel workers
have none: rerun with -n 0 (or -s) to get the (Pdb) prompt`, and the rest of
the run carries on.

The default `-n auto` stays quiet (plain `rstest -s` is an ordinary request
for pytest's `-s`), and so does `--co`, which runs no tests. `--reruns` is
inert on this path (unlike plain `-n 0/1`, where it runs a one-worker pool).
Drop the passthrough flag to get the pool back.

The stepwise flags also force single-worker mode, for sequencing rather than
IO: stepwise resumes from one nodeid cursor into one global collection order,
which parallel, duration-ordered dispatch can't reproduce (xdist has the same
constraint):

```text
--sw / --stepwise     --sw-skip / --stepwise-skip     --sw-reset / --stepwise-reset
```

## Argument splitting

If a forwarded value collides with an rstest flag name, separate with
`--`:

```console
$ rstest tests -- -m "not slow" -k pattern
```

Everything after `--` goes to the pytest session untouched, including names
rstest would otherwise claim. At `-n 0` that is how you reach a plugin whose
flag rstest owns, e.g. pytest-html's own layout with
`rstest -n 0 -- --html=report.html` (in parallel the plugin writes nothing;
use rstest's own [`--html`](#-html-path) there).

(Usually unnecessary: unknown flags forward automatically.)

**Optional-value flags need `=`.** `--changed[=REV]`, `--shuffle[=SEED]` and
`--debug[=PORT]` take a value only when it is attached with `=`. The bare
flag never consumes the next argument, so `--changed origin/main` means
"`--changed` against `HEAD`, plus the test path `origin/main`". Write
`--changed=origin/main`, `--shuffle=42`, `--debug=5679`.
