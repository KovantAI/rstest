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

The combined data covers the same lines a serial run executes, so the
percentages should match a serial pytest run with the same coverage config.
No gate compares the two numbers yet; the e2e coverage gate checks that the
pool renders a report and that `--cov-fail-under` fails the run. If yours
differ, first check the `--cov-config` note below.

!!! warning "Pass `--cov` flags on the rstest command line, not in `addopts`"
    rstest decides whether to combine and render coverage from its own
    command line only. With `addopts = --cov=pkg --cov-report=xml` (or the
    same in `PYTEST_ADDOPTS`) and no `--cov` on the command line, a
    parallel run measures coverage in every worker but **never combines or
    reports it**: no terminal table, no `coverage.xml`, and the per-worker
    `.coverage.<host>.pid<N>.*` data files are left behind. The run still
    exits 0, so a CI step that uploads `coverage.xml` fails later, or
    uploads a stale file. Move the flags to the command line:

    ```console
    $ rstest --cov=pkg --cov-report=xml
    ```

    (At `-n 0` the `addopts` flags do write `coverage.xml`, but the
    terminal table is not shown.) To keep them in `addopts` anyway, combine after the run
    yourself: `coverage combine && coverage xml`.

## How it works

Each worker runs pytest-cov in its distributed-worker mode (the same mode
it uses under pytest-xdist): coverage is measured per worker and saved as
suffixed `.coverage.*` data files. After the run, rstest plays the role
xdist's controller session would: it combines the data files and renders your
requested reports.

Supported pytest-cov options:

| Option | Behavior |
|---|---|
| `--cov=PKG` (repeatable) | measured in every worker |
| `--cov-report=term` / `term-missing` | printed after the summary |
| `--cov-report=xml[:path]` / `html[:dir]` / `json` / `lcov` / `annotate` | written by the orchestrator |
| `--cov-fail-under=N` | enforced after combining; run exits 1 below N |
| `--cov-context=test` | per-test line contexts, preserved through the parallel merge (see below) |
| `.coveragerc` / `[tool.coverage.*]` config | honored (read by coverage itself from its default locations) |
| `--cov-config=PATH` | **not honored by the combine and report step**: rstest renders with coverage's default config lookup (`.coveragerc`, `setup.cfg`, `tox.ini`, `pyproject.toml`), at `-n 0` too, so `omit` / `exclude_lines` in a custom-named file are ignored in the report. Move the settings to a default location |
| `--cov-append` | does not merge a previous run's `.coverage` into the parallel run's report |
| `--no-cov` | combined with `--cov` on the command line, the report step finds no data and the run **exits 1** (`coverage report failed: No data to report.`). Drop `--cov` instead of adding `--no-cov` |

Multiple `--cov-report` values compose, as under pytest-cov.

## Per-test contexts (`--cov-context=test`)

`--cov-context=test` records *which test covered each line*. Under rstest the
contexts **survive the parallel merge**: each worker records into its own data
file and the combine keeps the labels, so a line executed by tests on different
workers ends up attributed to each of them, identical to a serial run, at
parallel speed. (`--cov-report=html`/`json` are rendered with `show_contexts`
so the per-test attribution shows up in the report.)

A `--cov-context=test` run also writes a **line→test index** to
`.rstest_cache/coverage_index.json`: the map
[`--changed`](changed.md) uses to select only the tests
whose coverage actually executed the changed lines. Warm it by running your
coverage suite once with `--cov-context=test`; persist `.rstest_cache` across
CI runs the same way you persist it for scheduling.

## Diff coverage gate

[`--cov-diff-fail-under=PCT`](../reference/cli.md#-cov-diff-fail-under-pct)
gates a PR on the coverage of **only the lines it added or changed**: the
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
rstest: diff coverage 66.7% (8/12 added lines covered)
  mymod.py: uncovered added line(s) 7, 12-14
```

Needs `--cov`. A diff with no added executable lines (or whose files aren't
under `--cov`) passes: there is nothing to score.

## Notes

- At `-n 0` pytest-cov runs in its ordinary central mode and writes
  `.coverage` through the vendored pytest session. With `--cov` on the
  command line, rstest then renders the requested reports from that file
  after the summary, the same step it runs after a parallel run (so the
  `--cov-config` row above applies at `-n 0` too). In parallel mode rstest
  first combines the per-worker data, as xdist's controller would.
- **With `--shard`, each shard measures only the tests it ran.** For a
  suite-wide number, skip rendering on each shard (`--cov-report=`), then
  **rename its data file uniquely before uploading**. Every shard writes a
  file named `.coverage`, so they collide on a shared artifact. Give each a
  distinct suffix (coverage treats `.coverage.<anything>` as a combinable
  data file):

  ```console
  $ rstest -n 4 --shard $K/$N --cov=mypkg --cov-report=
  $ mv .coverage .coverage.shard-$K      # unique per shard before upload
  ```

  The renamed file is still a dotfile, and `actions/upload-artifact` (v4.4
  and later) skips hidden files unless you set `include-hidden-files: true`
  on the upload step; without it the artifact is empty and the merge finds
  nothing. In a final merge job, download all `.coverage.shard-*` files, then
  `coverage combine && coverage report`. `--cov-fail-under` is per-shard:
  enforce the global threshold in that merge step (`coverage report
  --fail-under=N`), not on individual shards.
- Branch coverage (`--cov-branch`) forwards like any other flag. Per-test
  contexts (`--cov-context=test`) are preserved through the merge and drive the
  `--changed` index: see [Per-test contexts](#per-test-contexts-cov-contexttest).
- Worker data files live in the invocation directory during the run and
  are combined into `.coverage` at the end: the same lifecycle as xdist.
