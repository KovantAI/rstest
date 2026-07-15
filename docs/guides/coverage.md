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
| `.coveragerc` / `[tool.coverage.*]` config | honored (read by coverage itself) |

Multiple `--cov-report` values compose, as under pytest-cov.

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
- Branch coverage (`--cov-branch`) and context options forward like any
  other flag.
- Worker data files live in the invocation directory during the run and
  are combined into `.coverage` at the end — the same lifecycle as xdist.
