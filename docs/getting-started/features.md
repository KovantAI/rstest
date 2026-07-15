# Features

Everything pytest does (via a vendored pytest core), plus:

!!! tip "New here? Start with three"
    `rstest` (parallel by default), `rstest --doctor` (why the suite is slow),
    and `rstest --watch` (instant reruns on save). Evaluating a switch from
    pytest? `rstest --try` gives a 30-second verdict. The full surface:

| Feature | Flag / API | Notes |
|---|---|---|
| Parallel by default | `-n auto` (default) | suite-aware worker count; `-n 0` = byte-exact pytest session |
| Migration check | `--try`, `--migrate-check` | `--try` runs pytest vs `rstest -n auto` and prints the "should I switch?" verdict; `--migrate-check` classifies parallel-only failures |
| Test-granular scheduling | `--dist load` (default) | duration cache runs slowest tests first; module locality preserved |
| Affinity modes | `--dist loadfile/loadscope/loadgroup` | file, fixture-scope, or `xdist_group` affinity (xdist-compatible) |
| Broadcast mode | `--dist each` | every worker runs the full suite (xdist `--dist=each`) for multi-environment validation; outcomes keyed `[gwN]` |
| Serial escape hatch | `@pytest.mark.serial` | exclusive, after the parallel phase |
| Crash recovery | automatic | crashed test reported failed; worker respawns; run completes |
| Flaky handling | `--reruns N`, `@pytest.mark.flaky`, `--only-rerun` | failed-then-passed = flaky (green run, counted, listed); crash-aware. `--reruns` needs `-n >= 2` — ignored under `-n 0/1` |
| Suite diagnostics | `--doctor`, `--doctor-json` | wait-bound tests, parallel floor, fixture hotspots, slowest files |
| Live status footer | automatic on a terminal | per-worker current test + elapsed, progress + ETA |
| Output styles | `--output dots/verbose/bar/github/json` | `bar` (the TTY default) = pytest-sugar-style per-test lines, inline failures, progress bar — works under the parallel pool. `github` emits CI annotations; `json` is a live NDJSON event stream |
| Watch mode | `--watch` | targeted reruns on save via the import graph |
| Smart selection | `--changed[=REV]` | run only tests affected by changed files |
| Coverage | `--cov`, `--cov-report`, `--cov-fail-under` | pytest-cov, combined across workers |
| Global fail-fast | `-x`, `--maxfail=N` | coordinated across all workers |
| Failure reruns cache | `--lf`, `--ff` | merged across workers |
| Hang watchdog | `--worker-timeout SECS` | kills + replaces a worker stuck on one test |
| Project config | `[tool.rstest]` in pyproject | committed defaults for `-n`, `--dist`, `--reruns`, timeout |
| Worker attribution | automatic | `[gwN]` on `-v` lines and failure headers |
| JUnit XML | `--junitxml` | rendered from merged results; flaky tests flagged via property |
| Machine-readable results | `--report-json` | per-test outcome snapshot |

Everything else — fixtures, parametrize, marks, conftest, plugins, ini
config, the pytest flag surface — behaves as pytest because it *is* pytest
underneath. See [Compatibility](../concepts/compatibility.md).
