# Selecting changed tests (`--changed`)

`rstest --changed` runs only the tests affected by your changes instead of the
whole suite — the fast inner-loop and per-commit-CI gate. Changes come from git
(working tree + untracked vs `HEAD`, or vs a `REV` like `--changed=origin/main`
in CI).

Two selection engines back it, and rstest picks the tightest one available:

| Engine | When | Granularity |
|---|---|---|
| **Import graph** | always available, zero setup | whole test *files* that transitively import a changed module |
| **Coverage index** | when a line→test index is warm | individual *tests* whose recorded coverage hit the changed *lines* |

The coverage engine is strictly tighter and turns on automatically once the
index exists — there is no flag to set and nothing to remember beyond keeping
the index warm.

## Import graph (default, no setup)

With no coverage index, `--changed` maps each changed `.py` file through the
project's import graph to every test file that could reach it, and runs those.
It is conservative by construction — ambiguous module names select every
match, function-local imports still count as edges, a changed `conftest.py`
selects its whole subtree, and any config or non-Python change falls back to a
full run. The one documented gap is dynamic imports
(`importlib.import_module`), which produce no edges; use
[`--changed-strict`](../reference/cli.md#-changed-strict) for correctness-
critical runs.

Editing a widely-imported module reselects most of the suite — correct, but
coarse. That is what the coverage index tightens.

## Coverage index (tighter, when warm)

Run your suite once with coverage contexts:

```console
$ rstest -n auto --cov=src --cov-context=test
```

That writes `.rstest_cache/coverage_index.json` — a map of *which tests'
coverage executed each source line* — as a side effect (see
[Coverage → per-test contexts](coverage.md#per-test-contexts-cov-contexttest)).
From then on, `--changed` maps the *changed lines* to only the tests that
actually executed them:

```console
$ rstest -n auto --changed
rstest: 1 changed file(s) -> 1 affected test target(s)
```

Editing one function now runs only the tests that touch that function, not
every test importing its module.

## How selection decides

Per changed file, `--changed` uses the tightest safe source and **unions** the
results:

| Change | Selected |
|---|---|
| a line the index recorded coverage for | exactly the tests whose coverage hit it |
| **new** code (inserted lines — no prior coverage) | import-graph fallback for that file |
| a file the index never measured | import-graph fallback for that file |
| an untracked file | import-graph fallback for that file |
| a changed **test** file | that test file runs itself |
| `conftest.py` | its whole subtree (import-graph rule) |
| a config or non-`.py` file | full run |
| **no index at all** (cold cache) | byte-identical to the import graph |

The guiding rule: **over-selection is safe, under-selection is not.** Anything
the index can't vouch for falls back to the conservative import graph; the
index is only trusted for the lines it actually recorded.

## Keeping the index warm

The index reflects coverage *as of the run that wrote it*. It is trusted for
the lines it recorded, so a stale index can miss a test added since — keep it
fresh:

- **Rebuild on your coverage runs.** Any `--cov-context=test` run refreshes it.
  A nightly or per-merge coverage job on the main branch keeps it current for
  PRs.
- **Persist `.rstest_cache` across CI runs** — the same cache you persist for
  [duration-aware scheduling](../concepts/scheduling.md) carries the index
  along (see [CI quickstart](ci-quickstart.md)). No index →
  `--changed` simply falls back to the import graph, so a cold cache is never
  wrong, only coarser.
- **Safe to delete** at any time; the next `--cov-context=test` run rebuilds it.

## CI usage

`--changed` is PR-aware: on a pull-request job it diffs against the merge-base
with the PR base branch (auto-detected from `GITHUB_BASE_REF`,
`CI_MERGE_REQUEST_*`, `BUILDKITE_PULL_REQUEST_BASE_BRANCH`), so a clean checkout
of the PR commit still selects exactly the PR's files. Full base-detection and
shallow-clone rules: [`--changed`](../reference/cli.md#-changedrev).

A typical layout: a scheduled main-branch job runs full coverage
(`--cov-context=test`) and saves `.rstest_cache`; PR jobs restore it and run
`rstest --changed` for a tight per-commit gate, falling back to the import
graph for anything the index doesn't cover yet.

## Interactions

- **Sharding.** A sharded coverage run only measures the tests in its shard, so
  its index is partial. Warm the index from an **unsharded** coverage run (or
  merge shard data before building it). See [Sharding](sharding.md).
- **Monorepos.** `--changed` is forwarded to each affected project, which
  narrows within its own tree against its own `.rstest_cache`. See
  [Monorepos](monorepo.md).
- **Watch mode.** [`--watch`](watch-mode.md) uses import-graph selection for
  its targeted reruns.
