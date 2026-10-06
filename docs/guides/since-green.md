# Running only what changed since green

Two flags answer "what do I still need to run since the suite was last
green?", from different angles:

- [`--since-green`](../reference/cli.md#-since-green) is
  [`--changed`](changed.md) with a base rstest manages for you: the git commit
  of the last fully green run. It narrows what gets **collected**.
- [`--incremental`](../reference/cli.md#-incremental) collects the whole
  suite, then **skips** every test that passed last run and whose executed
  source is byte-identical now. It needs no git, but needs a warm coverage
  index.

Both keep their state in [`.rstest_cache`](../concepts/caching.md).

## Which one

| | `--changed` | `--since-green` | `--incremental` |
|---|---|---|---|
| Compares against | `HEAD`, a `REV`, or the PR base | the last green commit (`last_green.json`) | each test's last green outcome (`incremental_outcomes.json`) |
| Needs | a git checkout | a git checkout with committed work | a warm coverage index, `-n 2` or more, `--dist load` |
| Granularity | test files ([import graph](changed.md#import-graph-default-no-setup)), or tests when the [coverage index](changed.md#coverage-index-tighter-when-warm) is warm | same as `--changed` | individual tests |
| Not-selected tests | absent from the run | absent from the run | reported as cached passes |
| Fits | PR gates in CI | a long local loop across several commits | a local loop without git, or a cache-carrying job |

Use `--changed` when you can name the base (the PR base, your branch point);
`--since-green` when the base is "wherever it last passed", which you'd
otherwise track by hand across commits and red runs; `--incremental` when
every artifact should list the whole suite, or there is no git.

The three don't stack: an explicit `--changed` silently wins over
`--since-green`, and `--incremental` yields to either with a one-line note on
stderr (`--incremental and --since-green are mutually exclusive; --since-green
takes precedence this run`).

## `--since-green`

The first run has no baseline, so it runs everything and records `HEAD`.
Later runs diff the working tree against the recorded commit and run what the
change set reaches, with `--changed`'s selection engines:

```console
$ rstest --since-green -q
rstest: --since-green: no prior green run recorded; running everything to establish the baseline
..... [100%]

5 passed in 0.13s
```

Edit `app/tax.py`, commit, run again:

```console
$ rstest --since-green -q
rstest: --since-green: selecting changes since last green run (706921f7c03f)
rstest: --changed is using the import graph (no coverage map). A prior `--cov --cov-context=test` run enables coverage-precise selection (usually fewer tests).
rstest: 1 changed file(s) -> 1 affected test target(s)
..                                                                       [100%]
2 passed in 0.00s
$ rstest --since-green -q
rstest: --since-green: selecting changes since last green run (db66a575c920)
rstest: no tests affected by 0 changed file(s)
```

### How the baseline advances

The baseline moves to `HEAD` only when **both** hold:

- **The run is fully green** (exit 0). A red run leaves the baseline where it
  was, so the change that broke a test stays in the diff, and its tests keep
  being selected, until a run passes. A run where nothing is affected counts
  as green.
- **The working tree is clean** as `--changed` sees it: nothing modified,
  staged or untracked, ignoring runner artifacts (`.rstest_cache`,
  `.pytest_cache`, `__pycache__`, `htmlcov`, `.coverage*`, `coverage.xml`).
  A green run over uncommitted edits proves the working tree, not `HEAD`, so
  it keeps the old baseline and says so:

  ```text
  rstest: --since-green: working tree has uncommitted changes; the green baseline stays put until a green run on a clean tree
  ```

  Commit, then run once more: the same tests run again (they are still in the
  diff) and this time the baseline advances.

!!! warning "Report files in the tree pin the baseline"
    An untracked `--junitxml junit.xml` or `--report-json report.json` written
    inside the repository makes the tree dirty after every run, so the
    baseline never advances, and from the next run on the file is a changed
    non-Python file, which forces a full run
    (`--changed falling back to full run (non-Python file changed: report.json)`).
    Gitignore report paths or write them outside the checkout. The
    [bundled GitHub action](https://github.com/KovantAI/rstest/tree/main/.github/actions/rstest)
    writes `junit.xml` in the working directory by default.

The baseline also resets, running everything once, when the environment
changes: the interpreter, the content of `uv.lock`, `poetry.lock`,
`pdm.lock` or `requirements.txt`, or the set of installed distributions in
the venv (a `pip install` with no lockfile change counts). A config or
non-Python file in the diff falls back to a full run, as under `--changed`.

### With a warm coverage index

Without an index, a changed source file reselects every test file that
imports it. Warm the index with one **full** coverage run and later
`--since-green` runs narrow to the tests that executed the changed lines:

```console
$ rstest --cov=app --cov-context=test --cov-report= -q
.....                                                                    [100%]
5 passed in 0.01s
$ rstest --since-green -q          # after editing and committing discount()
rstest: --since-green: selecting changes since last green run (db66a575c920)
rstest: 1 changed file(s) -> 1 of 5 mapped test(s) affected
.                                                                        [100%]
1 passed in 0.00s
```

The index is trusted for a file only while that file's content at the
baseline commit matches what the index recorded (see
[How selection decides](changed.md#how-selection-decides)). As the baseline
moves past your commits, edited files drift and fall back to the import
graph: safe, only coarser. Re-warm with a full `--cov-context=test` run from
time to time. Don't add `--cov-context=test` to the `--since-green` runs
themselves: a coverage run rewrites the index from the tests it ran, so a
narrowed run drops every other test's entries.

### What it needs

- **Git and a commit.** Outside a git checkout or on an unborn branch there is
  nothing to record: every run is a full run, with the "no prior green run"
  note each time.
- **The baseline commit in the clone.** If `last_green.json` names a commit
  the checkout lacks (a shallow CI clone), the diff fails with
  `fatal: bad object <sha>` and rstest exits 1. Fetch full history or delete
  `.rstest_cache/last_green.json`.

[`--changed-strict`](../reference/cli.md#-changed-strict) composes: an empty
selection exits 5 instead of 0.

## `--incremental`

```console
$ rstest -n 4 --incremental --cov=. --cov-context=test --cov-report= -q
..... [100%]

5 passed in 0.22s
$ rstest -n 4 --incremental --cov=. --cov-context=test --cov-report= -q
rstest: --incremental: 5 of 5 test(s) unchanged since last green -> skipped (cached)

5 passed in 0.19s (5 cached)
```

Edit `app/tax.py` (no commit needed) and the tests that executed it, and only
those, run again:

```console
$ rstest -n 4 --incremental --cov=. --cov-context=test --cov-report= -q
rstest: --incremental: 3 of 5 test(s) unchanged since last green -> skipped (cached)
.. [100%]

5 passed in 0.22s (3 cached)
```

A test is skipped only if it passed last run **and** every file it executed,
its own test file, and every first-party module its test file imports
(transitively) are byte-identical now. A failing test is never green, so it
runs every time until it passes. Outcomes are recorded after every eligible
run, red or green, so unlike `--since-green` a single failure doesn't make the
rest of the suite rerun.

Keep `--cov --cov-context=test` on the incremental runs: rstest folds the
skipped tests' prior coverage back into the rewritten index, so it stays
complete. Without `--cov` rstest warns that the index won't be refreshed, and
changed tests keep re-running until a coverage run. With no index ever
written, nothing is skipped. Prefer `--cov=.` (or a package name if you also
run from subdirectories, see the [reference](../reference/cli.md#-incremental)):
a narrower `--cov` turns any edit outside it into a full run, with a warning.

Skipping is **off for the run**, silently (no `(N cached)` in the summary),
when a config file (`pyproject.toml`, `pytest.ini`, `setup.cfg`, `tox.ini`,
`.coveragerc`, the pytest 9 names) or any `conftest.py` changed, or when any
git-tracked non-Python file changed. Untracked data files are not watched.

It needs the parallel pool with full collection and `--dist load`. Under
`-n 0`/`-n 1`, another `--dist`, `--collect lazy`, `--shard` or `--shuffle`,
it runs everything with a note (`--incremental needs the parallel pool with
full collection and --dist load ...`). `-n auto` sizes a quick suite (a few
seconds of cached test time) down to one worker, which trips this: pass an
explicit `-n N` there.

## Telling what was skipped

| | `--since-green` | `--incremental` |
|---|---|---|
| stderr | the baseline sha, then `N changed file(s) -> ...` | `K of N test(s) unchanged since last green -> skipped (cached)` |
| summary | counts cover only what ran | counts cover the whole suite, plus `(K cached)` |
| `--report-json` / JUnit | only the selected tests; **not written** when nothing is affected | every test; skipped ones carry `"cached": true` and no `setup`, `teardown` or `duration` |

```console
$ jq '[.tests[] | select(.cached)] | length' report.json
3
```

A `--since-green` run with nothing affected exits before running anything;
CI steps that expect a report file must tolerate its absence, as for
[`--changed`](changed.md#ci-usage).

## Local loop vs CI

**Locally** both work out of the box: `.rstest_cache` sits at the rootdir and
survives between runs. A typical loop is `rstest --since-green` while
committing as you go, with a full `--cov --cov-context=test` run now and then
to keep the index tight; or `rstest -n auto --incremental --cov=. --cov-context=test`
if you'd rather not commit to advance anything.

**In CI** the state must survive the job:

- Persist `.rstest_cache` as a directory, as the
  [CI quickstart](ci-quickstart.md#github-actions) recipe and the bundled
  action's default `actions/cache` backend do. The `--cache-remote`
  [shared cache backend](ci-shared-cache.md) merges durations, flakes and the
  coverage index only; it does not carry `last_green.json` or
  `incremental_outcomes.json`. If you cache individual files, include them
  ([Rules for every CI cache](ci-shared-cache.md#ci-cache-rules)).
- For `--since-green`, check out with full history (`fetch-depth: 0`) so the
  recorded commit exists, and keep report files out of the tree (see above).
  For a pull request, `--changed` already diffs against the PR base, which is
  usually the base you want; `--since-green` suits a long-lived branch job.
- Keep the default-branch runs that warm the cache **full**, for the reasons
  in [Warm from full default-branch runs](ci-shared-cache.md#warm-from-full-default-branch-runs).

## Interactions

- **Monorepo root.** `--since-green` is refused at a monorepo root unless
  `--changed` is also given; run it inside a project. `--incremental` is
  forwarded to every project. See [Monorepo mode](../concepts/monorepo.md).
- **Replay.** [`rstest replay`](replay.md) turns both off and reruns the
  recorded schedule as it was.
- **Resetting.** Delete `.rstest_cache/last_green.json` or
  `.rstest_cache/incremental_outcomes.json` to force one full run.
  [`rstest explain`](../reference/cli-commands.md#explain) shows a test's
  outcome on the last incremental run.
