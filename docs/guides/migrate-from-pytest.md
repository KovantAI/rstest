# Migrating from pytest

The short version: install rstest, run `rstest` where you ran `pytest`, and
check the list under [What changes](#what-changes) before you rely on it in
CI. The checklist below is the order to do it in; the rest of the page is
the long version: what is identical, what differs, how to roll out in
stages, and how to roll back.

## A migration checklist

1. **Still on pytest 8?** Do [Upgrading to pytest 9](upgrade-to-pytest9.md)
   first: rstest always runs pytest 9, so until your suite does too, any
   difference the next steps report can be pytest 8 versus 9 rather than
   rstest ([why](../getting-started/installation.md#your-suite-runs-on-pytest-9)).
2. `rstest try`: compares a plain pytest run with `rstest -n auto` and
   reports parity and speed
   ([how it works](../getting-started/evaluating.md#try-it-first)). If it
   flags differences, it points you at `migrate-check` (step 4).
3. `rstest -n 0`: confirm per-test outcomes match a plain `pytest` run with
   pytest 9.1.x installed, the version rstest vendors. Matching outcomes are
   the contract; report a bug if they differ. Move any rstest-owned flags out
   of `addopts` first ([why](#addopts-and-pytest_addopts)). See
   [The escape hatch](#the-escape-hatch).
4. `rstest migrate-check`: **the preflight that does the triage for you.**
   It classifies each test that fails only in parallel and names its fix;
   [Parallel safety](parallel-safety.md) is the reference for the remedies
   (`@pytest.mark.serial`, `--dist loadfile`, or fixing the shared state).
   See [The migrate-check preflight](#the-migrate-check-preflight) below.
5. `rstest`: run parallel. Green? Roll it out in CI with
   [the staged rollout](#rolling-out-in-stages-and-rolling-back).
6. Run `rstest --doctor` once to see where the suite's time goes and what
   caps its parallel speedup.

To have a coding agent run this checklist for you, see
[Driving it with Claude](#driving-it-with-claude-the-migrate-to-rstest-skill).

## The migrate-check preflight

`rstest migrate-check` is not a test run: it is a parallel-readiness
report. It turns the manual triage of parallel-only failures (read the
guide, classify each by hand) into one command.

It first collects the suite twice and flags test ids that differ between
collections (a per-process address or uuid is a hard blocker, since workers
can't agree on the test set; a timestamp may bail). If the ids are stable, it
runs the suite in parallel (at least two workers) and sorts every failing
test into one of six classes. Five are parallel-only: order dependency,
isolation leak (a [polluter](../concepts/glossary.md#polluter) test leaves
global state behind that breaks a later test on the same worker), wall-clock
sensitivity, intrinsic flake, and inconclusive. The sixth, not
parallel-specific, is a test that also fails at `-n 0`, reported apart as a
pre-existing problem. It names the fix for each class, and for order and
isolation failures it bisects the polluting file, for at most three such
tests; run [`rstest bisect`](../reference/cli-commands.md#bisect-nodeid) on
any others. The full classification is in the
[`migrate-check` reference](../reference/cli-commands.md#migrate-check).

It exits `1` if any blocking unstable id or parallelism-specific failure is
found (`2` if it couldn't judge, for example no usable interpreter), so a red
job tells you which one to fix. That makes it a **CI gate** that blocks new
parallel-unsafe tests while the suite migrates: no new isolation leak,
order dependency, or unstable-id site sneaks in green.

```yaml
      - name: migrate-check gate
        run: |
          rstest migrate-check --migrate-check-json migrate.json \
                 --migrate-allow tests/legacy/   # known-unsafe backlog, tolerated
      - uses: actions/upload-artifact@v7
        if: always()
        with:
          name: migrate-check
          path: migrate.json
```

`--migrate-check-json` writes the findings as a versioned JSON document
([schema reference](../reference/report-json.md#migrate-check-json)) for
tooling and trending. `--migrate-allow <substr>` accepts known findings by
nodeid/site substring; they're still reported (marked `(allowed)`) but don't
fail the build, so the gate goes red only on **new** issues while you work
through the backlog. Flag reference:
[`--migrate-check-json`](../reference/cli-commands.md#-migrate-check-json-path)
and [`--migrate-allow`](../reference/cli-commands.md#-migrate-allow-substring).

The gate is heavier than a normal run (it collects twice, then reruns the
failing files serially, twice, and under `--dist loadfile` to tell the
classes apart), so run it in its own job or on a
schedule rather than on every push if the suite is large. Once the suite
reports `ready`, drop the gate and just run `rstest`.

### Which command when?

`migrate-check` is the onboarding preflight (unstable ids first, a verdict
per failure); `audit` is the repeatable fix loop on the same classification,
ending in a ready-to-paste `conftest.py` block that marks the serial-fixable
tests.

| You want to… | Run | It tells you |
|---|---|---|
| Check if rstest is worth adopting | [`rstest try`](../getting-started/evaluating.md#try-it-first) | Parity and speedup against plain pytest, with no config or code changes |
| Fix tests that fail **only** in parallel after switching | [`rstest migrate-check`](../reference/cli-commands.md#migrate-check) | Unstable test ids first, then each failure in one of six classes (the five parallel-only ones are order dependency, isolation leak, wall-clock, intrinsic flake and inconclusive) with its fix |
| Quarantine the parallel-unsafe tests in one step | [`rstest audit`](../reference/cli-commands.md#audit) | Same classification, repeatable, plus a `conftest.py` block marking exactly the serial-fixable tests `@pytest.mark.serial` |
| Understand why a passing suite is **slow** | [`rstest --doctor`](doctor.md) | Where test time goes (wait-bound tests, a long-pole test, poor parallel balance) |

## What stays identical

**Your test code.** Nothing changes: fixtures, conftest hierarchies,
parametrize, marks, `pytest.raises`, assertion introspection, all of it
runs through a vendored pytest core (currently pytest 9.1.1), not a
reimplementation.

**Your plugins.** Plugins installed in the environment load through the
normal `pytest11` entry points against the vendored core. pytest-django,
pytest-asyncio, pytest-aiohttp, pytest-mock, and hypothesis are exercised
against real suites in rstest's compatibility battery. Most plugin flags
forward like any other pytest flag; the exceptions are in
[Flags rstest owns](#flags-rstest-owns) below.

**Your configuration.** Every pytest 9 config file (`pytest.toml`,
`.pytest.toml`, `pytest.ini`, `.pytest.ini`, `pyproject.toml` with
`[tool.pytest]` or `[tool.pytest.ini_options]`, `tox.ini`, `setup.cfg`),
including `addopts`, `testpaths`, `python_files`, `markers` and
`filterwarnings`, is read by the vendored core exactly as pytest reads it.
rstest's own lookup (for `-n auto` sizing, `--collect lazy`, `--watch` and
monorepo discovery) follows the same file order. One catch: rstest's *own* flags are read only
from the command line and `[tool.rstest]`, never from `addopts` (see
[below](#addopts-and-pytest_addopts)).

## Flags rstest owns

rstest forwards `-k`, `-m`, `-x`, `--maxfail`, `--lf`, `--ff`, `-W`, `-p`,
`--tb` and plugin options to the test session unchanged. It keeps about 50
flags for itself (the full list is the [CLI reference](../reference/cli.md)).
Most have rstest-only names (`--doctor`, `--watch`, `--report-json`,
`--python`, ...), but a few share a name with pytest core or a popular
plugin. On the command line, rstest takes these and the plugin never sees
them:

--8<-- "docs/_snippets/shadowed-flags.md"

To hand one of these to pytest or its plugin instead, put it after `--`:
everything after `--` goes to the session untouched. For example
`rstest -n 0 -- --html=report.html` gets pytest-html's own report, and
`rstest -n 0 -- --debug` gets pytest's debug log. Several of those plugins
only do anything at `-n 0`; see [Plugins](plugins.md).

### `addopts` and `PYTEST_ADDOPTS`

rstest does not read `addopts` or `PYTEST_ADDOPTS` for its own flags. Those
options still reach the vendored pytest session, so a shared-name flag set
there behaves as the *plugin's* flag, with the plugin's limits:

- `addopts = --reruns 2`: at `-n 0` pytest-rerunfailures reruns as usual.
  In the pool, rstest unregisters that plugin (it would crash there), so
  **nothing reruns and nothing warns**.
- `addopts = --junitxml=report.xml`: at `-n 0` pytest writes the file. In
  the pool, pytest's JUnit writer skips worker processes, so **no file is
  written**.
- `addopts = --html=report.html`: same pattern; pytest-html writes nothing
  in the pool.
- `addopts = -n 4 --dist loadgroup` (pytest-xdist): rstest reads neither,
  so `xdist_group` co-location is lost without a warning; see
  [If xdist is still in your ini](migrate-from-xdist.md#if-xdist-is-still-in-your-ini).

Move these to the rstest command line or `[tool.rstest]` (keys exist for
`reruns`, `numprocesses` and `dist`, e.g. `dist = "loadgroup"`) when you
switch.

## What changes

**Parallel by default.** This is the headline difference. pytest runs your
tests one at a time; rstest runs them on `auto` workers and says so in its
header line. Check each item against your suite:

- [ ] *Session/module-scoped fixtures instantiate once per worker*, not
  once per run, as under pytest-xdist; a session-scoped database or server
  fixture must tolerate N concurrent instances
  ([what to check](parallel-safety.md#session-scoped-fixtures-duplicate)).
- [ ] *`pytest_configure`, `pytest_sessionstart` and `pytest_sessionfinish`
  run in every worker*, concurrently. A conftest hook that creates a shared
  resource or writes a shared file must be idempotent or keyed on the worker
  id (`RSTEST_WORKER_ID` / `workerinput["workerid"]`).
- [ ] *Tests run in a different order*, interleaved across workers. Tests
  that depend on a previous test's side effects need [`--dist
  loadfile`](parallel-safety.md#file-affinity) or a fix.
- [ ] *Large suites may switch to lazy collection* once the cache is warm
  (at least 2000 cached tests and `tests × workers` of at least 16 000, under
  `--dist load` or `loadfile` only, and never with a path or nodeid selection,
  `--shard`, `--shuffle`, `--incremental` or doctests; full rules in
  [Auto-default](../concepts/lazy-collection.md#auto-default)). Each
  worker then imports only the test files it runs, so a `skipif` that reads
  `sys.modules` or a test that relies on a sibling module's import can behave
  differently. A banner line says when this happens; pin `--collect full` if
  your suite depends on every module being imported. See
  [Lazy collection](../concepts/lazy-collection.md#the-compatibility-trade).
- [ ] *Reordering in `pytest_collection_modifyitems` does not guarantee run
  order* at `-n ≥ 2`. Deselection is honored and dispatch starts from your
  order, but cached slow tests (1s or more) go first and tests on different
  workers run concurrently. This includes plugins that reorder: pytest-django,
  for example, moves `TestCase` tests ahead of `TransactionTestCase` in that
  hook, so expect that ordering not to hold in the pool (inferred from the
  mechanism, not verified against a Django suite). Use `-n 0` or an affinity
  `--dist` mode where order matters.
- [ ] *Custom `pytest_terminal_summary` output is not shown* at `-n ≥ 2`:
  the hook runs in each worker, but rstest renders one merged terminal.
- [ ] *pytest-rerunfailures is unregistered in pool workers*; rstest's own
  `--reruns` / `@pytest.mark.flaky` replace it.
- [ ] *Shared-name flags and `addopts`*: see
  [Flags rstest owns](#flags-rstest-owns).
- [ ] *Output interleaves across workers* under `-v`, in completion order.

The full hook contract is in [Plugins: hook coverage](plugins.md#hook-coverage).

**Test output rendering.** rstest renders progress, failures, and summaries
itself (from the same data pytest would use). Failure tracebacks, captured
sections, warnings summaries, and counts match pytest's content, but
plugins that *draw on the terminal* (pytest-sugar, pytest-rich) won't paint
their UI: rstest owns the terminal.

**Some files land elsewhere.** Each worker gets a disjoint `tmp_path` root
(like xdist's `popen-gwN`). `--junitxml` is written by rstest itself from
merged results, since per-worker sessions would clobber a shared file. The
`--lf` cache is likewise written merged.

## Rolling out in stages, and rolling back

rstest doesn't touch your pytest setup, so switching back is a one-line
change as long as you keep the pytest side intact during the rollout and
haven't yet adopted rstest-only features (see
[What ties you to rstest](#what-ties-you-to-rstest)):

1. **Shadow.** Add an rstest job next to your existing pytest (or
   pytest-xdist) CI job. Keep the old job as the required check. Keep pytest
   installed: `rstest try` compares against it, and it's your fallback.
   rstest has no pytest dependency, so a `pytest<9` pin does not conflict
   (but see [checklist step 1](#a-migration-checklist)).
2. **Compare.** Run both for a while. `rstest try` and
   `rstest migrate-check` ([above](#the-migrate-check-preflight)) tell you where results differ. Persist
   `.rstest_cache` between CI runs ([CI quickstart](ci-quickstart.md)):
   without it every shadow run is a cold run, and its timings understate
   rstest.
3. **Switch.** Make the rstest job required and the old job optional.
   Leave pytest-xdist and its `addopts` in place while the old job needs
   them, but copy any `--dist` mode to `[tool.rstest]`
   ([If xdist is still in your ini](migrate-from-xdist.md#if-xdist-is-still-in-your-ini)).
4. **Clean up** once you're confident: remove the old job, then the xdist
   flags from `addopts`, then pytest-xdist itself. Keep pytest-xdist
   installed if any conftest or plugin implements its hooks
   (`pytest_configure_node` and friends): without it, pytest stops with
   `unknown hook`. Or mark those impls `@pytest.hookimpl(optionalhook=True)`
   first (see [Controller-side hooks](migrate-from-xdist.md#controller-side-hooks)).

To roll back at any stage, point CI at `pytest` again. pytest ignores
`[tool.rstest]` in `pyproject.toml` and the `.rstest_cache/` directory, so
neither needs removing. If you already did step 4, restore the xdist flags
and dependency.

### What ties you to rstest

Every rstest feature you adopt adds a step to the rollback. Before you
switch back, grep for each construct below and apply the portable
alternative, or keep the plugin that provides it under pytest.

| If your suite uses | Under plain pytest | To stay portable |
|---|---|---|
| `@pytest.mark.serial` | Unknown marker: a `PytestUnknownMarkWarning`, and a collection error under `--strict-markers` or `-W error` | Register it in pytest's `markers` ini (`serial: run exclusively`). pytest-xdist has no equivalent; run those tests in a separate `-p no:xdist` job |
| `@pytest.mark.flaky(reruns=N)`, `--reruns` | Needs pytest-rerunfailures | Keep pytest-rerunfailures installed; it reads the same `reruns=` kwarg |
| `@pytest.mark.timeout(N)`, `--timeout` | Needs pytest-timeout | Keep pytest-timeout installed |
| `--html` | Needs pytest-html | Keep pytest-html installed |
| `worker_id` / `testrun_uid` fixtures | Fixture not found without pytest-xdist | Keep pytest-xdist installed, or define the fixtures in `conftest.py` |
| `RSTEST_WORKER_ID`, `RSTEST_WORKER_COUNT`, `RSTEST_RUN_UID` in code | Never set | Read `PYTEST_XDIST_WORKER` / `PYTEST_XDIST_WORKER_COUNT` or the `worker_id` fixture, which work under both runners |
| xdist hooks in `conftest.py` (`pytest_configure_node`, ...) | `unknown hook` error without pytest-xdist | Keep pytest-xdist installed, or mark the impls `@pytest.hookimpl(optionalhook=True)` |
| `[tool.rstest]` settings (`numprocesses`, `dist`, `reruns`, ...) | Ignored | Move the xdist equivalents (`-n`, `--dist`) back to `addopts` |
| rstest-only flags in CI (`--doctor*`, `--changed`, `--watch`, `--shard`, `--cache-*`, `--quarantine`, `--report-json`, `--fail-on-leak`) and subcommands (`try`, `migrate-check`, ...) | Usage error (`unrecognized arguments`) | Remove them from CI scripts; the features (doctor gates, changed-test selection, shared durations cache, quarantine) have no pytest equivalent |
| The rstest GitHub Action | Not applicable | Replace with a plain `pytest` step |

## The escape hatch

```console
$ rstest -n 0
```

`-n 0` (and `-n 1`, which is identical) is
[single-worker mode](../concepts/glossary.md#single-worker-mode): one worker,
one pytest session, pytest's exact per-test outcomes (only the
[flags rstest owns](#flags-rstest-owns) are still handled by rstest). With no
`--output` or `--reruns` set, the terminal output is pytest's own too,
byte-exact, and rstest only appends its extras (doctor, coverage, gate
messages) after pytest's summary line. If something behaves differently
under rstest's parallel mode, this is the first diagnostic: if it also fails
at `-n 0`, it's not parallelism.

Flags that need pytest's own terminal or a single global order switch to
this mode automatically: `--co`/`--collect-only`, `-s`, `--capture=...`,
`--pdb`, `--trace`, `--sw`/`--stepwise`, `--sw-skip`/`--stepwise-skip`,
`--sw-reset`/`--stepwise-reset`, and rstest's `--debug` (see
[Passthrough-IO flags](../reference/cli.md#passthrough-io-flags)).

## If your suite already uses pytest-xdist

Read [Migrating from pytest-xdist](migrate-from-xdist.md), starting with
[If xdist is still in your ini](migrate-from-xdist.md#if-xdist-is-still-in-your-ini).

## Driving it with Claude (the migrate-to-rstest skill)

rstest ships a Claude Code skill that runs this whole checklist for you. Run
`rstest install-skills` in your project (or install the `rstest` Claude Code
plugin; see [Agent skills](agent-skills.md)) and ask Claude to "migrate my
suite to rstest" or "parallelize my tests". It has two lanes:
**readiness** drives `migrate-check`, applies the right fix per verdict, and
wires up `[tool.rstest]` config + a CI gate; **speed** drives `--doctor` to find
the slowest tests, wait-bound (sleep/timeout) tests, the parallel-floor gate
test, and expensive fixtures, with the action for each. It asks before editing
your tests or CI.
