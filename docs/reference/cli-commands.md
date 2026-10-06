# CLI subcommands

These commands don't run your suite as a normal test run. Each is given as
the first argument (`rstest try`); a path literally named after one is
disambiguated with `rstest ./try` or `rstest -- try`. rstest's own options
and value-less pytest switches may also come before it (`rstest -q try`,
`rstest --python 3.12 migrate-check`). A subcommand name after a test path
or a pytest option's value (`rstest -k foo try`) is a usage error that says
to put the subcommand first, unless a file or directory of that name exists. `try`,
`migrate-check`, `audit`, `bisect` and `replay` (and `xdist-removal-check`
with `--xdist-trial`) do run pytest sessions, but as their
own analysis, not as a normal test run. The flags that only apply to a
subcommand are documented with it; everything else is on
[CLI flags](cli.md).

After the subcommand, only its own flags and the global ones are accepted.
The global flags are `--python`, `--cache-remote`, `--cache-compact-threshold`,
`--migrate-check-json`, `--migrate-allow`, `--audit-json`, `--audit-repeat`
and `--bisect-json`. Any other rstest flag there is a usage error (exit 2):
`rstest try --doctor` prints `unexpected argument '--doctor' found`. Put
rstest flags before the subcommand and it is no longer recognized: `rstest
-n 2 try` is a normal test run with `try` as a test path.

```text
rstest <COMMAND> [OPTIONS]
```

After a subcommand, rstest accepts only that subcommand's own flags plus a
small shared set: `--python`, `--cache-remote`, `--cache-compact-threshold`
and the subcommands' JSON and tuning flags (`--migrate-check-json`,
`--migrate-allow`, `--xdist-removal-json`, `--xdist-trial`, `--audit-json`,
`--audit-repeat`, `--bisect-json`). Any other run flag there, such as `-n` or
`--cache-push`, is a usage error (exit `2`). `rstest <COMMAND> --help` lists
what each one accepts.

- **Adoption and parallel safety:** [`try`](#try), [`migrate-check`](#migrate-check), [`xdist-removal-check`](#xdist-removal-check), [`audit`](#audit), [`bisect <nodeid>`](#bisect-nodeid), [`replay`](#replay)
- **CI and the shared cache:** [`shard-verify`](#shard-verify), [`cache-compact`](#cache-compact)
- **Inspection and integrity:** [`explain`](#explain), [`verify-vendor`](#verify-vendor)
- **Agent skills:** [`install-skills`](#install-skills)

## Adoption and parallel safety

### `try`

The zero-config "should I switch?" proof. Runs your suite once under plain
`pytest` and once under `rstest -n auto`, then prints the only two things that
matter: whether the outcomes are **identical** (your real pytest against
`rstest -n auto`, so a difference is a parallel-safety issue or a parity gap) and how much **faster** rstest is, with a
rough CI-time saving. No flags, no config.

```console
$ rstest try
================= rstest try =================
  ✓ parity:  8337 tests — identical outcomes to pytest
  ⚡ speed:   pytest 1m36s  →  rstest 21.0s   (4.6× at -n auto = -n 8)
  💸 saves   1m15s per run
================================================
  → drop-in ready: `rstest` is `pytest`, in parallel. Switch with confidence.
```

The speed line names the worker count `-n auto` resolved to for this suite
(`-n 1` when it ran single-worker). The `saves` line appears only when rstest
saved at least one second. In a git checkout it also projects the saving over
the last 30 days, counting each commit in that window as one CI run.

Parity needs something to compare. If either run hit a collection error, or
ran no tests at all (an empty directory, everything deselected), `try` gives
no verdict:

```console
================= rstest try =================
  ✗ could not compare: pytest hit 1 collection error (tests/test_bad.py)
================================================
```

Exit 0 when outcomes are identical, 1 when they differ (it then points you at
`migrate-check` to classify the differences, usually an unstable parametrize
id or a parallel-only failure), 2 when there was nothing to compare
(collection errors or no tests), it couldn't run pytest, rstest refused
to dispatch, or rstest hit an error (no usable interpreter, a failed spawn;
an `Error:` line on stderr says which). A pre-existing red pytest run is reported as such, not blamed on
rstest.

`try` is the one command that needs **pytest installed on its own**. It runs
the baseline as `python -m pytest` with the same interpreter rstest uses (the
project's `.venv`, or whatever [`--python`](cli.md#-python-path-or-version)
selects), so pytest must be importable there. A `pytest` on `PATH` from pipx
or another environment doesn't count. If that baseline can't run (pytest
missing, or the suite doesn't collect), `try` exits 2; check with
`python -m pytest -q` in the same environment. rstest itself vendors its core,
so `migrate-check` and normal runs have no such requirement.

The baseline uses the pytest version installed in that environment. If it is
older than the vendored pytest 9.1.1 (for example a `pytest<9` pin), a
difference `try` reports may come from the pytest version rather than from
rstest; see [Upgrading to pytest 9](../guides/upgrade-to-pytest9.md).

### `migrate-check`

Parallel-readiness preflight, not a test run. It works in two stages and
stops as early as it can.

**1. Collection stability.** It collects the suite **twice** and diffs the id
sets; ids present in only one collection are run-to-run unstable. It reports
each offending parametrize site, classified by why its id is unstable:

- **address / uuid**: a per-process value (a `repr()`-fallback id embedding
  `0x…`, or a uuid). These differ in *every* worker, so per-worker
  collections disagree and rstest refuses to dispatch (`workers collected
  different test sets ...`): nothing runs in parallel until the ids are fixed
  or you choose `-n 0`. Reported as **WILL bail**, a hard blocker.
- **time**: a timestamp/date in the id. A coarse one (seconds or dates) is
  usually stable enough *within* one run, since all workers collect
  near-simultaneously, so it typically runs at `-n auto`; but a run whose
  collection crosses a second boundary hits the same refusal, so it fails
  intermittently, and a sub-second timestamp differs every time. Reported as
  **may bail**.

The fix for both is a stable `ids=` on the `parametrize`.

It also compares the two collections **in order**. A site whose ids are the
same but come back in a different order (a `parametrize` over a `set`, whose
iteration order for strings changes with the per-process `PYTHONHASHSEED`)
is reported under **UNSTABLE ORDER** as **WILL bail**: every worker must
collect the identical ordered list. In the JSON it is an `unstable_ids` entry
with the `order` kind. Fix it with a list or `sorted(...)`; the stopgap is one
fixed `PYTHONHASHSEED` for the whole run.

If a WILL-bail id or order is found, it stops here: nothing runs in parallel
until collection is stable.

**2. Parallel classification.** Otherwise it **runs the suite in parallel**
and classifies every test that fails only under parallelism. The parallel
pass uses one worker per test file, up to your logical cores and never fewer
than two. It does not use `-n auto`: auto's duration-cache cap would put a
small suite, or any suite after one run, on a single worker, where nothing
runs concurrently. The discriminator reruns (`-n 0` twice and `--dist
loadfile`) are **scoped to the files containing failures**, so cost scales
with the number of failing files, not the suite size; a clean suite runs no
discriminators at all. One exception: with only one or two failing files the
`--dist loadfile` run covers the whole selection, because each worker is
handed two files to start with, so one or two files would run one after the
other on a single worker and a cross-file race would never show. A selection
of fewer than three files can't run two files at once under loadfile at all;
the check then prints a note that an ORDER DEPENDENCY verdict may also be two
tests of the same file racing. Each failure lands in one class:

| Class | Meaning | Fix it names |
|---|---|---|
| **NOT PARALLEL-SPECIFIC** | also fails at `-n 0` | a pre-existing bug or env gap; summarized, not a migration concern |
| **INTRINSIC FLAKE** | serial repeats disagree | flaky under any runner; fix the flake (`--reruns` only hides it) |
| **INCONCLUSIVE** | missing from a follow-up run (for example an unstable parametrize id), so there is no evidence either way; not counted as a pass | make the nodeid stable across collections |
| **ORDER DEPENDENCY** | passes serial and under `--dist loadfile`, fails under `load` | run with `--dist loadfile`, or fix the in-file coupling |
| **WALL-CLOCK / LOAD-SENSITIVE** | passes serial, fails parallel, and is wait-bound (wall ≫ cpu): a real-time deadline that misses under oversubscription | mock the clock / drop the tight upper bound; stopgap `-n 4` or `@pytest.mark.serial` |
| **ISOLATION / CO-LOCATION** | passes serial, fails under both `load` and `loadfile`, and is *not* wait-bound: a leaked-global-state defect | reset the leaked state per test; stopgap `@pytest.mark.serial` |

For ORDER-DEPENDENCY and ISOLATION findings it then **bisects the polluter**
(capped at 3 victims): it binary-searches for the file whose tests, run
serially before the victim, reproduce the failure, and reports `POLLUTED BY:
<file>` (cross-file), `SAME-FILE co-location (inspect <file>)`, or (when no
serial ordering reproduces) that the failure is likely a concurrent-resource
race rather than state pollution.

Each finding prints the upstream fix and the rstest stopgap. Exits `1` if
any WILL-bail id or parallelism-specific failure is found, and `2` when it
couldn't judge: the `-n auto` pass produced no outcomes (`parallel` is
`{"ran": false}` in the JSON), or rstest hit an error (an `Error:` line on
stderr). `0` means ready. Usable as a CI gate (see `--migrate-check-json` and `--migrate-allow` below for the
machine-readable form and the known-issue allow-list).

### `--migrate-check-json <path>`

Write the migrate-check findings as a single versioned JSON document (schema
`1`): the machine-readable surface for CI gating and trending. Only read by
the `migrate-check` subcommand (`rstest migrate-check --migrate-check-json
out.json`), which still prints its human report; on a normal run the flag
does nothing. The
document carries the unstable-id sites and the classified parallel findings,
each with its verdict, fix, allow-list status, and bisected polluter:
`{meta, ready, tests_collected, will_bail_count, unstable_ids[], parallel{…}}`.
Field reference: [Migrate-check JSON](report-json.md#migrate-check-json).

### `--migrate-allow <SUBSTRING>`

Accept a known finding so it does not fail the exit code (repeatable). Any
finding whose nodeid or unstable-id site **contains** SUBSTRING is still
reported (marked `(allowed)` in the human output and `"allowed": true` in the
JSON) but excluded from the non-zero gate. This lets CI gate on **new**
parallel-unsafe tests while tolerating a triaged backlog: allow-list today's
findings, and the build only goes red when a fresh one appears.

### `xdist-removal-check`

Readiness check for the last migration step: uninstalling pytest-xdist.
rstest never needs the package, but removing it breaks or changes things that
worked while it was installed. This command finds them before you uninstall,
without running your suite. It reads:

- the pytest config file (`addopts`, `required_plugins`, and xdist's ini keys)
  and `PYTEST_ADDOPTS`;
- every `.py` file under the rootdir (skipping virtualenvs, caches and hidden
  directories);
- the source of every installed pytest plugin (its `pytest11` entry point).

Each finding prints where it is, what happens once pytest-xdist is gone, and
the fix:

| Finding | Blocks? | What happens without pytest-xdist | Fix it names |
|---|---|---|---|
| `-n` / `--numprocesses` in `addopts` | yes | usage error (exit 4); rstest never read it anyway | `rstest -n N` or `[tool.rstest] numprocesses` |
| `--dist` in `addopts` | yes | usage error (exit 4); rstest never read it, so `loadgroup` / `loadscope` / `loadfile` grouping is already lost | `[tool.rstest] dist = "..."` |
| xdist-only flags in `addopts` (`--tx`, `--rsyncdir`, `-d`, `--maxprocesses`, `--looponfail`, `-p xdist`, ...) | yes | usage error (exit 4) | drop the flag (`--looponfail`: use `--watch`) |
| `pytest-xdist` in `required_plugins` | yes | pytest refuses to start | remove it |
| unguarded `import xdist` / `from xdist... import` | yes | `ImportError` | the native `worker_id` / `testrun_uid` fixtures, `config.workerinput`, or `PYTEST_XDIST_WORKER` |
| an xdist hook (`pytest_configure_node`, `pytest_testnode*`, `pytest_xdist_*`) in a conftest or plugin module, not marked `optionalhook=True` | yes (no for a hook class the file never registers) | `PluginValidationError: unknown hook` | `@pytest.hookimpl(optionalhook=True)` |
| `hasplugin("xdist")` in your code | no | the branch flips | gate on `config.workerinput` or `PYTEST_XDIST_WORKER` |
| unguarded module-level `import xdist` in an installed plugin | yes in the entry-point module and its packages' `__init__.py`, no elsewhere | `ImportError` at startup (entry module) or when that module is imported | upgrade or drop the plugin, or keep pytest-xdist |
| `hasplugin("xdist")` in an installed plugin | no | the plugin takes its non-xdist path | check that plugin's behavior |
| `rsyncdirs` / `rsyncignore` / `looponfailroots` | no | a `PytestConfigWarning` (an error under `--strict-config`) | remove the key |

An import inside a `try:` block, an `if TYPE_CHECKING:` block, or an
`if config.pluginmanager.hasplugin("xdist"):` block is treated as guarded
and not reported. The `if` counts only when its body can't run without the
guard: `and` chains count, while `or`, `not` and comparisons (other than
`getplugin("xdist") is not None`) don't. An xdist
hook under such a gate, or in a class the same file registers only under
one, isn't reported either. Hook names the project declares itself (through
`pytest_addhooks` and `@pytest.hookspec`) don't need pytest-xdist, and
neither do hook-named functions in test modules, which pytest never
registers as plugins, so neither is reported. Exits `1` on any blocking
finding that isn't allow-listed with
[`--migrate-allow`](#-migrate-allow-substring), which here matches against the finding's location (`pytest.ini addopts`,
`tests/conftest.py:12`, a plugin's distribution name), and `0` when ready. An
error inside the check (no usable interpreter, an unwritable
`--xdist-removal-json` path) exits `2`, as for the other verdict commands (see
[Exit codes](exit-codes.md#gating-flags-and-their-exit-codes)).

```console
$ rstest xdist-removal-check
$ rstest xdist-removal-check --xdist-trial tests/
```

Session args (paths, `-k`, `-m`, `-p`) are forwarded to the trial runs only.
The static scan always covers the whole project.

### `--xdist-trial`

Also run the suite with pytest-xdist hidden from the plugin manager
(`-p no:xdist`), which drops its options and hook specs as uninstalling it
would. The `xdist` module stays importable, so `import xdist` sites are
caught only by the static scan. When pytest-xdist is installed it first runs the suite with it
loaded, then names every test that passes with it and fails (or is no longer
collected) without it. If the session with xdist hidden never starts (a usage
error, a plugin validation error), it prints pytest's error. A regression or a
session that doesn't start fails the gate. Costs one or two full runs; without
this flag the command runs nothing.

### `--xdist-removal-json <path>`

Write the findings as a versioned JSON document (schema `1`) for CI gating:
`{meta, ready, xdist_version, findings[], trial}`, each finding carrying its
`kind`, `location`, `text`, `why`, `fix`, `blocking` and `allowed`. Only read
by `xdist-removal-check`. Missing parent directories are created. A run that
errors (exit `2`) deletes a report left at this path by an earlier run, so a
stale `"ready": true` is never read as current. Field reference:
[Xdist-removal-check](output-schemas.md#xdist-removal-check).

### `audit`

Auto parallel-safety audit: the one-command answer to "which of my tests
aren't parallel-safe, and how do I fix them?" It **runs the suite in parallel**
(one worker per test file, up to your logical cores and never fewer than two,
as `migrate-check` does; repeat with [`--audit-repeat`](#-audit-repeat-n), since a parallel flake is
probabilistic), then diffs against the `-n 0` oracle and classifies every test
that fails **only** under parallelism, reusing `migrate-check`'s discriminators
(`-n 0` at least twice + `--dist loadfile`, repeated with `--audit-repeat` and scoped to the failing files) and verdicts
(ISOLATION / WALL-CLOCK / ORDER-DEPENDENCY / INTRINSIC FLAKE / pre-existing).

Where `migrate-check` is the onboarding preflight (unstable ids first, verbose
per-verdict classification), `audit` is the focused fix-loop: it prints the
serial-fixable failures and a **ready-to-paste `conftest.py` block** that marks
exactly those nodeids `@pytest.mark.serial` (they then run last, alone, after
the parallel phase). One paste, no per-test edits:

```python
import pytest

_RSTEST_SERIAL = {
    "tests/test_a.py::test_x",
    "tests/test_b.py::test_z",
}


def pytest_collection_modifyitems(items):
    for item in items:
        if item.nodeid in _RSTEST_SERIAL:
            item.add_marker(pytest.mark.serial)
```

If your `conftest.py` already defines `pytest_collection_modifyitems`, paste
only `_RSTEST_SERIAL` and add the loop to your existing hook. A second
definition of the same name replaces the first, so your original hook would
silently stop running.

Only ISOLATION and WALL-CLOCK failures go in the block. Serial is a **stopgap**
for those; the report also names the real fix (reset leaked state, mock the
clock). ORDER-DEPENDENCY failures are listed separately with a `--dist loadfile`
recommendation instead: they depend on tests that run before them in the same
file, and the serial phase would run them apart from those tests, so marking
them serial wouldn't make them pass. Intrinsic flakes (serial repeats disagree)
and pre-existing `-n 0` failures are also reported separately; serial won't fix
those. A test that fails in the parallel pass but is missing from a
follow-up run (for example an unstable parametrize id) is reported as
**inconclusive** rather than guessed at. Exits non-zero on any parallel-only
failure (serial-fixable, order-dependent, intrinsic or inconclusive), so it
gates CI; pre-existing failures don't fail the audit. A selection that matches
no tests (for example a `-m` with no matching tests) exits `0` with a "no tests
were selected" note; exit `2` is kept for a run rstest refused to dispatch or
an error inside the audit (an `Error:` line on stderr). [`--audit-json`](#-audit-json-path) writes the findings, the serial set,
and the conftest block for tooling.

### `--audit-json <path>`

Write the `audit` findings as a versioned JSON document (schema `1`):
`{meta, ran, parallel_safe, tests, serial_candidates[], serial_conftest,
order_dependent[], intrinsic_flakes[], inconclusive[], preexisting_failures}`. `serial_candidates[]`
carries each `{nodeid, verdict, fix}`; `serial_conftest` is the paste-able block
as a string. The file is written as `{meta, ran: false, parallel_safe: false}`
before the audit starts and replaced with the full result at the end, so an
audit that stops early (the parallel pass produced no run, exit `2`, or a child
session failed) leaves `ran: false` rather than a stale result from an earlier
run. `-x`/`--maxfail` from your args or `addopts` is lifted for every run the
audit makes, so the whole suite is checked.
Field reference: [Audit JSON](output-schemas.md#audit).
Only read by the `audit` subcommand (`rstest audit --audit-json out.json`); on
its own it is ignored and no file is written.

### `--audit-repeat <N>`

How many times `audit` reruns the parallel pass (default `1`; `0` is treated
as `1`). A parallel-only
failure is probabilistic (a race may not fire every run), so a test that fails
in **any** repeat is treated as a candidate. Raise it (e.g. `--audit-repeat 5`)
to shake out intermittent races. The discriminators repeat the same number of
times (the `-n 0` oracle at least twice, `--dist loadfile` at least once), so
an intermittent failure gets as many chances to show up in them as it had in
the parallel pass. That makes a misclassification less likely but does not rule
it out: a test that is flaky in every mode can still pass all serial runs by
chance and be listed as a serial candidate.

### `bisect <nodeid>`

Order-dependency bisect: the automated answer to "this test only fails when
run after some other test; *which* one?" Given a failing test's nodeid, it
finds the **polluter**: the earlier test(s) whose leaked state makes the target
fail.

It works entirely at `-n 0` (serial), so it isolates **ordering**, not
concurrency (for parallel-only failures use
[`migrate-check`](#migrate-check)). The steps:

1. **Isolation check.** Runs the victim alone. If it fails by itself, that's a
   plain bug, not an order dependency; reported and done.
2. **Reproduce.** Runs the victim after *all* preceding tests (collection
   order). If it passes there, it runs the victim after *every* other test,
   including the ones that collect after it: on CI a polluter can run first
   on another worker's schedule. If it still passes, the failure doesn't come
   from ordering (likely parallel-only, so try `migrate-check`).
3. **Delta-debug.** [`ddmin`](https://www.st.cs.uni-saarland.de/dd/) over the
   set that reproduced: repeatedly run the victim preceded by a subset of it,
   shrinking toward the **1-minimal** set that still reproduces.
   Handles a single polluter *and* interacting pairs.

It prints the culprit(s) and a **minimal reproducing command**
(`rstest -n 0 <culprit…> <victim>`) you can paste to confirm and debug. It
runs from where you ran bisect: nodeids are shell-quoted and written relative to
the current directory, and your `--python` and pytest options are carried
over as given.

You can run bisect from any directory. The rootdir comes from pytest itself
(so `--rootdir`, `-c`, and every config file pytest honors apply), and the
predecessor set is the whole suite as a run from the rootdir would collect it
(its `testpaths`), not only the subdirectory you're in. The nodeid you pass
may be rootdir-relative or relative to the current directory; when both
readings name a test, the one relative to the current directory wins, as it
would for pytest. Every child run
uses the same interpreter, rootdir and config file as the collection, so a
nested config (say `pkg/pytest.ini`) can't re-root a run that only touches
`pkg/`. When the printed command would re-root the same way (a nested config,
or a nearer `setup.py` in a project with no config file), it carries those
pins too: `--rootdir` plus the loaded config, or `-c /dev/null` and
`--confcutdir` when there is none.

Order is the whole point, so bisect disables pytest-randomly (`-p no:randomly`)
in the collection, every child run and the printed command. The candidate
set is the suite's plain collection order: the preceding tests first, then
the whole suite with the victim moved last. To bisect a failure that only
appears in one shuffled order, reorder explicitly instead.

Its runs also use a private pytest cache, new and empty for each run, so `--ff`
and `--lf` (often set in `addopts`) have nothing to reorder by and your own
`.pytest_cache` is left alone (with the cacheprovider disabled the pin is
simply inert); the printed command brings a fresh cache of its
own when those flags are active. `-x` and `--maxfail` (from `addopts` or after
`--`) are lifted in the child runs and in the printed command, so an earlier
failure, the culprit's own included, can't stop a run before the victim.
`--nf` and `--sw` can't be switched off from the command line, so bisect
refuses them (exit `2`) and says how to drop them for the bisect. rstest's own
`[tool.rstest] reruns` config is off in the child runs too: a passing rerun
would hide the failure, and a rerun run goes through the pool, which orders
tests by duration. A `--confcutdir` of your own is kept as pytest applied it.

Bounded to ~80 child runs; if it hits that ceiling it stops and reports the
smallest reproducing set found (may not be fully minimal). Large suites are
fine: the selection reaches each child run through a file, not the command
line.

Pass pytest options after `--`; they apply to the collection and to every
child run:

```console
$ rstest bisect tests/test_report.py::test_totals
$ rstest bisect tests/test_report.py::test_totals -- -p no:randomly -o log_level=DEBUG
```

Only options go there. A test path or nodeid after `--` is rejected (exit `2`),
since it would be added to every child's selection. pytest decides what counts
as one, so option values are never mistaken for paths. The same goes for test
paths in the ini `addopts` or `PYTEST_ADDOPTS`: bisect says so and exits `2`.
Clear them for the bisect with `-- -o addopts="<options only>"` (or unset the
variable).

Exit code: `0` = order-dependent culprit found, `1` = not order-dependent
(fails alone, or doesn't reproduce from order), `2` = the nodeid isn't in the
suite, a test selection was passed after `--`, or rstest hit an error (an
`Error:` line on stderr; `--bisect-json` records it in `error`). If the victim doesn't run in
a child session (deselected by an option, a collection error), bisect stops
with an error instead of reading that as a pass. `--bisect-json` writes the
result.

### `--bisect-json <path>`

Write the `bisect` result as a versioned JSON document (schema `1`):
`{meta, nodeid, rootdir, cwd, order_dependent, culprits[], reproduce_command}`.
Nodeids are relative to `rootdir`; `reproduce_command` runs from `cwd` and is
null when the test isn't order-dependent. A run that ends without a verdict
(exit `2`, or an error) still writes the document, with an `error` message,
no culprits, and no `rootdir` or `cwd`, so a stale result from an earlier run is never left behind.
Used with the `bisect` subcommand. Field reference:
[Bisect JSON](output-schemas.md#bisect).

### `replay`

Re-run a recorded parallel schedule, so a parallel-only failure reproduces on
demand. rstest owns dispatch (which worker runs which test, in what order), so
unlike pytest/xdist it can record that schedule and pin it back. Every parallel
run (`-n >= 2`, except `--dist each` and `--shard`) journals
its exact per-worker assignment and order to
`.rstest_cache/replay/`: one file per run (`<run-uid>.json`, last 10 kept) plus a
stable `latest.json`. Journaling is on by default and costs almost nothing (it is
the assignment rstest already tracks); disable it with
`RSTEST_NO_REPLAY_JOURNAL=1` (`0`, `false` or empty leave it on). Replay itself
writes no journal and leaves the duration and flake-history caches untouched:
it is a diagnostic re-run, so repeating it while debugging doesn't pile
failures onto one test or skew the scheduling baseline.

```console
$ rstest replay                     # replay the most recent local run
$ rstest replay <run-uid>           # replay a specific journaled run
$ rstest replay --journal ci.json   # replay a downloaded CI journal
```

The primary flow is CI to local. The failing CI run journals without foresight,
CI uploads `.rstest_cache/replay/latest.json` as an artifact, and a developer
replays it locally:

```yaml
# CI: keep the schedule of a failed run
- run: rstest -n auto
- uses: actions/upload-artifact@v7
  if: failure()
  with:
    name: rstest-replay-${{ github.job }}-${{ strategy.job-index }}
    path: .rstest_cache/replay/latest.json
    if-no-files-found: ignore
```

```console
$ rstest replay --journal ./rstest-replay-tests-0/latest.json
```

For the full CI-to-local walkthrough (download, portability rules, what to
read in the output), see
[Replaying a CI-only failure locally](../guides/ci-quickstart.md#replaying-a-ci-only-failure-locally).

The journal keys on nodeid, not on the machine-local collection index, so it
survives the machine hop. Replay collects the suite fresh, re-resolves each
recorded nodeid to this run's index, forces `-n` to the recorded worker count,
and pins each worker to exactly its recorded nodeids in the recorded order, with
work-stealing and reruns off, `@pytest.mark.flaky` budgets and
`[tool.rstest] reruns` included (the recorded shuffle is already baked into the
pinned order). `@pytest.mark.serial` tests are held until every other worker
has finished, as in the recording's serial phase. Recorded cache-selection
flags (`--lf`, `--sw` and their variants) are dropped with a warning, since
they would filter by the local pytest cache; reorder-only `--ff`/`--nf` stay. A worker that crashes
mid-replay is respawned and runs only the rest of its list. A `--reruns` retry
in the recording is journaled once, as its first attempt. Absolute test paths
under the recording's working directory are stored relative to it. Replay
writes no new journal.

Determinism is per-worker: worker-local order and assignment are reproduced
exactly, which is what state-ordering flakes depend on. The exact cross-worker
interleaving stays timing-dependent, so a genuinely time-dependent race is
best-effort. If the suite changed since the journal was written, replay prints a
drift note, runs the tests that still match, and reports how many recorded tests
no longer collect. Session args (paths, `-k`, `-m`, plugins) come from the
journal, not from the `replay` invocation, and are printed (`rstest: replay:
args: ...`) before the run starts. They go to pytest as they are, so replay
only journals from runs you trust (see
[Security: replay journals](security.md#replay-journals)). To pick the
interpreter, put `--python` after the subcommand
(`rstest replay --journal ci.json --python .venv/bin/python`); before it,
`replay` is read as a test path.

Exit code: the replayed run's own code (`0` all passed, `1` a test failed,
and so on). A missing or unreadable journal exits `1` with an `Error:` line
before any test runs.

## CI and the shared cache

### `shard-verify`

Prove a `--shard` matrix covered the whole suite. Sharding partitions the suite
independently in each job with no coordination, so a divergent duration cache or
a differently-collected suite can silently drop or double-run tests and still
exit 0. `shard-verify` reconciles the per-shard reports after the fact.

Each shard run writes a report while `--shard` is active:

```console
$ rstest -n 4 --shard "$K/$N" --report-json "shard.$K.json"
```

(An explicit `-n` rather than `auto`: `--shard` needs at least two workers,
and `auto` can resolve to one on a small or warm-cached suite, which makes
the shard run exit 1.)

That report carries a `meta.shard` stamp: `k`, `n`, and the sha256
`collection_hash` and size of the full collected suite. In a final job that has
gathered all the shard reports, reconcile them:

```console
$ rstest shard-verify shard.*.json
ok shard-verify: 4 shards cover all 4200 collected tests (no drops, no overlap)
```

It exits `0` only when the shards agree on one collection (same
`collection_hash`, `n`, and size), the shard set is exactly `1..=N` once each,
and the union of what they ran equals the collection with no test on two shards.
It exits `1` on any drop, overlap, missing or duplicate shard, or a divergent
collection, printing a line that names the problem. It reads only the JSON
files, needs no interpreter, and runs no tests. Full-collection runs only: a
`--collect lazy` shard run stamps no collection hash and cannot be verified.
See [Verify no test was dropped](../guides/sharding.md#verify-no-test-was-dropped).

### `cache-compact`

Maintenance: fold remote segments into a fresh `base.json` and prune them, then
exit without running tests. Keeps the segment count (and pull size) down;
optional: pull/push work without it. Run occasionally (nightly, or on merge to
main). Needs `--cache-remote`. It is **run-less**: it exits before the run, so
`--cache-pull`/`--cache-push` aren't accepted after it: like any run flag
there, they are a usage error (`unexpected argument`, exit `2`), so they are
never silently skipped.

With no retention flags it folds **all** segments. To keep a recent window loose
(so the newest history stays merge-on-read while the tail is compacted):

- `--keep-last N`: retain the newest N segments; fold only older ones. Env:
  `RSTEST_CACHE_KEEP_LAST`.
- `--max-age DURATION`: retain segments younger than DURATION (a bare number is
  seconds, or a `s`/`m`/`h`/`d`/`w` suffix, e.g. `30d`); fold older ones. Env:
  `RSTEST_CACHE_MAX_AGE`.

Both flags belong to the subcommand and must come **after** it:
`rstest cache-compact --cache-remote s3://ci-cache/rstest --keep-last 200`.
rstest reserves both names in every position, so anywhere else they are a
usage error (`unexpected argument '--keep-last' found`, exit `2`): before the
subcommand (`rstest --keep-last 200 cache-compact ...`) and on a normal run
(`rstest --keep-last 200 tests/`) alike. On a normal run, the inline
[`--cache-compact-threshold`](cli.md#-cache-compact-threshold-n) compaction
reads the retention window from the env vars instead.

A segment retained by **either** rule stays loose. A bad flag/env value is a hard
error, never a silent fold-all.

## Inspection and integrity

### `explain`

Print one test's dossier from the caches without running anything. rstest
accretes rich per-test data across runs (the duration cache, the flake/fail log,
the last-green outcome set, the coverage index), but every other surface renders
it suite-wide. `explain` answers "tell me everything about this test" by merging
those caches for a single nodeid.

```console
$ rstest explain "tests/test_api.py::test_login"
test: tests/test_api.py::test_login

  duration   1.8241s (last recorded)
  outcome    passed last incremental run
  flakes     flaked 3x, failed 1x (last 2d ago)
  coverage   covers 2 file(s), 47 line(s):
               src/api/auth.py
               src/api/session.py
```

Add `--json` for a schema-stamped object on stdout (`{meta, nodeid, found,
duration_seconds, last_outcome, source_line, flakes, coverage}`), suitable for an
editor or CI step. Absent fields are `null`: a never-flaked test has no `flakes`,
a cold coverage index yields `null` coverage. Field reference:
[Explain JSON](output-schemas.md#explain).

It reads only cache files, needs no interpreter, and runs no tests. The data
comes from `.rstest_cache/`: `durations.json` (last recorded call time),
`flakes.json` (flake/fail counts and last-event age), `incremental_outcomes.json`
(last-green outcome and source line), and `coverage_index.json` (the coverage
footprint, populated by a prior `--cov-context=test` run). Fields whose cache is
cold are shown as unavailable rather than omitted. In human mode an unknown
nodeid exits `1` and prints substring suggestions; with `--json` it exits `0`
with `"found": false` so tooling can probe nodeids cheaply.

Note the local caches keep only the *latest* duration per test, not a history
series, so variance and an ordered last-N-outcomes list are not reported yet;
`explain` grows richer as more per-test data is persisted.

### `verify-vendor`

Prove the vendored pytest tree in your installed rstest is intact. rstest ships
an unmodified copy of pytest inside its worker package; this rehashes every
file under `_vendor/` and compares it to the packaged manifest (`vendor.lock`),
catching an accidentally-edited, corrupted, or partial install. Run-less: it
verifies and exits without running your suite.

```console
$ rstest verify-vendor
vendored pytest 9.1.1: 84 files verified against vendor.lock
```

Exit 0 when the tree matches the manifest, non-zero on any drift (each
offending file is listed). The check is **offline**: it does not contact
PyPI. Proving the vendored tree matches *upstream* pytest (not just what
shipped) is a separate maintainer/CI check (`vendor.yml` provenance job); see
[Security & supply chain](security.md#verifying-the-vendored-copy-is-unmodified).

## Agent skills

### `install-skills`

Write the agent skills bundled with this rstest (`migrate-to-rstest`,
`rstest-triage`) into a skills directory, so a coding agent can drive rstest's
migration and triage workflows. The bundled copy matches the binary, so every
flag and subcommand the skills mention exists in your version. Run-less: it
needs no interpreter and runs no tests.

```console
$ rstest install-skills
  migrate-to-rstest: installed
  rstest-triage: installed
rstest 0.8.0 skills in /path/to/project/.claude/skills. Claude Code picks up project and user skills live; if they don't show up, start a new session.
```

| Flag | Effect |
|------|--------|
| (none) | Write `./.claude/skills/` |
| `--user` | Write `~/.claude/skills/` (`~/.agents/skills/` with `--agents`) |
| `--agents` | Write `.agents/skills/`, the layout Codex and other agents read |
| `--dir DIR` | Write `DIR/<skill>/`; overrides `--user` / `--agents` |
| `--force` | Overwrite installed skills that differ from the bundled copy |

A skill already installed with the same contents is reported up to date. One
whose files differ is left alone and the command exits `1`, unless `--force`.
`--force` overwrites only the files rstest ships; files you added to a skill
directory are kept. Installing as a Claude Code plugin instead, and what to do
when the skills don't appear: [Agent skills](../guides/agent-skills.md).
