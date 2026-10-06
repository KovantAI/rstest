# Features

Everything pytest does (via a vendored pytest core), plus:

!!! tip "New here? Start with three"
    `rstest` (parallel by default), `rstest --doctor` (why the suite is slow),
    and `rstest --watch` (reruns on save). Evaluating a switch?
    See [`rstest try`](evaluating.md#try-it-first). The tables below cover
    the main features; the [CLI reference](../reference/cli.md) documents
    each flag.

## Day one

| Feature | Flag / API | Notes |
|---|---|---|
| Parallel by default | `-n auto` (default) | logical cores, capped for small suites ([how](run-your-suite.md#controlling-parallelism)); `-n 0` = single-worker mode, one pytest session |
| Suite diagnostics | `--doctor`, `--doctor-json` | wait-bound tests, parallel floor, fixture hotspots, slowest files |
| Watch mode | `--watch` | targeted reruns on save via the import graph |
| Failure reruns cache | `--lf`, `--ff` | merged across workers |
| Global fail-fast | `-x`, `--maxfail=N` | coordinated across all workers |
| Serial escape hatch | `@pytest.mark.serial` | exclusive, after the parallel phase |
| Migration check | `try`, `migrate-check` | `try` runs pytest vs `rstest -n auto` and prints the "should I switch?" verdict; `migrate-check` classifies parallel-only failures |

## Advanced / CI

| Feature | Flag / API | Notes |
|---|---|---|
| Duration-aware scheduling | `--dist load` (default) | duration cache runs slowest tests first; module locality preserved |
| Collection strategy | `--collect full/lazy` (default: auto) | auto picks [lazy](../concepts/lazy-collection.md) for large warm-cache parallel runs: each file is collected once and runs whole on one worker; `full` everywhere else |
| Affinity modes | `--dist loadfile/loadscope/loadgroup` | file, fixture-scope, or `xdist_group` affinity (xdist-compatible) |
| Broadcast mode | `--dist each` | every worker runs the full suite (xdist `--dist=each`) for multi-environment validation; outcomes keyed [`[gwN]`](../concepts/glossary.md#worker-id) |
| Crash recovery | automatic | crashed test reported failed; worker respawns; run completes |
| Flaky handling | `--reruns N`, `@pytest.mark.flaky`, `--only-rerun` | failed-then-passed = flaky (green run, counted, listed); crash-aware. Works at any `-n`: at `-n 0/1`, `--reruns` runs a one-worker pool instead of single-worker mode |
| Live status footer | automatic on a terminal | per-worker current test + elapsed, progress + ETA |
| Output styles | `--output dots/verbose/bar/github/json` (+ `gitlab/buildkite/teamcity/azure/tap`; see [CLI](../reference/cli.md)) | `bar` (the TTY default) = pytest-sugar-style per-test lines, inline failures, progress bar; works under the parallel pool. `github` emits CI annotations; `json` is a live NDJSON event stream. With no `--output` set, single-worker mode prints pytest's own terminal output instead |
| Smart selection | `--changed[=REV]` | run only tests affected by changed files |
| Coverage | `--cov`, `--cov-report`, `--cov-fail-under` | pytest-cov (install it in your project), combined across workers; see [Coverage](../guides/coverage.md) |
| Per-test timeout | `--timeout SECS`, `@pytest.mark.timeout` | interrupts the test in-process with a traceback; no pytest-timeout needed |
| Hang watchdog | `--worker-timeout SECS` | kills + replaces a worker stuck on one test |
| Project config | `[tool.rstest]` in pyproject | committed defaults for `-n`, `--dist`, `--reruns`, `--reruns-only-known-flaky`, `--worker-timeout`, `--collect`, output, monorepo `projects`, and `order` ([keys](../reference/cli.md#configuration-file)) |
| Worker attribution | automatic | `[gwN]` on `-v` lines and failure headers |
| JUnit XML | `--junitxml` | rendered from merged results; flaky tests flagged via property |
| Machine-readable results | `--report-json` | per-test outcome snapshot |
| HTML report | `--html` | self-contained merged report; works in parallel (replaces pytest-html) |

Everything else (fixtures, parametrize, marks, conftest, plugins, ini
config, the rest of the pytest flag surface) behaves as pytest because it
*is* pytest underneath. In parallel, session fixtures run
[once per worker](../guides/parallel-safety.md#session-scoped-fixtures-duplicate),
as under pytest-xdist. See [Compatibility](../concepts/compatibility.md).
