# Coverage

pytest-cov works under rstest, including in parallel mode:

```console
$ rstest -n auto --cov=mypkg --cov-report=term-missing
...
4 passed in 0.20s

Name                Stmts   Miss  Cover   Missing
-------------------------------------------------
mypkg/__init__.py       6      1    83%   10
-------------------------------------------------
TOTAL                   6      1    83%
```

Coverage percentages match a serial pytest run exactly — the data is the
same; only the collection is parallel.

## How it works

Each worker runs pytest-cov in its distributed-worker mode (the same mode
it uses under pytest-xdist): coverage is measured per worker and saved as
suffixed `.coverage.*` data files. After the run, rstest plays the role
xdist's master session would: it combines the data files and renders your
requested reports.

Supported pytest-cov options:

| Option | Behavior |
|---|---|
| `--cov=PKG` (repeatable) | measured in every worker |
| `--cov-report=term` / `term-missing` | printed after the summary |
| `--cov-report=xml[:path]` / `html[:dir]` / `json` / `lcov` / `annotate` | written by the orchestrator |
| `--cov-fail-under=N` | enforced after combining; run exits 1 below N |
| `--cov-context=test` | per-test line contexts, preserved through the parallel merge (see below) |
| `.coveragerc` / `[tool.coverage.*]` config | honored (read by coverage itself) |

Multiple `--cov-report` values compose, as under pytest-cov.

## Per-test contexts (`--cov-context=test`)

`--cov-context=test` records *which test covered each line*. Under rstest the
contexts **survive the parallel merge**: each worker records into its own data
file and the combine keeps the labels, so a line executed by tests on different
workers ends up attributed to each of them — identical to a serial run, at
parallel speed. (`--cov-report=html`/`json` are rendered with `show_contexts`
so the per-test attribution shows up in the report.)

A `--cov-context=test` run also writes a **line→test index** to
`.rstest_cache/coverage_index.json` — the map
[`--changed`](changed.md) uses to select only the tests
whose coverage actually executed the changed lines. Warm it by running your
coverage suite once with `--cov-context=test`; persist `.rstest_cache` across
CI runs the same way you persist it for scheduling.

## Diff coverage gate

[`--cov-diff-fail-under=PCT`](../reference/cli.md#-cov-diff-fail-under-pct)
gates a PR on the coverage of **only the lines it added or changed** — the
"did you test the new code?" check, without a separate `diff-cover` or Codecov
step. It reuses the run's own coverage data.

```console
$ rstest -n auto --cov=. --cov-diff-fail-under=90 --changed=origin/main
```

The diff is taken against the [`--changed`](changed.md) base (else `HEAD`).
Each added line that coverage.py counts as an executable statement is scored
covered or missed; non-executable lines (blank, comment) are ignored. Below the
threshold the run exits `1` and the uncovered added lines are named per file:

```text
rstest: diff coverage 83.3% (5/6 added lines covered)
  mymod.py: uncovered added line(s) 7, 12-14
```

Needs `--cov`. A diff with no added executable lines (or whose files aren't
under `--cov`) passes — there is nothing to score.

## Notes

- At `-n 0` pytest-cov runs in its ordinary central mode and produces its
  own report through the vendored pytest session — rstest does not
  re-render it, so the byte-exact contract holds. (In parallel mode rstest
  combines the per-worker data and renders the report, as xdist's master
  would.)
- **With `--shard`, each shard measures only the tests it ran.** For a
  suite-wide number: on each shard skip rendering (`--cov-report=`), then
  **rename its data file uniquely before uploading** — every shard writes a
  file named `.coverage`, so they collide on a shared artifact. Give each a
  distinct suffix (coverage treats `.coverage.<anything>` as a combinable
  data file):

  ```console
  $ rstest -n auto --shard $K/$N --cov=mypkg --cov-report=
  $ mv .coverage .coverage.shard-$K      # unique per shard before upload
  ```

  In a final merge job, download all `.coverage.shard-*` files, then
  `coverage combine && coverage report`. `--cov-fail-under` is per-shard —
  enforce the global threshold in that merge step (`coverage report
  --fail-under=N`), not on individual shards.
- Branch coverage (`--cov-branch`) forwards like any other flag. Per-test
  contexts (`--cov-context=test`) are preserved through the merge and drive the
  `--changed` index — see [Per-test contexts](#per-test-contexts-cov-contexttest).
- Worker data files live in the invocation directory during the run and
  are combined into `.coverage` at the end — the same lifecycle as xdist.
