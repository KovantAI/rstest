# Environment variables

## Read by tests and plugins (public contract)

| Variable | Value | Meaning |
|---|---|---|
| `RSTEST_WORKER_ID` | `gw0`, `gw1`, ... | identity of the worker running this test; **unset** in `-n 0`/`-n 1` mode |
| `RSTEST_WORKER_COUNT` | integer | pool size; **unset** in `-n 0`/`-n 1` mode |
| `PYTEST_XDIST_WORKER` | `gw0`, `gw1`, ... | pytest-xdist's env var, set for compatibility — plugins and conftests that grep it work unedited; **unset** in `-n 0`/`-n 1` mode |
| `PYTEST_XDIST_WORKER_COUNT` | integer | xdist's pool-size var, same compatibility contract; **unset** in `-n 0`/`-n 1` mode |
| `RSTEST_RUN_UID` | opaque string | one uid per run, shared by every worker (and every project of a monorepo run); also exposed as `workerinput["testrun_uid"]` |
| `RSTEST_MONO_PROJECT` | relative path | set inside a [monorepo](../guides/monorepo.md) child run to that project's path (e.g. `libs/core`); unset otherwise |

Plugins that read pytest-xdist's `workerinput` get the same information via
`request.config.workerinput["workerid"]` / `["workercount"]` /
`["testrun_uid"]` — that path works under both runners.

## Set by the orchestrator (internal)

`RSTEST_BASETEMP`, `RSTEST_SEND_IDS`, `RSTEST_DOCTOR`, `RSTEST_WORKER_PATH`
coordinate workers and may change between versions. Don't depend on them.
The orchestrator sets them fresh on every run, so any value you pre-set in
the environment is overwritten — no need to scrub them for a hermetic run.

## Honored from the environment

| Variable | Effect |
|---|---|
| `VIRTUAL_ENV` | worker interpreter discovery (first after `--python`) |
| `NO_COLOR` | disables colored output (a forwarded `--color=yes/no` wins) |
| `PYTEST_ADDOPTS` | read by the vendored core, exactly as under pytest |
| `RSTEST_CACHE_DIR` | base dir for the interpreter-probe cache **only** (`<dir>/rstest/interp-probes-v1.json`), which speeds up repeated `--python` version resolution. It does **not** relocate `.rstest_cache/` (durations/flakes) — that always lives in the invocation directory. Defaults to `$XDG_CACHE_HOME` (or `~/.cache`) on Unix and `%LOCALAPPDATA%` on Windows; if none resolve, probing just isn't persisted |
