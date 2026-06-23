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

- At `-n 0` pytest-cov runs in its ordinary central mode; rstest still
  renders the report so output is consistent across modes.
- Branch coverage (`--cov-branch`) and context options forward like any
  other flag.
- Worker data files live in the invocation directory during the run and
  are combined into `.coverage` at the end — the same lifecycle as xdist.
