# CLI

```
rstest [RSTEST FLAGS] [PATHS] [PYTEST FLAGS]
```

rstest owns a small set of flags; **everything else forwards to the test
session verbatim**, so the entire pytest flag surface — including flags
added by your plugins — works without translation.

## rstest-owned flags

### `-n, --numprocesses <N|auto>`

Worker count. Default `auto` (logical cores).

- `-n auto` — one worker per available logical core, then capped by what the
  suite can use (test-file count and cached total runtime). On Linux the core
  count honors the process CPU affinity mask and cgroup CPU quota, so a
  CPU-limited container (`docker run --cpus=2`, a constrained CI runner) sees
  its allocation, not the host's core count — no over-subscription. Pin `-n
  <k>` if you want a fixed count regardless.
- `-n 4` — four workers
- `-n 0` or `-n 1` — **single-worker mode**: one pytest session, byte-exact
  pytest semantics; identical to each other, with no worker identity below
  `-n 2`. See [Byte-exact mode](../concepts/glossary.md#byte-exact-mode)
  (and, for migrators, how it differs from pytest-xdist's `-n 1`)

### `--dist <load|loadfile|loadscope|loadgroup|each>`

Distribution mode. Default `load`.

- `load` — test-granular, dynamic, duration-aware: cached slow tests
  dispatch first and individually; the rest flows in contiguous chunks
  that preserve module-fixture locality.
- `loadfile` — whole files stay on one worker, in file order. For
  order-dependent suites.
- `loadscope` — fixture-scope affinity: a class's tests stay together,
  module-level functions stay with their module. For expensive
  class/module fixtures that must not duplicate.
- `loadgroup` — `@pytest.mark.xdist_group("name")` affinity, across
  files; unmarked tests distribute individually.
- `each` — every worker runs the FULL suite. Counts are per-worker
  totals and outcomes are keyed `nodeid [gwN]`; `--reruns` is
  rejected (the mode exists to *expose* per-worker outcome
  differences, and rerunning failures would mask exactly the
  flakiness `each` is there to surface); the duration cache is not
  updated. Honest scope note:
  every worker uses the same interpreter, so this validates isolation
  and shakes out flakiness — xdist's heterogeneous-environment use
  (`--tx` gateways) has no rstest equivalent.

All five are pytest-xdist-compatible mode names.

### `--durations <N>` / `--durations-min <SECS>`

pytest's slowest-durations report, rendered by the orchestrator after
the run (worker terminals are captured, so the merged block covers all
workers). `--durations=0` shows everything; entries under
`--durations-min` (default 0.005s) are hidden with pytest's note unless
`-vv`. Phase granularity matches pytest: setup, call, and teardown each
get a line.

### `--output <dots|verbose|bar|github|gitlab|buildkite|teamcity|azure|tap|json>` { #-output-dotsverbosebargithubjson }

Terminal output style. The default is **automatic**: on an interactive
terminal it's `bar` (the pretty view); off a TTY (CI, pipes) it falls back
to `dots`, so logs stay byte-stable. Pass `--output` to pin a style.

`dots` is pytest's one-char-per-test (`.`/`F`/`s`/…) with a running
percentage. `verbose` is the `-v` equivalent: one `nodeid OUTCOME` line
per test. `bar` is a pytest-sugar-style view: a per-test result line
(`✓ nodeid` / `✗ nodeid`) as each test finishes, the failure traceback
inlined right under a failing test, a live filled progress bar in the
footer, and a closing results bar — a full-width bar whose segments are
colored green/red/yellow in proportion to passed/failed/skipped, over the
test count:

```
Results (4.20s):
  ██████████████████████████████ 29/29
```


`bar` works **under the parallel pool**, where terminal-reporter plugins
like pytest-sugar structurally can't (each worker would fight for the one
terminal — the same reason pytest-sugar is disabled under pytest-xdist); rstest
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

```
::error file=<path>,title=<nodeid>,line=<n>::<traceback>
```

`file` comes from the nodeid path; `line` (1-based — the annotator adds 1 to
pytest's 0-based `report.location`, matching the `lineno` in the JSON reports)
from pytest's report location, omitted when none is available. The traceback is escaped per the
workflow-command spec. Use it as your CI `--output`.

Tests that passed only after reruns (`--reruns` /
`@pytest.mark.flaky`) additionally emit a `::warning` annotation
(`flaky: passed only after N reruns`) — the run stays green, but the
flake is visible on the PR without opening the log.

`azure` renders the normal `dots` log and additionally emits an [Azure
Pipelines logging
command](https://learn.microsoft.com/azure/devops/pipelines/scripts/logging-commands)
per failing test, surfaced as an inline issue on the file in the PR:

```
##vso[task.logissue type=error;sourcepath=<path>;linenumber=<n>]<nodeid>: <message>
```

`sourcepath` comes from the nodeid path; `linenumber` (1-based — the 0-based
`report.location` plus 1, as in the GitHub annotator) from pytest's report
location, omitted when none is available. The message is
collapsed to one line (logissue is single-line). Flaky-passed tests
(`--reruns`) additionally emit a `type=warning` logissue — green run,
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
as each test finishes — a `testStarted`/`testFinished` pair per test,
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

`json` makes stdout a pure **newline-delimited JSON** stream — one
`testreport` object per phase as each test finishes, closed by a
`sessionfinish` envelope. No banner, footer, or human summary is printed,
so every line parses on its own. Built for editors and tooling that consume
results live; see [Streaming JSON](report-json.md#streaming-json) for the
event shapes and fields. This differs from
[`--report-json`](#-report-json-path), which writes a single end-of-run
snapshot document to a file.

### `--doctest-modules`

Works as in pytest — forwarded to the vendored core, which collects
doctest items from all modules; they dispatch across workers like any
other test. `--doctest-glob` and friends forward the same way.

### `--collect <full|lazy>`

Collection strategy. Default `full`: every worker collects the whole
suite (identical sessions, hash-verified). `lazy`: each test file is
collected exactly once, on one worker, on demand — a distributed single
collection pass. Big win for narrow `-k`/`-m` selections on large
suites; full runs of suites with a few giant files prefer `full` (or
`lazy` with an explicit `--dist load`, which enables work-stealing).
See [Lazy collection](../concepts/lazy-collection.md) for semantics and
the compatibility trade. Configurable via `[tool.rstest] collect`.

With `--collect lazy`, `--dist loadscope|loadgroup` are rejected, and
nodeid/`--pyargs` arguments fall back to full collection.

### `--durations-regress <RATIO>`

Gate CI on per-test duration regressions. After the run, each test's
wall time is compared against the duration cache
(`.rstest_cache/durations.json` — the same file LPT scheduling uses;
restore it from your CI cache). Any test that grew past `RATIO` × its
baseline is listed and the run exits 1:

```
=========== duration regressions (>= 2x baseline) ===========
     0.10s ->    1.21s  tests/test_api.py::test_poll
```

Jitter-floored so CI noise can't flag: baselines under 50ms and
absolute growth under 0.5s never count, and tests absent from the
baseline (new or renamed) are skipped. A missing baseline file skips
the comparison entirely (first run / cold cache). The comparison runs
before the cache is refreshed with this run's times.

### `--shuffle[=SEED]`

Run tests in a seeded random order (the pytest-randomly idea, applied to
the orchestrator's dispatch queue). Order dependence is the central
parallel-readiness hazard; a shuffled run flushes it out on demand —
in CI or before enabling more workers — instead of waiting for a
scheduling change to bite. Without a value the seed is chosen per run
and printed; reproduce a failing order with `--shuffle=SEED` (add
`-n 2 --dist loadfile` to keep the repro stable).

Affinity modes (`loadfile`/`loadscope`/`loadgroup`) shuffle the group
order and keep in-group order intact — in-group order is the affinity
contract. In `load` mode the shuffle replaces duration-aware
sequencing for that run. Requires the parallel pool with full
collection: single-worker mode, `--collect lazy`, and `--dist each`
are refused (not silently ignored — a run probing for order
dependence must not quietly run ordered).

### `--shard <K/N>`

Split the suite across `N` independent CI jobs and run only shard `K`
(1-based: `1/4` … `4/4`). Each job partitions the collected tests into
`N` buckets balanced by the duration cache
(`.rstest_cache/durations.json`, longest-processing-time-first) and runs
its bucket; a cold cache falls back to an even count split. Buckets are
disjoint and cover the whole suite, so merging the per-job JUnit
reconstructs the full run.

Orthogonal to `-n`: each shard still runs its slice across local
workers. Requires the parallel pool (`-n ≥ 2`); rejected with
`--shuffle` (a per-run shuffle breaks the identical-partition guarantee
that lets jobs agree without coordinating) and `--dist each`. Under
`--collect lazy` it shards at file granularity. Composes with
`--changed` (selection first, then partition). Restore the **same**
duration cache on every job so their partitions match — see the
[Sharding guide](../guides/sharding.md).

### `--doctor`

After the run, print a diagnosis: wait-bound tests (wall vs CPU time),
parallel-floor analysis (the tests that cap any `-n`), parallel efficiency
(realized speedup and per-worker load imbalance, `-n > 1` only), fixture
hotspots (with scope advice), and slowest files. Adds two cheap measurements
to the run; outcomes are unaffected.

### `--try`

The zero-config "should I switch?" proof. Runs your suite once under plain
`pytest` and once under `rstest -n auto`, then prints the only two things that
matter: whether the outcomes are **identical** (the `-n 0 ≡ pytest` contract,
checked against your real pytest) and how much **faster** rstest is, with a
rough CI-time saving. No flags, no config.

```console
$ rstest --try
================= rstest --try =================
  ✓ parity:  8337 tests — identical outcomes to pytest
  ⚡ speed:   pytest 96s  →  rstest 21s   (4.6× at -n auto)
================================================
  → drop-in ready: `rstest` is `pytest`, in parallel.
```

Exit 0 when outcomes are identical, 1 when they differ (it then points you at
`--migrate-check` to classify the differences — usually an unstable parametrize
id or a parallel-only failure), 2 when it couldn't run pytest or rstest refused
to dispatch. A pre-existing red pytest run is reported as such, not blamed on
rstest.

`--try` is the one command that needs **pytest installed on its own** (it runs
your suite under plain `pytest` for the baseline). rstest itself vendors its
core and doesn't otherwise require an external pytest; if `pytest` isn't on
PATH, `--try` exits 2. `--migrate-check` and normal runs have no such
requirement.

### `--migrate-check`

Parallel-readiness preflight, not a run. Collects the suite **twice** and
diffs the id sets; ids present in only one collection are run-to-run unstable.
Reports each offending parametrize site, classified by why its id is unstable:

- **address / uuid** — per-process values (a `repr()`-fallback id embedding
  `0x…`, or a uuid). These differ in *every* worker, so per-worker collections
  disagree and rstest must bail → the suite is forced to `-n 0`. Reported as
  **WILL bail**.
- **time** — a timestamp/date in the id. Usually stable enough *within* one run
  (all workers collect near-simultaneously), so it typically runs at `-n auto`.
  Reported as **may bail**.

If no WILL-bail id is found, it then **runs the suite at `-n auto`** and
classifies any test that fails only under parallelism. The discriminators
(`-n 0` twice and `--dist loadfile`) are **scoped to the files containing
failures**, so cost scales with the number of failing files, not the suite
size — a clean suite runs no discriminators at all:

- **NOT PARALLEL-SPECIFIC** — also fails at `-n 0`; a pre-existing bug/env gap,
  summarized (not a migration concern).
- **INTRINSIC FLAKE** — serial repeats disagree; flaky under any runner.
- **ORDER DEPENDENCY** — passes serial and under `--dist loadfile`, fails under
  `load`; run with `loadfile` or fix the in-file coupling.
- **WALL-CLOCK / LOAD-SENSITIVE** — passes serial, fails parallel, and is
  wait-bound (wall ≫ cpu): a real-time deadline that misses under
  oversubscription. Mock the clock / drop the tight upper bound; stopgap `-n 4`.
- **ISOLATION / CO-LOCATION** — passes serial, fails under both `load` and
  `loadfile`, and is *not* wait-bound; a leaked-global-state defect — reset it
  per test, or `@pytest.mark.serial`.

For ORDER-DEPENDENCY and ISOLATION findings it then **bisects the polluter**
(capped): it binary-searches for the file whose tests, run serially before the
victim, reproduce the failure, and reports `POLLUTED BY: <file>` (cross-file),
`SAME-FILE co-location (inspect <file>)`, or — when no serial ordering
reproduces — that the failure is likely a concurrent-resource race rather than
state pollution.

Each finding prints the upstream fix (for unstable ids: give the parametrize a
stable `ids=`) and the rstest stopgap. Exits non-zero if any WILL-bail id or
parallelism-specific failure is found — usable as a CI gate (see
`--migrate-check-json` and `--migrate-allow` below for the machine-readable
form and the known-issue allow-list).

### `--migrate-check-json <path>`

Write the migrate-check findings as a single versioned JSON document (schema
`1`) — the machine-readable surface for CI gating and trending. Implies
`--migrate-check`; pass the bare flag too to also print the human report. The
document carries the unstable-id sites and the classified parallel findings,
each with its verdict, fix, allow-list status, and bisected polluter:
`{meta, ready, tests_collected, will_bail_count, unstable_ids[], parallel{…}}`.
Field reference: [Migrate-check JSON](report-json.md#migrate-check-json).

### `--migrate-allow <SUBSTRING>`

Accept a known finding so it does not fail the exit code (repeatable). Any
finding whose nodeid or unstable-id site **contains** SUBSTRING is still
reported — marked `(allowed)` in the human output and `"allowed": true` in the
JSON — but excluded from the non-zero gate. This lets CI gate on **new**
parallel-unsafe tests while tolerating a triaged backlog: allow-list today's
findings, and the build only goes red when a fresh one appears.

The first slice of a broader migration assistant.

### `--only-rerun <REGEX>`

With reruns active, retry only failures whose error text matches the
pattern (repeatable; any match retries). Same semantics as
pytest-rerunfailures' flag — useful for retrying known-transient errors
(`ConnectionError`, `TimeoutError`) while letting real failures fail fast.

### `--worker-timeout <SECS>`

Hang backstop, off by default: a worker stuck on **one test** (any
phase — setup, call, or teardown) longer than
SECS is killed — the test is reported failed with a timeout message, the
worker's other tests redistribute, and a replacement worker joins (the
crash-recovery machinery, same budgets). Use
[pytest-timeout](https://pypi.org/project/pytest-timeout/) for ordinary
per-test limits; `--worker-timeout` catches what in-process timeouts
can't interrupt — tests hard-blocked inside C extensions or deadlocked
threads. Under `--reruns`, a timed-out test is retried within the budget
(deadlocks can be races). Hangs OUTSIDE a test — during collection or
session config — are not covered by this watchdog.

### `--changed[=REV]`

Run only tests affected by changed files. Changes come from git (working
tree + untracked vs `HEAD`, or vs `REV` — e.g. `--changed=origin/main` in
CI) and map through a project import graph to the test files that can be
affected; only those run.

Conservative by construction: ambiguous module names select every match,
function-local imports count, a changed `conftest.py` selects its whole
subtree, and any config or non-Python change falls back to a full run.
Known gap: dynamic imports (`importlib.import_module`) produce no graph
edges — for correctness-critical runs, use `--changed-strict` below.
With nothing affected, the run prints
`no tests affected by N changed file(s)` and exits 0 without running.

PR-aware in CI: on a pull-request / merge-request job, bare `--changed`
diffs against the merge-base with the PR base branch instead of `HEAD` —
a clean checkout of the PR commit still selects exactly the PR's files.
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
variable — pass one explicitly or expose a build parameter as env.

### `--changed-strict`

`--changed` hardened for gating CI (merge queues). Implies `--changed`
(vs `HEAD`) when `--changed` isn't given. Three behavior changes:

- **A changed source file the import graph cannot connect to any test
  forces a FULL run** (naming the file) instead of silently selecting
  nothing for it — the dynamic-import / unused-module / deleted-file
  cases stop being false skips.
- **Monorepos: undeclared cross-project imports count as dependency
  edges.** Each project's Python files are scanned; an import resolving
  to a sibling's top-level modules adds the edge (with a warning naming
  both projects) even when the pyproject never declared it — the
  shared-workspace-venv trap. Namespace packages shared by several
  siblings over-connect, which errs toward running more, never less.
- **"Nothing affected" exits 5** (pytest's nothing-collected code)
  instead of 0, so a pipeline must consciously allow it rather than
  mistaking it for a green run.

Residual risk it cannot remove: imports constructed at runtime from
strings the scanner can't see (`importlib.import_module(f"plugins.{name}")`)
still produce no edges — name such modules in a test file import, or
keep full runs on the gating path.

### `--reruns <N>`

Rerun failed tests up to N times (requires `-n ≥ 2`). A test that then
passes is reported **flaky**: the run stays green, the test is counted in
the summary (`N flaky`), listed in its own section, and flagged in
`--report-json`. Only the final attempt's outcome and output are recorded.

Per-test budgets are available via `@pytest.mark.flaky(reruns=N)`, which
works with or without the global flag — but, like `--reruns`, the retry
machinery is orchestrator-side, so it too only takes effect at `-n ≥ 2`
(see [Markers](markers.md#pytestmarkflaky)).

Migration footgun: a `reruns` in `[tool.rstest]` (or `--reruns` on the
command line) is **silently inert** whenever the run drops to single-worker
— `-n 0`/`-n 1` or a passthrough-IO flag (`--pdb`, `-s`, …). At `-n 0/1`,
though, an installed pytest-rerunfailures takes over and honors `--reruns`
natively (rstest only neutralizes it inside the pool), so leave the plugin
installed if you rely on reruns in single-worker runs.

Crash-aware: **while `--reruns` (or `@pytest.mark.flaky`) budget remains**,
a test that killed its worker is retried on the replacement worker, bounded
by both the rerun and restart budgets (the segfault-loop guard) — something
in-process rerun plugins cannot do. Once that budget is exhausted, or with
no reruns configured at all, the crashed test is reported FAILED and not
retried (see [crash handling](../concepts/crash-handling.md)). The flag is intercepted by rstest and an installed
pytest-rerunfailures is neutralized inside workers, so nothing
double-reruns.

Reruns rescue a flake within one run; the flake history and
[`--quarantine`](#-quarantine-file) manage it across runs — see
[Flaky tests](../guides/flaky-tests.md).

### `--quarantine <FILE>`

Ring-fence known-flaky tests without hiding them. `FILE` lists nodeids
or `*` glob patterns (one per line, `#` comments):

```
# tracked in JIRA-1234, remove when fixed
tests/test_api.py::test_poll_eventually
tests/test_ws.py::*
```

A failure matching the list is demoted to a **quarantined** outcome:
counted separately in the summary (`N quarantined`), printed with its
traceback in its own section, flagged as a `quarantined` testcase
property in junit (no `<failure>` element — junit-gating CI stays
green) and in `--report-json` (schema 5), and never fatal — a run whose
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

### `--doctor-json <path>`

Write the doctor analysis as JSON (stable, versioned schema — currently
`2`) for CI trending. Implies doctor instrumentation; combine with
`--doctor` for the human report too. Field reference:
[Doctor JSON](report-json.md#doctor-json).

### `--doctor-md <path>`

Write the doctor analysis as GitHub-flavored markdown — the same signals
as the terminal report, rendered as job-summary tables. Implies doctor
instrumentation.

On GitHub Actions and Buildkite you rarely need the flag: any doctor run
(`--doctor`, `--doctor-json`, or `--doctor-md`) automatically publishes
this markdown to the job summary — appended to `$GITHUB_STEP_SUMMARY` on
GitHub, piped to `buildkite-agent annotate` (info style) on Buildkite —
so the report shows up on the run page with zero extra steps. GitLab and
TeamCity have no native markdown job-summary surface; use `--doctor-md`
and publish the file as an artifact.

### `--doctor-fail-on <COND>`

Fail the run when a doctor metric breaches a threshold — turning the
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

A metric whose section did not apply to the run is **skipped, not failed**
— e.g. `parallel_efficiency` at `-n 1` (no parallelism to measure) prints a
`not measured` note and never fails the gate. An unknown metric or malformed
condition aborts up front, before the run, so a typo can never become a gate
that silently never fires. `==`/`!=` are reliable only on the integer-valued
metrics (`tests`, `workers`); on a floating-point metric they almost never
match, so rstest warns and you should use a `<`/`>` threshold instead.

The failure block prints to stderr, so `--output json`/`tap` stay pure on
stdout. Under a passthrough-IO flag (`-s`/`--pdb`/`--co`) there is no doctor
instrumentation, so the gate can't run — rstest warns instead of passing green.

The conditions can live in your CI config or `pyproject.toml` invocation, so
non-GitHub CIs get the same gate the composite action offers externally.

### `--watch`

Watch the project and rerun on change. A change set consisting only of
test files reruns exactly those files (with your other flags); a source
(`.py`) change reruns the tests the import graph says are affected
(the `--changed` machinery; unresolvable changes fall back to the full
selection); a pytest-config change reruns the full selection. Ignores
VCS, caches, and virtualenvs. `Ctrl+C` exits.

### `--junitxml <path>`

Write merged results as JUnit XML. Intercepted by rstest (rather than
forwarded) because per-worker sessions would clobber a shared file; the
XML is rendered from merged results with pytest's classname conventions.

Only final outcomes appear: a test that passed after `--reruns` retries is
a passing `<testcase>` carrying a
`<property name="flaky" value="true"/>` (JUnit's standard extension
point), so junit-based dashboards can track flakes without parsing
`--report-json`.

### `--report-json <path>`

Write a per-test outcome snapshot: every test's setup/call/teardown
outcome, duration, source line, xfail flag, and skip reason. Stable schema
intended for tooling; see [Report JSON](report-json.md).

Combined with `--collect-only` (or `--co`) it writes a **discovery**
document instead — node ids, absolute file paths, source lines, and
markers, without running the suite. See
[Discovery JSON](report-json.md#discovery-json).

### `--python <path-or-version>`

Interpreter for the workers. Accepts either a path to an interpreter or a
version request — `3.12`, `>=3.12,<3.13`, `pypy@3.10`, `3.13t` (free-threaded).
Without it, rstest searches, in order: the active virtualenv (`$VIRTUAL_ENV`),
a `.venv` found walking up from the working directory, versioned `python` /
`pythonX.Y` names on `PATH`, and finally uv-managed interpreters as a fallback.
A `.python-version` file (or a `--python` version request) does not pick an
interpreter directly — it sets the version that filters those candidates.

## Configuration file

rstest-owned defaults can live in `pyproject.toml`, so a project commits
its runner settings once instead of repeating flags in CI and on every
machine:

```toml
[tool.rstest]
numprocesses = 8        # or "auto"
dist = "loadfile"
reruns = 2
worker-timeout = 300
collect = "full"        # or "lazy"
output = "bar"          # dots|verbose|bar|github|gitlab|buildkite|teamcity|azure|tap|json (default: bar on a TTY, dots off-TTY)
```

Precedence: command line > `[tool.rstest]` > built-in defaults. pytest's
own options stay where they always were (`[tool.pytest.ini_options]`,
`addopts`, ...).

## Forwarded pytest flags

Everything not listed above is passed to the vendored pytest core
unchanged: `-k`, `-m`, `-x`, `--maxfail`, `-q`, `-v`/`-vv`, `--lf`,
`--ff`, `-W`, `-p`, `--tb`, `--color`, `--basetemp`, plugin flags, ...

Three of them get extra orchestration on top of their per-session meaning:

- **`-x` / `--maxfail=N`** — coordinated globally: when the threshold is
  reached across all workers, dispatch halts and every worker winds down.
  In-flight tests finish (bounded overshoot, as with pytest-xdist).
- **`--lf` / `--ff`** — the last-failed cache is written by rstest from
  merged results (workers each see only their own failures), so a
  follow-up `--lf` behaves exactly as after a serial run.
- **`-v`** — rendered by rstest as one line per test, in completion order
  across workers, each prefixed with the worker that ran it (`[gw2] ...`,
  xdist's convention). Failure headers carry the same attribution; the
  worker also appears per-test in `--report-json`.

## Passthrough-IO flags

Flags that need pytest's own terminal (or stdin) force single-worker mode
with inherited stdio, and pytest renders its own output:

```
--collect-only / --co     -s / --capture=...     --pdb     --trace
```

This **overrides any `-n` value or `[tool.rstest]` worker count without
error** — e.g. `rstest -n 8 --pdb` runs one session, not eight. `--reruns`
is likewise inert on this path (like `-n 0/1`). Drop the passthrough flag to
get the pool back.

The stepwise flags also force single-worker mode, but for sequencing rather
than IO: stepwise resumes from a single nodeid cursor into one global
collection order, which parallel, duration-ordered dispatch cannot
reproduce (the same constraint xdist has — stepwise wants `-n 0`):

```
--sw / --stepwise     --sw-skip / --stepwise-skip     --sw-reset / --stepwise-reset
```

## Argument splitting

If a forwarded value collides with an rstest flag name, separate with
`--`:

```console
$ rstest tests -- -m "not slow" -k pattern
```

(Usually unnecessary — unknown flags forward automatically.)
