# CLI flags

rstest owns the flags listed on this page, grouped by topic below;
**everything else forwards to the test session verbatim**, so the rest of the
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

Subcommands (`rstest try`, `rstest migrate-check`, `rstest audit`, `rstest
bisect`, `rstest shard-verify`, `rstest explain`, `rstest cache-compact`,
`rstest verify-vendor`) and their own flags are on [CLI
subcommands](cli-commands.md).

At a [monorepo](../concepts/monorepo.md) root, rstest forwards most of these
flags to every project, gives output files a per-project name, and refuses the
few that need one project's session. The per-flag table is in
[Flags at a monorepo root](../concepts/monorepo.md#flags-at-a-monorepo-root).

## Parallelism and scheduling

### `-n, --numprocesses <N|auto>`

Worker count. Default `auto` (logical cores, capped as described below).

- `-n auto`: one worker per available logical core, then capped by what the
  suite can use: never more workers than test files (counted over the whole
  project, not just the paths you pass), and, once the duration cache is
  warm, at most one worker per ~2 s of cached suite time. On Linux the core
  count honors the process CPU affinity mask and cgroup CPU quota, so a
  CPU-limited container (`docker run --cpus=2`, a constrained CI runner) sees
  its allocation, not the host's core count (no oversubscription). Pin `-n
  <k>` if you want a fixed count regardless.
- `-n 4`: four workers
- `-n 0` or `-n 1`: **byte-exact mode**, one pytest session with byte-exact
  pytest semantics; identical to each other, with no worker identity below
  `-n 2`. Exception: with `--reruns`, `-n 0/1` runs a one-worker pool instead
  (worker `gw0`, not byte-exact); see [`--reruns`](#-reruns-n). See [Byte-exact mode](../concepts/glossary.md#byte-exact-mode)
  (and, for migrators, how it differs from pytest-xdist's `-n 1`).

**An explicit `-n <k>` is not capped by core count.** Only `auto` caps down.
A literal `-n 16` on an 8-core box runs 16 workers. This is the knob for
**wait-bound suites** (IO, sleeps, network, timeouts): a worker holds no core
while it waits, so running more workers than cores overlaps more waits and
cuts wall time, something `auto` will never do on its own. `auto` can also
settle *below* the core count on a few-file suite, so pin `-n` when you want to
overlap more than the file count allows. The same caps can resolve `auto` to a
single worker (one test file, or a warm cache under ~2 s), and
[`--shard`](#-shard-kn) refuses to run on one worker, so pass an explicit
`-n 2` or higher alongside `--shard`. Full tuning method:
[wait-bound playbook](../guides/wait-bound.md#2-tune-the-worker-count).
Conversely, load-sensitive suites (tight timing assertions) may need `-n`
*capped* below cores; see [Parallel safety](../guides/parallel-safety.md#choosing-the-worker-count).

`-s`, `--capture=…`, `--pdb`, `--trace`, `--debug` and the stepwise flags
override `-n` and run one process (rstest warns when you set `-n` above 1);
see [Passthrough-IO flags](#passthrough-io-flags).

### `--dist <load|loadfile|loadscope|loadgroup|each>`

Distribution mode. Default `load`.

- `load`: test-granular, dynamic and duration-aware. Cached slow tests
  dispatch first and individually; the rest flows in contiguous chunks
  that preserve module-fixture locality.
- `loadfile`: whole files stay on one worker, in file order. For
  order-dependent suites.
- `loadscope`: fixture-scope affinity. A class's tests stay together,
  module-level functions stay with their module. For expensive
  class/module fixtures that must not duplicate.
- `loadgroup`: `@pytest.mark.xdist_group("name")` affinity, across
  files; unmarked tests distribute individually.
- `each`: every worker runs the **full** suite. Counts are per-worker
  totals and outcomes are keyed `nodeid [gwN]`; `--reruns` is
  rejected (the mode exists to *expose* per-worker outcome
  differences, and rerunning failures would mask exactly the
  flakiness `each` is there to surface); the duration cache is not
  updated. Honest scope note:
  every worker uses the same interpreter, so this validates isolation
  and shakes out flakiness; xdist's heterogeneous-environment use
  (`--tx` gateways) has no rstest equivalent.

All five are pytest-xdist-compatible mode names.

### `--order <throughput|fail-fast>`

!!! note "Unreleased"
    Not in rstest 0.7.0 (the latest release); available when installing from
    source, and in the next release. The same applies to the
    `[tool.rstest] order` key.

Dispatch **ordering** within `--dist load` (the other dist modes carry an
affinity order that is the point, so they ignore this).

- `throughput` (default): slowest cached tests first, individually, to
  pack workers for the best wall-clock time. This is the historical
  behavior.
- `fail-fast`: order for the earliest **red** signal. Tests that
  hard-failed go first (most recent failure first), then flaky tests (most
  recent flake first), both read from `.rstest_cache/flakes.json`, each
  dispatched on its own so they run in parallel. At most 128 lead, and
  tests matched by [`--quarantine`](#-quarantine-file) are never pulled
  forward. The remaining clean tests
  follow in `throughput` order, so workers still pack and modules stay
  together. Pair with [`--maxfail`/`-x`](#forwarded-pytest-flags) for true early exit. A
  broken run then dies in seconds instead of minutes.

**Auto:** with neither the flag nor `[tool.rstest] order` set, rstest
picks `fail-fast` under [`--watch`](#-watch) (you want the failure now, on
each save) and `throughput` otherwise. An explicit `--order fail-fast` (flag
or config) warns where it has no effect: an affinity dist
(`loadfile`/`loadscope`/`loadgroup`), `--dist each` (no dispatch queue), and
`--collect lazy`. On a single-worker run or a passthrough run (`-s`, `--pdb`,
`--co`, `--debug`, ...) it warns only when passed on the command line.
Combining it with `--shuffle` is an error. A cold `flakes.json`
just means no test has a failure/flake signal yet, so fail-fast matches
`throughput`. Monorepo runs forward `--order` to every project. Config `[tool.rstest] order`.

### `--shuffle[=SEED]`

Run tests in a seeded random order (the pytest-randomly idea, applied to
the orchestrator's dispatch queue). Order dependence is the central
parallel-readiness hazard; a shuffled run flushes it out on demand
(in CI or before enabling more workers) instead of waiting for a
scheduling change to bite. Without a value the seed is chosen per run
and printed; reproduce a failing order with `--shuffle=SEED` (add
`-n 2 --dist loadfile` to keep the repro stable).

Affinity modes (`loadfile`/`loadscope`/`loadgroup`) shuffle the group
order and keep in-group order intact: in-group order is the affinity
contract. In `load` mode the shuffle replaces duration-aware
sequencing for that run. Requires the parallel pool with full
collection: byte-exact mode, `--collect lazy`, and `--dist each`
are refused (not silently ignored; a run probing for order
dependence must not quietly run ordered).

At a monorepo root the seed is chosen once and shared by every project, so one
`--shuffle=SEED` reproduces the whole run (**Unreleased**: rstest 0.7.0 did not
forward `--shuffle` to projects).

### `--shard <K/N>`

Split the suite across `N` independent CI jobs and run only shard `K`
(1-based: `1/4` … `4/4`). Each job partitions the collected tests into
`N` buckets balanced by the duration cache
(`.rstest_cache/durations.json`, longest-processing-time-first) and runs
its bucket; a cold cache falls back to an even count split. When every job
sees the same collection and the same duration cache, buckets are disjoint
and cover the whole suite, so merging the per-job JUnit reconstructs the full
run. If jobs see different caches (for example a shared cache that changes
mid-matrix), their partitions can disagree; verify with
[`shard-verify`](cli-commands.md#shard-verify).

Orthogonal to `-n`: each shard still runs its slice across local
workers. Requires the parallel pool (`-n ≥ 2`); rejected with
`--shuffle` (a per-run shuffle breaks the identical-partition guarantee
that lets jobs agree without coordinating) and `--dist each`. Under
`--collect lazy` it shards at file granularity. Composes with
`--changed` (selection first, then partition). Restore the **same**
duration cache on every job so their partitions match: see the
[Sharding guide](../guides/sharding.md).

## Test selection and collection

### `--collect <full|lazy>`

Collection strategy. Default `full`: every worker collects the whole
suite (identical sessions, hash-verified). `lazy`: each test file is
collected exactly once, on one worker, on demand, a distributed single
collection pass. Big win for narrow `-k`/`-m` selections on large
suites; full runs of suites with a few giant files prefer `full` (or
`lazy` with an explicitly chosen `load`, from `--dist load` or
`[tool.rstest] dist = "load"`, which enables work-stealing; the implicit
default keeps strict file affinity).
See [Lazy collection](../concepts/lazy-collection.md) for semantics and
the compatibility trade. Configurable via `[tool.rstest] collect`.

`--collect lazy` accepts only `--dist load` and `loadfile`; `loadscope`,
`loadgroup` and `each` are rejected (exit 1):

```text
--collect lazy is file-affine and cannot honor --dist loadscope (only load/loadfile: loadscope/loadgroup need a global id list and each runs the full suite on every worker; use --collect full)
```

Nodeid/`--pyargs` arguments fall back to full collection.

### `--doctest-modules`

Works as in pytest: forwarded to the vendored core, which collects
doctest items from all modules; they dispatch across workers like any
other test. `--doctest-glob` and friends forward the same way.

### `--changed[=REV]`

Run only tests affected by changed files. Changes come from git (working
tree + untracked vs `HEAD`, or vs `REV`, e.g. `--changed=origin/main` in
CI) and map to the affected tests; only those run.

**Coverage-aware when a line→test index is warm.** If
`.rstest_cache/coverage_index.json` exists (written by any
[`--cov-context=test`](../guides/coverage.md#per-test-contexts-cov-contexttest) run),
`--changed` maps the *changed lines* to only the tests whose recorded coverage
executed them: far tighter than the import graph, which reselects every test
importing a changed module. This is automatic and needs no flag; selection only
ever gets tighter once the index exists. The index is trusted for lines it
recorded, so keep it warm (rebuild on your coverage runs; persist
`.rstest_cache` across CI runs).

Falls back **per file** to the import graph (and is byte-identical to it with no index, i.e. a cold cache) for anything coverage can't vouch for: brand-new code
(inserted lines have no prior coverage), files the index never measured, and
untracked files. A changed test file always runs its own tests, and any config
or non-Python change is still a full run. Over-selection is safe; the fallbacks
never under-select against unknown code.

**Map-health reporting.** With a warm map the selection banner reports the
savings ratio (`N changed file(s) -> M of K mapped test(s) affected`)
so you can see how much was skipped. Whole-file targets from the import-graph
fallback or changed test files are counted separately
(`... affected + F whole-file target(s)`). If the map is **cold** and a non-test
`.py` file changed (exactly where coverage precision would have helped),
`--changed` prints a one-line hint that it fell back to the import graph and
that a prior `--cov --cov-context=test` run enables coverage-precise selection
(not when the same diff forces a full run anyway).
The hint stays silent for test-only, `conftest.py`, config, or non-Python
changes, so it never nags a suite that doesn't use coverage.

Conservative by construction: ambiguous module names select every match,
function-local imports count, a changed `conftest.py` selects its whole
subtree, and any config or non-Python change falls back to a full run.
Known gap: dynamic imports (`importlib.import_module`) produce no graph
edges; for correctness-critical runs, use `--changed-strict` below.
With nothing affected, the run prints
`no tests affected by N changed file(s)` and exits 0 without running.

PR-aware in CI: on a pull-request / merge-request job, bare `--changed`
diffs against the merge-base with the PR base branch instead of `HEAD`,
so a clean checkout of the PR commit still selects exactly the PR's files.
The base is auto-detected from the CI environment:

| CI | Variable | Base |
| --- | --- | --- |
| GitHub Actions | `GITHUB_BASE_REF` | base branch → `git merge-base origin/<branch> HEAD` |
| GitLab CI | `CI_MERGE_REQUEST_DIFF_BASE_SHA` | exact MR diff-base SHA (used directly) |
| GitLab CI | `CI_MERGE_REQUEST_TARGET_BRANCH_NAME` | target branch (fallback when the SHA is unset) |
| Buildkite | `BUILDKITE_PULL_REQUEST_BASE_BRANCH` | base branch → merge-base |

Variables are probed in that order; the first set wins. Requires the
base to be present in the clone (`actions/checkout` with `fetch-depth: 0`,
GitLab's default MR fetch, or `git fetch origin <branch>`); an
unresolvable base is an error, never a silent full skip. An explicit
`REV` disables the auto-targeting. TeamCity has no standard base-branch
variable: pass one explicitly or expose a build parameter as env.

### `--changed-strict`

`--changed` hardened for gating CI (merge queues). Implies `--changed`
(vs `HEAD`) when `--changed` isn't given. Three behavior changes:

- **A changed source file the import graph cannot connect to any test
  forces a full run** (naming the file) instead of silently selecting
  nothing for it: the dynamic-import / unused-module / deleted-file
  cases stop being false skips.
- **Monorepos: undeclared cross-project imports count as dependency
  edges.** Each project's Python files are scanned; an import resolving
  to a sibling's top-level modules adds the edge (with a warning naming
  both projects) even when the pyproject never declared it, the
  shared-workspace-venv trap. Namespace packages shared by several
  siblings over-connect, which errs toward running more, never less.
- **"Nothing affected" exits 5** (pytest's nothing-collected code)
  instead of 0, so a pipeline must consciously allow it rather than
  mistaking it for a green run.

Residual risk it cannot remove: imports constructed at runtime from
strings the scanner can't see (`importlib.import_module(f"plugins.{name}")`)
still produce no edges; name such modules in a test file import, or
keep full runs on the gating path.

### `--since-green`

Incremental testing against the last green run. rstest records the git commit
of the last fully-passing run in the cache (`last_green.json`) and uses it as
the [`--changed`](#-changedrev) base, so only tests affected by changes since
then run. The baseline advances **only when a run is fully green**, so a
failing test keeps being selected until it passes.

- First run (no baseline yet) runs everything.
- The baseline is keyed to an environment fingerprint (interpreter plus the
  content of `uv.lock`, `poetry.lock`, `pdm.lock`, `requirements.txt`); a
  change to any of them runs everything once.
- Ignored when `--changed` is given explicitly. Mutually exclusive with
  `--incremental` (`--since-green` wins, with a warning).
- Like `--changed`, it reasons over git-tracked first-party source: a
  dependency upgraded in place without touching a lockfile is not detected.
  Delete `.rstest_cache/last_green.json` (or do one full run) after such a
  change.

### `--incremental`

Dispatch-level incremental testing, without git. rstest collects the whole
suite, then **skips** every test that passed last run and whose covered source
is byte-identical now (content-addressed through the coverage index). Skipped
tests are carried forward as cached passes: they count as passed and carry
`"cached": true` in [`--report-json`](report-json.md).

- Needs a warm coverage index: a prior `--cov --cov-context=test` run. Pass
  `--cov=.` on incremental runs too, so the index keeps advancing; rstest
  warns when `--cov` is missing or scoped narrower than the whole tree (edits
  outside a narrowed scope can't bust the skip).
- Needs the parallel pool with full collection and `--dist load`. Under
  `-n 0/1`, another `--dist`, `--collect lazy`, `--shard` or `--shuffle`,
  rstest warns and runs everything.
- A config-file change (conftest, pytest config) disables skipping for that
  run.
- Mutually exclusive with `--changed` (which owns selection) and
  `--since-green`.

## Reruns and flaky tests

### `--reruns <N>`

Rerun failed tests up to N times. A test that then
passes is reported **flaky**: the run stays green, the test is counted in
the summary (`N flaky`), listed in its own section, and flagged in
`--report-json`. Only the final attempt's outcome and output are recorded.

Per-test budgets are available via `@pytest.mark.flaky(reruns=N)`, which
works with or without the global flag (see
[Markers](markers.md#pytestmarkflaky)).

**Works at any worker count, including `-n 0`/`-n 1`.** The retry machinery
is orchestrator-side, so a single-worker run with `--reruns` set is executed
as a **degenerate one-worker pool** to drive it: rstest neutralizes an
installed pytest-rerunfailures inside that worker, so nothing double-reruns.
This is the escape hatch for rate-limited suites that must run few workers
(real-LLM tests capped on outbound calls) yet still need retries: you no
longer have to pin `-n 2` just to get reruns. Passing `--reruns` opts that
run out of [byte-exact mode](../concepts/glossary.md#byte-exact-mode): a
plain `-n 0`/`-n 1` run with no reruns stays the byte-exact single session.

One exception: reruns stay **inert** under a passthrough-IO flag (`--pdb`,
`-s`, `--co`, …), which needs pytest's own terminal and can't be pooled;
rstest warns when you combine them.

Crash-aware: **while `--reruns` (or `@pytest.mark.flaky`) budget remains**,
a test that killed its worker is retried on the replacement worker, bounded
by both the rerun and restart budgets (the segfault-loop guard), something
in-process rerun plugins cannot do. Once that budget is exhausted, or with
no reruns configured at all, the crashed test is reported FAILED and not
retried (see [crash handling](../concepts/crash-handling.md)). The flag is intercepted by rstest and an installed
pytest-rerunfailures is neutralized inside workers, so nothing
double-reruns.

Reruns rescue a flake within one run; the flake history and
[`--quarantine`](#-quarantine-file) manage it across runs: see
[Flaky tests](../guides/flaky-tests.md).

### `--only-rerun <REGEX>`

With reruns active, retry only failures whose error text matches the
pattern (repeatable; any match retries). Same semantics as
pytest-rerunfailures' flag: useful for retrying known-transient errors
(`ConnectionError`, `TimeoutError`) while letting real failures fail fast.

### `--reruns-only-known-flaky`

Spend the rerun budget only on tests that have a **prior flaky history**:
tests recorded as passed-after-rerun in `.rstest_cache/flakes.json` on some
earlier run. A first-time failure with no flaky history is reported failed
immediately, without retrying.

The motivation is deterministic mass-failures: one root cause (a missing
migration, a broken import) fails many tests *identically*, and a plain
`--reruns 1` reruns every one of them for zero recovery: pure wall-time
waste. Those tests were never flaky, so they carry no flaky history and this
flag skips their reruns, while genuine known-flakes are still rescued.

Details:

- **Keys on flaky history, not failures.** Only a `flaky > 0` record counts.
  A test with a hard-failure-only history (`failed > 0`, `flaky == 0`), the
  signature of a deterministic failure, is *not* treated as known-flaky.
- **`@pytest.mark.flaky` always bypasses it.** An explicit per-test marker is
  an author declaration of flakiness, so a marked test is retried regardless
  of history.
- **Composes with [`--only-rerun`](#-only-rerun-regex).** Both gates must pass
  for a retry to fire.
- **Seeding the history is a separate step.** The flag *consumes* flaky
  history; it does not build it. A `flaky > 0` record is only written when a
  rerun actually fires and the test recovers, but this flag suppresses that
  rerun for any not-yet-known test, so a run with the flag on can never
  promote a brand-new flake into the known set. The history must come from
  elsewhere:
  - a run of `--reruns` *without* this flag (e.g. a nightly or pre-merge job)
    that lets unknown failures rerun and records the ones that recover, or
  - an explicit `@pytest.mark.flaky` (always rerun, see above).

  So the intended setup is two-mode: an unflagged learning run builds
  `.rstest_cache/flakes.json`, and the hot path runs with the flag to spend
  budget only on what that history already knows. A brand-new flake fails the
  first time it appears on the flagged path and is *not* rescued until a
  learning run records it. Cache `.rstest_cache` across CI runs (as you would
  for the duration cache) so the history persists.

Also settable as `[tool.rstest] reruns-only-known-flaky = true`. The gate
only affects tests whose rerun budget comes from `--reruns`, so it is a no-op
unless `--reruns` (or `[tool.rstest] reruns`) is active. A `@pytest.mark.flaky`
budget is unaffected either way: marked tests always bypass the gate.

### `--quarantine <FILE>`

Ring-fence known-flaky tests without hiding them. `FILE` lists nodeids
or `*` glob patterns (one per line, `#` comments):

```text
# tracked in JIRA-1234, remove when fixed
tests/test_api.py::test_poll_eventually
tests/test_ws.py::*
```

A failure matching the list is demoted to a **quarantined** outcome:
counted separately in the summary (`N quarantined`), printed with its
traceback in its own section, flagged as a `quarantined` testcase
property in junit (no `<failure>` element, so junit-gating CI stays green) and in `--report-json` (schema 5), and never fatal: a run whose
only failures are quarantined exits 0. **Failures outside the list
still fail the run**, and a listed test that passes is a plain pass.

Candidates come from the **flake history** every run records to
`.rstest_cache/flakes.json`: per-test counts of flaky passes
(`--reruns` rescues) and hard failures, with a last-seen timestamp.
The flaky and quarantined sections annotate each test with its history
(`flaked 3x before, failed 1x`). Difference from `--reruns`: reruns
paper over a flake within one run; quarantine is cross-run policy for
tests a team has explicitly decided to tolerate while fixing. Workflow,
file format, and CI surfaces:
[Flaky tests](../guides/flaky-tests.md).

## Timeouts and crashes

### `--timeout <SECS>`

Per-test deadline: fail any test whose **call phase** runs longer than SECS.
The test is interrupted **in-process** (a signal in the worker), so the
failure's traceback points at the exact line it was stuck on: the
pytest-timeout behavior, built in, no plugin required and working under the
parallel pool.

```console
$ rstest -n auto --timeout 30
```

Fractional seconds are allowed (`--timeout 0.5`). `@pytest.mark.timeout(N)`
overrides the global value per test:

```python
@pytest.mark.timeout(5)
def test_slow_path(): ...
```

A test hard-blocked inside a C extension never returns to the interpreter, so
the signal can't fire: the [`--worker-timeout`](#-worker-timeout-secs)
watchdog is the backstop for that, and rstest auto-arms it (at a generous
multiple of `--timeout`) whenever you set `--timeout` without an explicit
`--worker-timeout`. The interrupt uses a Unix signal on the test's main
thread; on platforms without it (Windows), the watchdog alone applies.

### `--worker-timeout <SECS>`

Hang backstop, off by default: a worker stuck on **one test** for longer than SECS, in any phase (setup, call, or teardown), is killed. The test is reported failed with a timeout message, the
worker's other tests redistribute, and a replacement worker joins (the
crash-recovery machinery, same budgets). This is the coarse hang backstop;
for ordinary per-test limits use [`--timeout`](#-timeout-secs) (above).
`--worker-timeout` catches what an in-process timeout can't interrupt:
tests hard-blocked inside C extensions or deadlocked threads. Under
`--reruns`, a timed-out test is retried within the budget (deadlocks can be
races). Hangs **outside** a test (during collection or session config) are not
covered by this watchdog.

## Durations and regression gates

### `--durations <N>` / `--durations-min <SECS>`

pytest's slowest-durations report, rendered by the orchestrator after
the run (worker terminals are captured, so the merged block covers all
workers). `--durations=0` shows everything; entries under
`--durations-min` (default 0.005s) are hidden with pytest's note unless
`-vv`. Phase granularity matches pytest: setup, call, and teardown each
get a line.

### `--durations-regress <RATIO>`

Gate CI on per-test duration regressions. After the run, each test's
wall time is compared against the duration cache
(`.rstest_cache/durations.json`: the same file LPT (longest-processing-time-first) scheduling uses;
restore it from your CI cache). Any test that grew past `RATIO` × its
baseline is listed and the run exits 1:

```text
=========== duration regressions (>= 2x baseline) ===========
     0.10s ->    1.21s  tests/test_api.py::test_poll
```

Jitter-floored so CI noise can't flag: baselines under 50ms and
absolute growth under 0.5s never count, and tests absent from the
baseline (new or renamed) are skipped. A missing baseline file skips
the comparison entirely (first run / cold cache). The comparison runs
before the cache is refreshed with this run's times.

### `--require-baseline`

With `--durations-regress` active, treat an **absent** duration baseline as a
hard error instead of the silent "comparison skipped". This closes the
dead-gate failure mode where a CI run that never restored (or pulled) the cache
passes regressions green. A *failed* `--cache-pull` is always an error; this
adds the "*successful* pull returned nothing, but a gate needs it" case. It only
enforces on an actual gated run: collect-only (`--co`), `migrate-check`, and
passthrough (`-s`/`--pdb`) modes don't evaluate the gate, so they don't trip it.

## Caching and remote cache

### `--cache-remote <URL|DIR>` / `--cache-pull` / `--cache-push`

Publish and warm the `.rstest_cache` (durations, flake history, and the
`--changed` coverage index) to/from a **shared remote**, no hand-rolled
`actions/cache` glue, no dedicated refresh job, no cache-key dance. Also
settable as `RSTEST_CACHE_REMOTE`. `--cache-remote` accepts:

- a **directory** / `file://` path: local, an NFS/EFS mount, or a dir a CI step
  materializes via `download-artifact` / `aws s3 sync`;
- an **`s3://` / `gs://`** bucket URL: driven through the `aws` / `gcloud`
  (falling back to `gsutil`) CLI already installed and authenticated on the
  runner; credentials come from the process environment (no SDK, no secrets in
  the URL);
- an **`http(s)://`** endpoint: the endpoint must serve `GET <root>/segments/`
  as a JSON array of segment names (the [listing
  contract](../concepts/caching.md#shared-cache-backend)) and support `GET` /
  `PUT` / `DELETE`. Bearer auth from `RSTEST_CACHE_REMOTE_TOKEN`.

Any other `scheme://` is rejected loudly: rstest never silently writes to a
junk local directory named after the URL.

- `--cache-pull` merges the remote into the local cache **before** the run:
  warming scheduling and the regression baseline.
- `--cache-push` publishes **this run's** contribution afterward as one
  immutable, uniquely-named **segment**. Concurrent shards/PRs each drop their
  own segment and never conflict; readers union all segments on the next pull.
  (A push failure warns but never fails an otherwise-green run.)

Because each run pushes an immutable segment rather than overwriting a shared
blob, there is no single-writer job: every shard just runs
`--cache-pull --cache-push`. See [Shared cache](../concepts/caching.md#shared-cache-backend).

`--cache-remote` on its own (without pull/push/compact) does nothing and warns.
Pull/push are **not** supported at a [monorepo root](../concepts/monorepo.md#flags-at-a-monorepo-root):
each project has its own cache dir, so run rstest per project for shared
caching there (rstest errors rather than silently no-op).

### `--cache-compact-threshold <N>`

Fold **on push** instead of in a separate job: after a `--cache-push`, if the
remote holds more than N loose segments, rstest compacts inline (honoring
`RSTEST_CACHE_KEEP_LAST` / `RSTEST_CACHE_MAX_AGE`). Env:
`RSTEST_CACHE_COMPACT_THRESHOLD`. Strictly **best-effort**: a listing, config,
or compaction failure warns and never fails an otherwise-green run. Concurrent
auto-compactions are safe (the absorbed-id set prevents double-counting), only
redundant. Leave it unset to keep compaction an explicit `cache-compact` step.

## Diagnostics

### `--doctor`

After the run, print a diagnosis: wait-bound tests (wall vs CPU time),
parallel-floor analysis (the tests that cap any `-n`), parallel efficiency
(realized speedup and per-worker load imbalance, `-n ≥ 2` only), fixture
hotspots (with scope advice), slowest files, and **resource leaks** (tests
that ended with more threads / open file descriptors than they started; see
the [Resource leaks](../guides/resource-leaks.md) guide). Adds a few cheap
measurements to the run; outcomes are unaffected.

### `--doctor-json <path>`

Write the doctor analysis as JSON (stable, versioned schema: currently
`3`) for CI trending. Implies doctor instrumentation; combine with
`--doctor` for the human report too. Field reference:
[Doctor JSON](report-json.md#doctor-json).

### `--doctor-md <path>`

Write the doctor analysis as GitHub-flavored markdown: the same signals
as the terminal report, rendered as job-summary tables. Implies doctor
instrumentation.

On GitHub Actions and Buildkite you rarely need the flag: any doctor run
(`--doctor`, `--doctor-json`, or `--doctor-md`) automatically publishes
this markdown to the job summary (appended to `$GITHUB_STEP_SUMMARY` on
GitHub, piped to `buildkite-agent annotate` in info style on Buildkite), so the report shows up on the run page with zero extra steps. GitLab and
TeamCity have no native markdown job-summary surface; use `--doctor-md`
and publish the file as an artifact.

### `--doctor-fail-on <COND>`

Fail the run when a doctor metric breaches a threshold, turning the
otherwise-advisory doctor signal into a CI gate. Repeatable; the run fails
if *any* condition fires. Implies doctor instrumentation.

Grammar is `metric OP value`:

```console
$ rstest -n auto --doctor-fail-on 'parallel_efficiency<30' \
                 --doctor-fail-on 'wait_pct>50'
```

Operators: `<`, `<=`, `>`, `>=`, `==`, `!=`. Metrics (from the
[Doctor JSON](report-json.md#doctor-json) model):

| metric | meaning |
|---|---|
| `wall_seconds` | total wall-clock time |
| `test_time_seconds` | summed test durations |
| `cpu_time_seconds` | summed call-phase CPU time |
| `tests` | tests with timing data |
| `workers` | worker count (`-n`) |
| `wait_pct` | % of test time spent waiting, not computing |
| `wait_seconds` | seconds spent waiting |
| `parallel_efficiency` / `efficiency_pct` | realized-vs-possible speedup, % |
| `realized_speedup` | test time ÷ wall time |
| `imbalance_pct` | busiest-vs-idlest worker load gap, % |
| `long_pole_seconds` | slowest single test |

A metric whose section did not apply to the run is **skipped, not failed**,
e.g. `parallel_efficiency` at `-n 1` (no parallelism to measure) prints a
`not measured` note and never fails the gate. An unknown metric or malformed
condition aborts up front, before the run, so a typo can never become a gate
that silently never fires. `==`/`!=` are reliable only on the integer-valued
metrics (`tests`, `workers`); on a floating-point metric they almost never
match, so rstest warns and you should use a `<`/`>` threshold instead.

The failure block prints to stderr, so `--output json`/`tap` stay pure on
stdout. Under a passthrough-IO flag (`-s`/`--pdb`/`--co`) there is no doctor
instrumentation, so the gate can't run: rstest warns instead of passing green.

Because the gate is a doctor run, it also **publishes the full doctor report**
the way any doctor run does (appended to `$GITHUB_STEP_SUMMARY` on GitHub
Actions, `buildkite-agent annotate` on Buildkite), even when you pass only
`--doctor-fail-on` (no `--doctor`). That is intentional: a failed gate shows
its report on the run page so you can see *why* it failed. Pass `--doctor-md`
for a file copy, or run in a CI with no summary surface if you want gate-only.

### `--fail-on-leak`

Fail the run if any test **leaked a resource**: ended with more live threads
or open file descriptors than it started, its own teardown included. Turns the
leak signal (see [`--doctor`](#-doctor)) into a CI gate.

```console
$ rstest -n auto --fail-on-leak
```

Enables the leak-check instrumentation on its own: you do **not** need
`--doctor`. Exits `1` when any leak is found (listing the offenders on stderr,
so `--output json`/`tap` stay pure on stdout); exits `0` and prints
`no thread/fd leaks detected` on a clean suite. Not evaluated under a
passthrough-IO flag (`-s`/`--pdb`/`--co`), which has no instrumentation: the
flag is ignored there with a warning rather than passing silently.

The first test each worker runs is an unchecked **warm-up** (first-touch
imports are not a per-test leak), so under `-n auto` one test per worker is not
gated; a clean exit does not prove those tests are leak-free.

A leaked thread or fd is shared state that can flake a *later* test; the guide
covers what is measured, the false-positive cases (session-scoped fixtures),
and how to fix a leak: [Resource leaks](../guides/resource-leaks.md).

Exit-code note for machine consumers: the gate affects the **process exit
code** (1 on breach), which is authoritative. It does **not** rewrite the
`exitstatus` inside an already-streamed `--output json`/`tap` `sessionfinish`
envelope: that field is emitted mid-run and reflects the *test* outcome, so a
green session that fails the gate still shows `"exitstatus": 0` there. Key CI
success off the process exit code, not the envelope field (same as
[`--durations-regress`](#-durations-regress-ratio)).

`--fail-on-leak` and `--doctor-fail-on` are command-line only (neither is a
`[tool.rstest]` key): put them in the CI step that invokes rstest.

## Coverage

### `--cov-diff-fail-under <PCT>`

Diff-coverage gate: fail the run when fewer than PCT% of the **lines added or
changed in this diff** are covered by tests. The classic PR gate ("you added
code with no test exercising it") computed from the run's own coverage data,
no `diff-cover`/Codecov round-trip.

```console
$ rstest -n auto --cov=. --cov-diff-fail-under=90 --changed=origin/main
```

Requires `--cov` (there must be coverage data to score). The diff is taken
against the [`--changed`](#-changedrev) base when given, otherwise `HEAD`. Only
**executable** added lines count (blank lines, comments, and lines coverage.py
doesn't treat as statements are ignored), and the report names the uncovered
added lines per file:

```text
rstest: diff coverage 83.3% (5/6 added lines covered)
  mymod.py: uncovered added line(s) 7, 12-14
rstest: --cov-diff-fail-under: diff coverage 83.3% is below 90%
```

Exits `1` when below the threshold (the gate line prints to stderr, keeping
`--output json`/`tap` stdout pure). A diff with no added executable lines (or
whose changed files aren't under `--cov`) passes (nothing to score). See the
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

### `--output <dots|verbose|bar|github|gitlab|buildkite|teamcity|azure|tap|json>` { #-output-dotsverbosebargithubjson }

Terminal output style. The default is **automatic**: on an interactive
terminal it's `bar` (the pretty view); off a TTY (CI, pipes) it falls back
to `dots`, so logs stay byte-stable. Pass `--output` to pin a style.

`dots` is pytest's one-char-per-test (`.`/`F`/`s`/…) with a running
percentage. `verbose` is the `-v` equivalent: one `nodeid OUTCOME` line
per test. `bar` is a pytest-sugar-style view: a per-test result line
(`✓ nodeid` / `✗ nodeid`) as each test finishes, the failure traceback
inlined right under a failing test, a live filled progress bar in the
footer, and a closing results bar, a full-width bar whose segments are
colored green/red/yellow in proportion to passed/failed/skipped, over the
test count:

```text
Results (4.20s):
  ██████████████████████████████ 29/29
```


`bar` works **under the parallel pool**, where terminal-reporter plugins
like pytest-sugar structurally can't (each worker would fight for the one
terminal: the same reason pytest-sugar is disabled under pytest-xdist); rstest
renders it orchestrator-side from the streamed results.

When stdout is not a TTY (CI, pipes) the live footer, progress bar, and
closing results bar self-disable; the per-test lines plus the stable
`N passed … in Xs` summary remain, so logs stay greppable. `-v` selects
`verbose` unless `--output` says otherwise. Pin any style explicitly with
`--output` or `[tool.rstest] output` to override the TTY auto-default.

#### Machine-readable styles { #machine-readable-styles }

`github` renders the normal `dots` log and additionally emits a
[GitHub Actions](https://docs.github.com/actions) `::error` workflow command
for each failing test, so failures show up as inline annotations on the PR
diff:

```text
::error file=<path>,title=<nodeid>,line=<n>::<traceback>
```

`file` comes from the nodeid path. `line` is 1-based (the annotator adds 1 to
pytest's 0-based `report.location`) and is omitted when no location is
available. (The `lineno` field in the JSON reports stays 0-based, so
`line` = `lineno + 1`.) The traceback is escaped per the
workflow-command spec. Use it as your CI `--output`.

Tests that passed only after reruns (`--reruns` /
`@pytest.mark.flaky`) additionally emit a `::warning` annotation
(`flaky: passed only after N reruns`): the run stays green, but the
flake is visible on the PR without opening the log.

`azure` renders the normal `dots` log and additionally emits an [Azure
Pipelines logging
command](https://learn.microsoft.com/azure/devops/pipelines/scripts/logging-commands)
per failing test, surfaced as an inline issue on the file in the PR:

```text
##vso[task.logissue type=error;sourcepath=<path>;linenumber=<n>]<nodeid>: <message>
```

`sourcepath` comes from the nodeid path; `linenumber` (1-based: the 0-based
`report.location` plus 1, as in the GitHub annotator) from pytest's report
location, omitted when none is available. The message is
collapsed to one line (logissue is single-line). Flaky-passed tests
(`--reruns`) additionally emit a `type=warning` logissue: green run,
visible flake.

`gitlab` renders the normal `dots` log; each failure in the end-of-run
failures block is wrapped in a [GitLab CI collapsible
section](https://docs.gitlab.com/ci/jobs/job_logs/#custom-collapsible-sections)
(`section_start`/`section_end`, collapsed by default), so the job log
folds tracebacks per test. GitLab has no per-line warning command, so the
flaky-tests block folds into its own collapsed section under this style.

`buildkite` renders the normal `dots` log; each failure is emitted under
an auto-expanded [`+++` group
header](https://buildkite.com/docs/pipelines/configure/managing-log-output),
so failing tests open as their own groups in the Buildkite log UI. Flaky
tests are published as a `warning`
[annotation](https://buildkite.com/docs/agent/v3/cli-annotate) on the
build page (best-effort via `buildkite-agent`).

`teamcity` emits [TeamCity service
messages](https://www.jetbrains.com/help/teamcity/service-messages.html)
as each test finishes: a `testStarted`/`testFinished` pair per test,
plus `testFailed` (with the escaped traceback as `details`) or
`testIgnored` for skips/xfails. Each test's messages are emitted as one
group, so parallel results never interleave. Flaky tests emit a
`WARNING`-status build message. The banner and summary stay: TeamCity
ignores non-service lines.

`tap` makes stdout a pure [Test Anything Protocol](https://testanything.org)
version 13 stream: one `ok N - nodeid` / `not ok N - nodeid` point per
test as it finishes, failure text as `#` diagnostic lines, skips as
`# SKIP <reason>`, xfail/xpass as `# TODO`, closed by the trailing
`1..N` plan. No banner or human summary. For TAP harnesses (`prove`,
Jenkins TAP plugin, etc.).

`json` makes stdout a pure **newline-delimited JSON** stream: one
`testreport` object per phase as each test finishes, closed by a
`sessionfinish` envelope. No banner, footer, or human summary is printed,
so every line parses on its own. Built for editors and tooling that consume
results live; see [Streaming JSON](report-json.md#streaming-json) for the
event shapes and fields. This differs from
[`--report-json`](#-report-json-path), which writes a single end-of-run
snapshot document to a file.

### `--junitxml <path>`

Write merged results as JUnit XML. Intercepted by rstest (rather than
forwarded) because per-worker sessions would clobber a shared file; the
XML is rendered from merged results with pytest's classname conventions.

Only final outcomes appear: a test that passed after `--reruns` retries is
a passing `<testcase>` carrying a
`<property name="flaky" value="true"/>` (JUnit's standard extension
point), so junit-based dashboards can track flakes without parsing
`--report-json`.

### `--html <path>`

Write a self-contained HTML report of the merged run: one file with inline CSS
and a small script for sort, filter, search and expand, readable with
JavaScript disabled. rstest renders it on the orchestrator from merged
results, so it works under the parallel pool, where pytest-html writes nothing
at `-n 2` or more (pytest-html only writes from a non-worker node, and every
rstest worker is a worker node).

`--html` is rstest-owned: pytest-html, if installed, never receives the flag,
and the report layout is rstest's, not pytest-html's. Not written under a
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
channel**, leaving stdout's human output untouched. This is the same schema
as `--output json`, but on a separate stream, so an editor can show normal
terminal output **and** drive a Test Explorer from the events at the same
time. `FILE` may be a regular file or a named pipe (fifo) the editor opened
for reading first (opening a fifo for write blocks until a reader is
present). Works in every pooled and single-worker run mode; under a
passthrough-IO flag (`--pdb`, `-s`, `--co`, ...) there is no merged run, so
no closing `sessionfinish` envelope is written. Lines are flushed as they are
produced.

## Interpreter, watch mode and debugging

### `--python <path-or-version>`

Interpreter for the workers. Accepts either a path to an interpreter or a
version request: `3.12`, `>=3.12,<3.13`, `pypy@3.10`, `3.13t` (free-threaded).
Without it, rstest searches, in order: the active virtualenv (`$VIRTUAL_ENV`),
a `.venv` found walking up from the working directory, versioned `python` /
`pythonX.Y` names on `PATH`, and finally uv-managed interpreters as a fallback.
A `.python-version` file (or a `--python` version request) does not pick an
interpreter directly: it sets the version that filters those candidates.

### `--watch`

Watch the project and rerun on change. A change set consisting only of
test files reruns exactly those files (with your other flags); a source
(`.py`) change reruns the tests the import graph says are affected
(the `--changed` machinery; unresolvable changes fall back to the full
selection); a pytest-config change reruns the full selection. Ignores
VCS, caches, and virtualenvs. Type `q` then Enter to exit cleanly (`Ctrl+C`
also works; **Unreleased:** `q` is not in rstest 0.7.0, where only `Ctrl+C`
exits). Closing stdin (`nohup`, `< /dev/null`) does not end the session.
Started as a background job on a terminal (`rstest --watch &`), rstest leaves
stdin alone so the shell does not suspend it for tty input; stop it with `kill`
or bring it back with `fg`. A session moved to the background later (Ctrl+Z,
then `bg`) may be suspended for tty input; `fg` resumes it.
With a flag that hands stdin to the test process (`--pdb`, `--trace`, `-s`,
`--capture=...`, `--debug`), `q` belongs to that process instead, and only
`Ctrl+C` exits.

Selection is **incremental** across the session. The import graph that maps a
source change to affected tests is built once and then kept warm: each save
re-checks every file's modification time and size, and re-reads only the files
that changed, patching their graph edges in place, instead
of re-walking and re-parsing the whole tree every save. Adding or deleting a
`.py` file rebuilds the graph fully (a new module can be imported by files that
did not themselves change), reusing the cached parse of every untouched file. On
large suites this is the difference between each save reselecting in tens of
milliseconds versus hundreds; on a 1,600-file tree it is roughly a 4x cut to the
per-save selection latency.

Watch reruns default to [`--order fail-fast`](#-order-throughputfail-fast)
so a fresh failure surfaces first on each save; add `-x`/`--maxfail=1` to
stop at it. Pass `--order throughput` to opt back into packing.

### `--debug[=PORT]`

Run under [debugpy](https://github.com/microsoft/debugpy) for editor
debugging (VS Code and any DAP client). Like `--pdb`, this forces
single-worker mode with inherited stdio so exactly one Python process hosts
the debugger; rstest then starts debugpy in that worker and **blocks until a
client attaches** before collecting, so breakpoints in conftest, collection,
and tests are all honored. Bare `--debug` listens on `127.0.0.1:5678`;
`--debug=PORT` overrides the port.

The target interpreter (`--python`) must have `debugpy` installed
(`pip install debugpy` in the test environment); without it the run proceeds
without a debugger and prints a hint. The editor attaches with a DAP *attach*
configuration pointed at the same host/port. `--reruns` and pooling are inert
here, exactly as under `--pdb`.

When the listener is up, the worker prints a machine-readable ready line to
**stderr** so an editor can attach deterministically instead of racing the
port:

```json
{"event": "debugpy", "host": "127.0.0.1", "port": 5678}
```

A human-readable `rstest: debugpy listening on …` line follows it; both
precede the blocking wait for the client.

## Configuration file

rstest-owned defaults can live in `pyproject.toml`, so a project commits
its runner settings once instead of repeating flags in CI and on every
machine:

```toml
[tool.rstest]
numprocesses = 8        # or "auto"
dist = "loadfile"
reruns = 2
reruns-only-known-flaky = true
worker-timeout = 300
collect = "full"        # or "lazy"
order = "throughput"    # or "fail-fast"
output = "bar"          # dots|verbose|bar|github|gitlab|buildkite|teamcity|azure|tap|json (default: bar on a TTY, dots off-TTY)
projects = ["libs/*", "services/api"]   # monorepo subprojects; replaces auto-discovery
```

These are the only keys read; other rstest flags (`--timeout`, `--html`,
`--junitxml`, `--fail-on-leak`, `--doctor-fail-on`, ...) are command-line only.
Keys use kebab-case. `worker-timeout` takes whole seconds (an integer), like
the `--worker-timeout` flag. The `order` key is **Unreleased** (not in rstest
0.7.0).

Precedence: command line > `[tool.rstest]` > built-in defaults. rstest reads
the `[tool.rstest]` of the **nearest** `pyproject.toml`, walking up from the
working directory, and stops there even if that file has no `[tool.rstest]`
table. pytest's own options stay where they always were
(`[tool.pytest.ini_options]`, `addopts`, ...), but rstest-owned flags placed
in `addopts` or `PYTEST_ADDOPTS` are **not** read by rstest (see the warning at
the top of this page).

**Invalid entries are reported, then ignored** (**Unreleased**: rstest 0.7.0
dropped them silently). An unknown key or a wrong-typed value prints one
warning to stderr per run, and that setting falls back to its default:

```text
rstest: /repo/pyproject.toml: ignoring unknown [tool.rstest] key `worker_timeout` (did you mean `worker-timeout`?)
rstest: /repo/pyproject.toml: ignoring [tool.rstest] reruns = "2" (expected a non-negative integer)
```

The `did you mean` hint appears only when the kebab-case spelling is a real
key. The expected types are: a non-negative integer (`reruns`,
`worker-timeout`), a non-negative integer or `"auto"` (`numprocesses`),
`true` or `false` (`reruns-only-known-flaky`), a string (`dist`, `collect`,
`order`, `output`), and a list of glob strings (`projects`). Only the type is
checked here; a string with an unknown value (`dist = "bogus"`) is still
validated when the run starts, exactly as the same value passed as a flag.

**Monorepo roots read only `projects`.** At a
[monorepo](../concepts/monorepo.md) root, the root `[tool.rstest]` supplies the
project list and nothing else: the worker budget comes from the command-line
`-n` (else `auto`), and `dist`, `order`, `output`, `reruns` and
`worker-timeout` reach the projects only when given on the root command line.
Each project runs as its own rstest and reads its own nearest `pyproject.toml`,
so a project with a `pyproject.toml` never sees the root's keys, while a
project without one (configured by `pytest.toml`, `pytest.ini`, `tox.ini` or
`setup.cfg`) finds the root `pyproject.toml` and uses its `[tool.rstest]`. See
[Monorepo mode](../concepts/monorepo.md#session-isolation).

## Forwarded pytest flags

Everything not listed above is passed to the vendored pytest core
unchanged: `-k`, `-m`, `-x`, `--maxfail`, `-q`, `-v`/`-vv`, `--lf`,
`--ff`, `-W`, `-p`, `--tb`, `--color`, `--basetemp`, plugin flags, ...

Three of them get extra orchestration on top of their per-session meaning:

- **`-x` / `--maxfail=N`**: coordinated globally. When the threshold is
  reached across all workers, dispatch halts and every worker winds down.
  In-flight tests finish (bounded overshoot, as with pytest-xdist).
- **`--lf` / `--ff`**: the last-failed cache is written by rstest from
  merged results (workers each see only their own failures), so a
  follow-up `--lf` behaves exactly as after a serial run.
- **`-v`**: rendered by rstest as one line per test, in completion order
  across workers, each prefixed with the worker that ran it (`[gw2] ...`,
  xdist's convention). Failure headers carry the same attribution; the
  worker also appears per-test in `--report-json`.

## Passthrough-IO flags

Flags that need pytest's own terminal (or stdin) force single-worker mode
with inherited stdio, and pytest renders its own output:

```text
--collect-only / --co     -s / --capture=...     --pdb     --trace     --debug
```

This **overrides any `-n` value or `[tool.rstest]` worker count**, e.g.
`rstest -n 8 --pdb` runs one session, not eight, and prints no worker
banner. When the worker count was set explicitly (above 1), rstest warns on
stderr, naming the flag:

```text
rstest: -s runs the session in a single process with pytest's own output, so -n 8 is ignored (no parallel workers); drop -s to run in parallel
```

The default `-n auto` stays quiet (plain `rstest -s` is an ordinary request
for pytest's `-s`), and so does `--co`, which runs no tests. `--reruns` is
likewise inert on this path (unlike plain `-n 0/1`, where `--reruns` runs a
one-worker pool). Drop the passthrough flag to get the pool back.

The stepwise flags also force single-worker mode, but for sequencing rather
than IO: stepwise resumes from a single nodeid cursor into one global
collection order, which parallel, duration-ordered dispatch cannot
reproduce (xdist has the same constraint; stepwise wants `-n 0`):

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
