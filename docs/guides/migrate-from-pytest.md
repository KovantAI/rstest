# Migrating from pytest

The short version: install rstest, run `rstest` where you ran `pytest`, and
check the list under [What changes](#what-changes) before you rely on it in
CI. This page is the long version: what is identical, what differs, how to
roll out in stages, and how to roll back.

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

**Your configuration.** `pyproject.toml [tool.pytest.ini_options]`,
`pytest.ini`, `setup.cfg`, `tox.ini` (including `addopts`, `testpaths`,
`python_files`, `markers`, `filterwarnings`) are read by the vendored core
exactly as pytest reads them. One catch: rstest's *own* flags are read only
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

| Flag | Also defined by | What rstest does with it |
|---|---|---|
| `-n`, `--dist` | pytest-xdist | runs its own worker pool; xdist stays inert |
| `--junitxml` | pytest core | writes one merged JUnit file itself, at every worker count |
| `--html` | pytest-html | writes rstest's own merged HTML report, at every worker count |
| `--timeout` | pytest-timeout | rstest's native per-test timeout |
| `--reruns`, `--only-rerun` | pytest-rerunfailures | rstest's native, crash-aware reruns |
| `--debug` | pytest core (debug log) | starts debugpy and waits for an editor to attach |

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

Move these to the rstest command line (or `[tool.rstest]` where a key
exists, such as `reruns`) when you switch.

## What changes

**Parallel by default.** This is the headline difference. pytest runs your
tests one at a time; rstest runs them on `auto` workers and says so in its
header line. Check each item against your suite:

- [ ] *Session/module-scoped fixtures instantiate once per worker*, not
  once per run, the same semantics as pytest-xdist. A session-scoped
  database or server fixture must tolerate N concurrent instances.
  (`rstest --doctor` flags session fixtures that ran more than once.)
- [ ] *`pytest_configure`, `pytest_sessionstart` and `pytest_sessionfinish`
  run in every worker*, concurrently. A conftest hook that creates a shared
  resource or writes a shared file must be idempotent or keyed on the worker
  id (`RSTEST_WORKER_ID` / `workerinput["workerid"]`).
- [ ] *Tests run in a different order*, interleaved across workers. Tests
  that depend on a previous test's side effects need [`--dist
  loadfile`](parallel-safety.md#file-affinity) or a fix.
- [ ] *Reordering in `pytest_collection_modifyitems` is ignored* at
  `-n ≥ 2` (deselection is honored). rstest schedules by duration instead.
  This includes plugins that reorder: pytest-django, for example, moves
  `TestCase` tests ahead of `TransactionTestCase` in that hook, so expect
  that ordering not to hold in the pool (inferred from the mechanism, not
  verified against a Django suite). Use `-n 0` or an affinity `--dist` mode
  where order matters.
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
the cheap probe: it surfaces exactly what a pytest upgrade would, with
your installed pytest untouched. Budget the runner switch as
"pytest upgrade first, then a one-line command change," not one step.
[Upgrading to pytest 9](upgrade-to-pytest9.md) is the concrete
checklist for that first step: the small set of 8→9 removals that actually
bite, with the grep and the fix for each.

## Rolling out in stages, and rolling back

rstest doesn't touch your pytest setup, so switching back is a one-line
change as long as you keep the pytest side intact during the rollout:

1. **Shadow.** Add an rstest job next to your existing pytest (or
   pytest-xdist) CI job. Keep the old job as the required check. Keep pytest
   installed: `rstest try` compares against it, and it's your fallback.
2. **Compare.** Run both for a while. `rstest try` and
   `rstest migrate-check` (below) tell you where results differ.
3. **Switch.** Make the rstest job required and the old job optional.
   Leave pytest-xdist and its `addopts` (`-n 4`, `--dist ...`) in place for
   now: rstest neutralizes them inside its workers, and the old job still
   needs them.
4. **Clean up** once you're confident: remove the old job, then the xdist
   flags from `addopts`, then pytest-xdist itself.

To roll back at any stage, point CI at `pytest` again. pytest ignores
`[tool.rstest]` in `pyproject.toml` and the `.rstest_cache/` directory, so
neither needs removing. If you already did step 4, restore the xdist flags
and dependency.

## The escape hatch

```console
$ rstest -n 0
```

One worker, one pytest session, pytest's exact per-test outcomes (only the
[flags rstest owns](#flags-rstest-owns) are still handled by rstest). If something
behaves differently under rstest's parallel mode, this is the first
diagnostic: if it also fails at `-n 0`, it's not parallelism.

Flags that need pytest's own terminal switch to this mode automatically:
`--co`, `-s` / `--capture=no`, `--pdb`, `--trace`.

## If your suite already uses pytest-xdist

rstest neutralizes xdist inside its workers automatically: an `addopts =
-n 4` in your ini will not spawn nested workers. Keep it while your pytest
job still runs (see [the staged rollout](#rolling-out-in-stages-and-rolling-back)),
then remove it and pass `-n` to rstest instead. See
[Migrating from pytest-xdist](migrate-from-xdist.md).

## Just want to know if it's worth it?

```console
$ rstest try
```

runs your suite under plain pytest and under `rstest -n auto` and tells you, in
one command, whether the results are identical and how much faster rstest is,
before you commit to anything. It costs one serial pytest run plus one rstest
run. If it flags differences, it
points you at `migrate-check` (below).

## A migration checklist

0. `rstest migrate-check`: **the preflight that does the triage for you.**
   It is the front door of the migration: run it first, fix what it names,
   and steps 2–3 below usually become a formality. See
   [The migrate-check preflight](#the-migrate-check-preflight) just below for
   what it reports.
1. `rstest -n 0`: confirm identical results to pytest (this is the
   contract; report a bug if not). Move any rstest-owned flags out of
   `addopts` first ([why](#addopts-and-pytest_addopts)).
2. `rstest`: run parallel. Green? You're done.
3. A few tests fail only in parallel? `migrate-check` already classified
   each one and named its fix; [Parallel safety](parallel-safety.md) is the
   reference for the remedies (`@pytest.mark.serial`, `--dist loadfile`, or
   fixing the shared state).
4. Run `rstest --doctor` once. It usually pays for the migration by
   itself.

## Driving it with Claude (the migrate-to-rstest skill)

rstest ships a Claude Code skill that runs this whole checklist for you:
`.claude/skills/migrate-to-rstest/`. Open the rstest repo (or copy that
directory into your own project's `.claude/skills/`) and ask Claude to
"migrate my suite to rstest" or "parallelize my tests". It has two lanes:
**readiness** drives `migrate-check`, applies the right fix per verdict, and
wires up `[tool.rstest]` config + a CI gate; **speed** drives `--doctor` to find
the slowest tests, wait-bound (sleep/timeout) tests, the parallel-floor gate
test, and expensive fixtures, with the action for each. It asks before editing
your tests or CI.

## The migrate-check preflight

`rstest migrate-check` is not a test run: it is a parallel-readiness
report. It turns the manual triage of step 3 ("a few tests fail in parallel,
read the guide, classify each by hand") into one command. It works in two
stages, stopping as early as it can:

**1. Collection stability.** It collects the suite **twice** and diffs the id
sets. Any test whose id appears in only one collection has a *run-to-run
unstable* id, classified by why:

- **address / uuid**: the id embeds a per-process value (a `repr()`-fallback
  `0x…` address, or a uuid). Every worker collects a different id, so the
  workers can't agree on the test set and rstest is forced to `-n 0`. Reported
  as **WILL bail**, a hard blocker. Fix: give the `parametrize` a stable
  `ids=`.
- **time**: a timestamp/date in the id. Usually stable enough *within* one
  run (workers collect near-simultaneously), so it typically runs fine at
  `-n auto`. Reported as **may bail**.

If a WILL-bail id is found it stops here: fix the ids first, since nothing
runs in parallel until they're stable.

**2. Parallel classification.** Otherwise it runs the suite at `-n auto` and
classifies every test that fails *only* under parallelism. The discriminator
reruns (`-n 0` twice, `--dist loadfile`) are **scoped to the files containing
failures**, so a clean suite runs zero of them and cost scales with the number
of failing files, not suite size. Each failure lands in one class:

| Class | Meaning | Fix it names |
|---|---|---|
| **NOT PARALLEL-SPECIFIC** | also fails at `-n 0` | a pre-existing bug / env gap, not a migration concern |
| **INTRINSIC FLAKE** | serial repeats disagree | flaky under any runner; fix the test |
| **INCONCLUSIVE** | missing from a follow-up run, so no evidence either way | make the nodeid stable across collections |
| **ORDER DEPENDENCY** | passes serial + `loadfile`, fails under `load` | `--dist loadfile`, or fix the in-file coupling |
| **WALL-CLOCK / LOAD-SENSITIVE** | passes serial, fails parallel, wait-bound (wall ≫ cpu) | mock the clock / drop the tight deadline; stopgap `-n 4` |
| **ISOLATION / CO-LOCATION** | passes serial, fails under `load` **and** `loadfile` | reset the leaked global state; stopgap `@pytest.mark.serial` |

For ORDER-DEPENDENCY and ISOLATION findings it then **bisects the polluter**
(binary-searching for the file whose tests, run serially before the victim,
reproduce the failure) and reports `POLLUTED BY: <file>`, `SAME-FILE
co-location`, or that no serial ordering reproduces (a likely concurrent-
resource race rather than state pollution).

It exits non-zero if any WILL-bail id or parallelism-specific failure is
found, so it doubles as a **CI gate** that blocks new parallel-unsafe tests:

```console
$ rstest migrate-check --migrate-check-json migrate.json \
         --migrate-allow tests/legacy/    # tolerate a triaged backlog
```

`--migrate-check-json` writes the findings as a versioned JSON document
([schema reference](../reference/report-json.md#migrate-check-json)) for
tooling and trending. `--migrate-allow <substr>` accepts known findings by
nodeid/site substring, they're still reported (marked `(allowed)`) but don't
fail the build, so the gate goes red only on **new** issues while you work
through the backlog. Full flag reference:
[`migrate-check`](../reference/cli-commands.md#migrate-check).
