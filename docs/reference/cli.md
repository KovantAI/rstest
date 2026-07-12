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

- `-n auto` — one worker per logical core
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
  rejected; the duration cache is not updated. Honest scope note:
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

### `--output <dots|verbose|bar|github|json>`

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

#### Machine-readable styles

`github` renders the normal `dots` log and additionally emits a
[GitHub Actions](https://docs.github.com/actions) `::error` workflow command
for each failing test, so failures show up as inline annotations on the PR
diff:

```
::error file=<path>,title=<nodeid>,line=<n>::<traceback>
```

`file` comes from the nodeid path; `line` (1-based) from pytest's report
location, omitted when none is available. The traceback is escaped per the
workflow-command spec. Use it as your CI `--output`.

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

### `--doctor`

After the run, print a diagnosis: wait-bound tests (wall vs CPU time),
parallel-floor analysis (the tests that cap any `-n`), fixture hotspots
(with scope advice), and slowest files. Adds two cheap measurements to the
run; outcomes are unaffected.

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
  ⚡ speed:   pytest 4.5s  →  rstest 2.9s   (1.5× at -n auto)
================================================
  → drop-in ready: `rstest` is `pytest`, in parallel.
```

Exit 0 when outcomes are identical, 1 when they differ (it then points you at
`--migrate-check` to classify the differences — usually an unstable parametrize
id or a parallel-only failure), 2 when it couldn't run pytest or rstest refused
to dispatch. A pre-existing red pytest run is reported as such, not blamed on
rstest.

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

The first slice of a broader migration assistant (see the project's
`DESIGN-migrate-check.md`).

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

PR-aware in CI: on a GitHub Actions pull_request job (`GITHUB_BASE_REF`
set), bare `--changed` diffs against the merge-base with the PR base
branch instead of `HEAD` — a clean checkout of the PR commit still
selects exactly the PR's files. Requires the base branch to be fetched
(`actions/checkout` with `fetch-depth: 0`); an unfetched base is an
error, never a silent full skip. An explicit `REV` disables the
auto-targeting.

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

Crash-aware: **while `--reruns` (or `@pytest.mark.flaky`) budget remains**,
a test that killed its worker is retried on the replacement worker, bounded
by both the rerun and restart budgets (the segfault-loop guard) — something
in-process rerun plugins cannot do. Once that budget is exhausted, or with
no reruns configured at all, the crashed test is reported FAILED and not
retried (see [crash handling](troubleshooting.md#a-worker-crashed-what-happened-to-its-tests)). The flag is intercepted by rstest and an installed
pytest-rerunfailures is neutralized inside workers, so nothing
double-reruns.

### `--doctor-json <path>`

Write the doctor analysis as JSON (stable, versioned schema — currently
`1`) for CI trending. Implies doctor instrumentation; combine with
`--doctor` for the human report too. Field reference:
[Doctor JSON](report-json.md#doctor-json).

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
output = "bar"          # dots | verbose | bar | github | json (default: bar on a TTY, dots off-TTY)
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
