# Migrating from pytest

The short version: install rstest, run `rstest` where you ran `pytest`, and
check the list under [What changes](#what-changes) before you rely on it in
CI. The checklist below is the order to do it in; the rest of the page is
the long version: what is identical, what differs, how to roll out in
stages, and how to roll back.

## A migration checklist

1. `rstest try`: the one-command answer to "should we switch?". It runs the
   suite under your installed pytest and under `rstest -n auto`, and reports
   whether outcomes match and how much faster rstest is, before you commit to
   anything. It costs one serial pytest run plus one rstest run; if it flags
   differences, it points you at `migrate-check` (step 4).
2. **Still on pytest 8?** Do [Upgrading to pytest 9](upgrade-to-pytest9.md)
   first. Until then, a difference `try` reports can be pytest 8 versus 9,
   not rstest ([details](#what-changes)).
3. `rstest -n 0`: on pytest 9.1.x, confirm identical results to pytest (this
   is the contract; report a bug if not). Move any rstest-owned flags out of
   `addopts` first ([why](#addopts-and-pytest_addopts)). See
   [The escape hatch](#the-escape-hatch).
4. `rstest migrate-check`: **the preflight that does the triage for you.**
   Fix what it names, and steps 5 and 6 usually become a formality. See
   [The migrate-check preflight](#the-migrate-check-preflight) below for
   what it reports and how to keep it as a CI gate.
5. `rstest`: run parallel. Green? You're done. Roll it out in CI with
   [the staged rollout](#rolling-out-in-stages-and-rolling-back).
6. A few tests fail only in parallel? `migrate-check` already classified
   each one and named its fix; [Parallel safety](parallel-safety.md) is the
   reference for the remedies (`@pytest.mark.serial`, `--dist loadfile`, or
   fixing the shared state).
7. Run `rstest --doctor` once. It usually pays for the migration by
   itself.

To have a coding agent run this checklist for you, see
[Driving it with Claude](#driving-it-with-claude-the-migrate-to-rstest-skill).

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
- `addopts = -n 4 --dist loadgroup` (pytest-xdist): rstest reads neither.
  The worker count falls back to rstest's default (`auto`), the pool
  distributes with `load`, and `@pytest.mark.xdist_group` co-location is
  lost without a warning. xdist itself stays inert inside the workers; once
  pytest-xdist is uninstalled, pytest rejects these options as unknown
  (exit 4).

Move these to the rstest command line or `[tool.rstest]` (keys exist for
`reruns`, `numprocesses` and `dist`, e.g. `dist = "loadgroup"`) when you
switch.

## What changes

**Parallel by default.** This is the headline difference. pytest runs your
tests one at a time; rstest runs them on `auto` workers and says so in its
header line. Check each item against your suite:

- [ ] *Session/module-scoped fixtures instantiate once per worker*, not
  once per run, the same semantics as pytest-xdist. A session-scoped
  database or server fixture must tolerate N concurrent instances.
  (`rstest --doctor` flags session fixtures that ran more than once, but
  only expensive ones: at least 0.5s of total setup, top 8. Check cheap
  session fixtures by hand.)
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

**Pinned to an older pytest?** Price that step separately. rstest's
vendored core is pytest 9; if your suite and plugin set are pinned to
pytest 8 (or older), adopting rstest *includes* a pytest-9 migration:
deprecation warnings, plugin version bumps, the usual. `rstest -n 0` is
the cheap probe: it runs your suite under the pytest 9 core with your
installed pytest untouched (a few pytest 9 behavior changes raise no
error, so also check the list in the upgrade guide). Budget the runner switch as
"pytest upgrade first, then a one-line command change," not one step.
[Upgrading to pytest 9](upgrade-to-pytest9.md) is the concrete
checklist for that first step: the small set of 8→9 removals that actually
bite, with the grep and the fix for each.

## Rolling out in stages, and rolling back

rstest doesn't touch your pytest setup, so switching back is a one-line
change as long as you keep the pytest side intact during the rollout and
haven't yet adopted rstest-only features (see
[What ties you to rstest](#what-ties-you-to-rstest)):

1. **Shadow.** Add an rstest job next to your existing pytest (or
   pytest-xdist) CI job. Keep the old job as the required check. Keep pytest
   installed: `rstest try` compares against it, and it's your fallback.
   rstest does not install or upgrade pytest (it runs its own vendored
   pytest 9 core and has no pytest dependency), so a `pytest<9` pin does not
   conflict when you add it. Until that environment is on pytest 9, though,
   a difference `rstest try` reports can be a pytest 8 versus 9 difference
   rather than a parallelism one: check it at `rstest -n 0` (see
   [Upgrading to pytest 9](upgrade-to-pytest9.md)).
2. **Compare.** Run both for a while. `rstest try` and
   `rstest migrate-check` (below) tell you where results differ. Persist
   `.rstest_cache` between CI runs ([CI quickstart](ci-quickstart.md)):
   without it every shadow run is a cold run, and its timings understate
   rstest.
3. **Switch.** Make the rstest job required and the old job optional.
   Leave pytest-xdist and its `addopts` (`-n 4`, `--dist ...`) in place for
   now: rstest neutralizes them inside its workers, and the old job still
   needs them. rstest does not read them, though, so copy any `--dist` mode
   to `[tool.rstest] dist` (for example `dist = "loadgroup"`, or
   `xdist_group` co-location is lost) and pass `-n` to rstest if you want a
   fixed count.
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

One worker, one pytest session, pytest's exact per-test outcomes (only the
[flags rstest owns](#flags-rstest-owns) are still handled by rstest). If something
behaves differently under rstest's parallel mode, this is the first
diagnostic: if it also fails at `-n 0`, it's not parallelism.

Flags that need pytest's own terminal or a single global order switch to
this mode automatically: `--co`/`--collect-only`, `-s`, `--capture=...`,
`--pdb`, `--trace`, `--sw`/`--stepwise`, `--sw-skip`/`--stepwise-skip`,
`--sw-reset`/`--stepwise-reset`, and rstest's `--debug` (see
[Passthrough-IO flags](../reference/cli.md#passthrough-io-flags)).

## If your suite already uses pytest-xdist

rstest neutralizes xdist inside its workers automatically: an `addopts =
-n 4` in your ini will not spawn nested workers. Keep it while your pytest
job still runs (see [the staged rollout](#rolling-out-in-stages-and-rolling-back)),
then remove it and pass `-n` to rstest instead. A `--dist` mode in `addopts`
is not read by rstest either: set it as `[tool.rstest] dist` (see
[above](#addopts-and-pytest_addopts)). See
[Migrating from pytest-xdist](migrate-from-xdist.md).

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

## The migrate-check preflight

`rstest migrate-check` is not a test run: it is a parallel-readiness
report. It turns the manual triage of [checklist](#a-migration-checklist)
step 6 ("a few tests fail in parallel, read the guide, classify each by
hand") into one command.

It first collects the suite twice and flags test ids that differ between
collections (a per-process address or uuid is a hard blocker, since workers
can't agree on the test set; a timestamp may bail). If the ids are stable, it
runs the suite in parallel (at least two workers) and sorts every
parallel-only failure into one of six verdicts (order dependency, isolation leak, wall-clock sensitivity,
intrinsic flake, inconclusive, or not parallel-specific), names the fix for
each, and bisects the polluting file for order and isolation failures (the
first 3 such failures only; bisect the rest with
[`rstest bisect`](../reference/cli-commands.md#bisect-nodeid)). The
full classification is in the
[`migrate-check` reference](../reference/cli-commands.md#migrate-check).

It exits `1` if any blocking unstable id or parallelism-specific failure is
found (`2` if it couldn't judge, for example no usable interpreter), so a red
job tells you which one to fix. That makes it a **CI gate** that blocks new
parallel-unsafe tests while the suite migrates: no new co-location leak,
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

The gate is heavier than a normal run (it collects twice and reruns the
failing files under discriminators), so run it in its own job or on a
schedule rather than on every push if the suite is large. Once the suite
reports `ready`, drop the gate and just run `rstest`.
