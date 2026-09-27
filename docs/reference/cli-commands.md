# CLI subcommands

These commands don't run your suite as a normal test run. Each is given as
the first argument (`rstest try`); a path literally named after one is
disambiguated with `rstest ./try` or `rstest -- try`. `try`,
`migrate-check`, `audit` and `bisect` do run pytest sessions, but as their
own analysis, not as a normal test run. The flags that only apply to a
subcommand are documented with it; everything else is on
[CLI flags](cli.md).

```text
rstest <COMMAND> [OPTIONS]
```

- **Adoption and parallel safety:** [`try`](#try), [`migrate-check`](#migrate-check), [`audit`](#audit), [`bisect <nodeid>`](#bisect-nodeid)
- **CI and the shared cache:** [`shard-verify`](#shard-verify), [`cache-compact`](#cache-compact)
- **Inspection and integrity:** [`explain`](#explain), [`verify-vendor`](#verify-vendor)

## Adoption and parallel safety

### `try`

The zero-config "should I switch?" proof. Runs your suite once under plain
`pytest` and once under `rstest -n auto`, then prints the only two things that
matter: whether the outcomes are **identical** (the `-n 0 ≡ pytest` contract,
checked against your real pytest) and how much **faster** rstest is, with a
rough CI-time saving. No flags, no config.

```console
$ rstest try
================= rstest try =================
  ✓ parity:  8337 tests — identical outcomes to pytest
  ⚡ speed:   pytest 96s  →  rstest 21s   (4.6× at -n auto)
================================================
  → drop-in ready: `rstest` is `pytest`, in parallel.
```

Exit 0 when outcomes are identical, 1 when they differ (it then points you at
`migrate-check` to classify the differences, usually an unstable parametrize
id or a parallel-only failure), 2 when it couldn't run pytest or rstest refused
to dispatch. A pre-existing red pytest run is reported as such, not blamed on
rstest.

`try` is the one command that needs **pytest installed on its own** (it runs
your suite under plain `pytest` for the baseline). rstest itself vendors its
core and doesn't otherwise require an external pytest; if `pytest` isn't on
PATH, `try` exits 2. `migrate-check` and normal runs have no such
requirement.

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

The fix for both is a stable `ids=` on the `parametrize`. If a WILL-bail id
is found, it stops here: nothing runs in parallel until the ids are stable.

**2. Parallel classification.** Otherwise it **runs the suite at `-n auto`**
and classifies every test that fails only under parallelism. The
discriminator reruns (`-n 0` twice and `--dist loadfile`) are **scoped to
the files containing failures**, so cost scales with the number of failing
files, not the suite size; a clean suite runs no discriminators at all. Each
failure lands in one class:

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

Each finding prints the upstream fix and the rstest stopgap. Exits non-zero
if any WILL-bail id or parallelism-specific failure is found: usable as a CI
gate (see `--migrate-check-json` and `--migrate-allow` below for the
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

The first slice of a broader migration assistant.

### `audit`

!!! note "Unreleased"
    Not in rstest 0.7.0 (the latest release); available when installing from
    source, and in the next release.

Auto parallel-safety audit: the one-command answer to "which of my tests
aren't parallel-safe, and how do I fix them?" It **runs the suite at `-n auto`**
(repeat with [`--audit-repeat`](#-audit-repeat-n), since a parallel flake is
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
those. A test that fails under `-n auto` but is missing from a
follow-up run (for example an unstable parametrize id) is reported as
**inconclusive** rather than guessed at. Exits non-zero on any parallel-only
failure (serial-fixable, order-dependent, intrinsic or inconclusive), so it
gates CI; pre-existing failures don't fail the audit. A selection that matches
no tests (for example a `-m` with no matching tests) exits `0` with a "no tests
were selected" note; exit `2` is kept for a run rstest refused to dispatch. [`--audit-json`](#-audit-json-path) writes the findings, the serial set,
and the conftest block for tooling.

### `--audit-json <path>`

Write the `audit` findings as a versioned JSON document (schema `1`):
`{meta, ran, parallel_safe, tests, serial_candidates[], serial_conftest,
order_dependent[], intrinsic_flakes[], inconclusive[], preexisting_failures}`. `serial_candidates[]`
carries each `{nodeid, verdict, fix}`; `serial_conftest` is the paste-able block
as a string. The file is written as `{meta, ran: false, parallel_safe: false}`
before the audit starts and replaced with the full result at the end, so an
audit that stops early (the `-n auto` pass produced no run, exit `2`, or a child
session failed) leaves `ran: false` rather than a stale result from an earlier
run. `-x`/`--maxfail` from your args or `addopts` is lifted for every run the
audit makes, so the whole suite is checked.
Only read by the `audit` subcommand (`rstest audit --audit-json out.json`); on
its own it is ignored and no file is written.

### `--audit-repeat <N>`

How many times `audit` reruns the `-n auto` pass (default `1`). A parallel-only
failure is probabilistic (a race may not fire every run), so a test that fails
in **any** repeat is treated as a candidate. Raise it (e.g. `--audit-repeat 5`)
to shake out intermittent races. The discriminators repeat the same number of
times (the `-n 0` oracle at least twice, `--dist loadfile` at least once), so
an intermittent failure gets as many chances to show up in them as it had in
the parallel pass. That makes a misclassification less likely but does not rule
it out: a test that is flaky in every mode can still pass all serial runs by
chance and be listed as a serial candidate.

### `bisect <nodeid>`

!!! note "Unreleased"
    Not in rstest 0.7.0 (the latest release); available when installing from
    source, and in the next release.

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
   order). If it passes there, the failure doesn't come from ordering (likely
   parallel-only, so try `migrate-check`).
3. **Delta-debug.** [`ddmin`](https://www.st.cs.uni-saarland.de/dd/) over the
   predecessor set: repeatedly run the victim preceded by a subset of the
   earlier tests, shrinking toward the **1-minimal** set that still reproduces.
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
in the collection, every child run and the printed command. The predecessor
set is the suite's plain collection order. To bisect a failure that only
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
`reruns` (config or `--reruns`) is off in the child runs too: a passing rerun
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
suite, or a test selection was passed after `--`. If the victim doesn't run in
a child session (deselected by an option, a collection error), bisect stops
with an error instead of reading that as a pass. `--bisect-json` writes the
result.

### `--bisect-json <path>`

Write the `bisect` result as a versioned JSON document (schema `1`):
`{meta, nodeid, rootdir, cwd, order_dependent, culprits[], reproduce_command}`.
Nodeids are relative to `rootdir`; `reproduce_command` runs from `cwd` and is
null when the test isn't order-dependent. A run that ends without a verdict
(exit `2`, or an error) still writes the document, with an `error` message
and no culprits, so a stale result from an earlier run is never left behind.
Used with the `bisect` subcommand.

## CI and the shared cache

### `shard-verify`

!!! note "Unreleased"
    Not in rstest 0.7.0 (the latest release); available when installing from
    source, and in the next release.

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
don't combine it with `--cache-pull`/`--cache-push` (rstest rejects that
combination rather than silently skipping them).

With no retention flags it folds **all** segments. To keep a recent window loose
(so the newest history stays merge-on-read while the tail is compacted):

- `--keep-last N`: retain the newest N segments; fold only older ones. Env:
  `RSTEST_CACHE_KEEP_LAST`.
- `--max-age DURATION`: retain segments younger than DURATION (a bare number is
  seconds, or a `s`/`m`/`h`/`d`/`w` suffix, e.g. `30d`); fold older ones. Env:
  `RSTEST_CACHE_MAX_AGE`.

A segment retained by **either** rule stays loose. A bad flag/env value is a hard
error, never a silent fold-all.

## Inspection and integrity

### `explain`

!!! note "Unreleased"
    Not in rstest 0.7.0 (the latest release); available when installing from
    source, and in the next release.

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
a cold coverage index yields `null` coverage.

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
