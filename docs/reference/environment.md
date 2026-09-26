# Environment variables

Environment variables rstest sets for tests and plugins, and the ones it reads to change its own behavior.

## Read by tests and plugins (public contract)

| Variable | Value | Meaning |
|---|---|---|
| `RSTEST_WORKER_ID` | `gw0`, `gw1`, ... | identity of the worker running this test; **unset** in `-n 0`/`-n 1` mode (unless `--reruns` makes it a one-worker pool: then `gw0`) |
| `RSTEST_WORKER_COUNT` | integer | pool size; **unset** in `-n 0`/`-n 1` mode (`1` under `--reruns`) |
| `PYTEST_XDIST_WORKER` | `gw0`, `gw1`, ... | pytest-xdist's env var, set for compatibility, so plugins and conftests that grep it work unedited; **unset** in `-n 0`/`-n 1` mode (`gw0` under `--reruns`) |
| `PYTEST_XDIST_WORKER_COUNT` | integer | xdist's pool-size var, same compatibility contract; **unset** in `-n 0`/`-n 1` mode (`1` under `--reruns`) |
| `RSTEST_RUN_UID` | opaque string | one uid per run, shared by every worker (and every project of a monorepo run); also exposed as `workerinput["testrun_uid"]` |
| `RSTEST_MONO_PROJECT` | relative path | set inside a [monorepo](../guides/monorepo.md) child run to that project's path (e.g. `libs/core`); unset otherwise |

Plugins that read pytest-xdist's `workerinput` get the same information via
`request.config.workerinput["workerid"]` / `["workercount"]` /
`["testrun_uid"]`: that path works under both runners.

An "unset" above means rstest does not set it. rstest does not clear
variables either: a `RSTEST_WORKER_ID` or `PYTEST_XDIST_WORKER` already in
your shell or CI environment is inherited by an `-n 0` run, and a test then
sees that value. In a parallel run, a pre-set `PYTEST_XDIST_WORKER` /
`PYTEST_XDIST_WORKER_COUNT` even **wins over** the real value (every worker
sees the same id), which breaks per-worker resource naming. Don't export
these variables yourself. `request.config.workerinput` always carries the
real values.

## Set by the orchestrator (internal)

`RSTEST_BASETEMP`, `RSTEST_SEND_IDS`, `RSTEST_DOCTOR`, `RSTEST_TIMEOUT`,
`RSTEST_LEAKCHECK`, `RSTEST_DEBUGPY_PORT`, `RSTEST_STREAM_OUTPUT` coordinate
workers and may change between versions. Don't depend on them, and **don't set
them**: most are only set on the runs that need them (for example
`RSTEST_DOCTOR` only under `--doctor`), and rstest never removes them, so a
value you export leaks into every other run. For a hermetic run, make sure
none of them are in the environment.

## Honored from the environment

| Variable | Effect |
|---|---|
| `VIRTUAL_ENV` | worker interpreter discovery (first after `--python`) |
| `NO_COLOR` | disables colored output (a forwarded `--color=yes/no` wins) |
| `PYTEST_ADDOPTS` | read by the vendored core, exactly as under pytest. rstest-owned flags placed here (`--reruns`, `--junitxml`, `--timeout`, ...) are **not** seen by rstest; see [CLI](cli.md) |
| `RSTEST_CACHE` | relocates the project cache directory (default `.rstest_cache` in the invocation directory): durations, flakes, coverage index, last-green baseline. Monorepo child projects keep their own `.rstest_cache` regardless |
| `RSTEST_CACHE_REMOTE` | default for [`--cache-remote`](cli.md#-cache-remote-urldir-cache-pull-cache-push) (the flag wins) |
| `RSTEST_CACHE_REMOTE_TOKEN` | bearer token sent to an `http(s)://` cache remote |
| `RSTEST_CACHE_KEEP_LAST` | `cache-compact` / auto-compaction retention: keep the newest N segments loose (default for `--keep-last`) |
| `RSTEST_CACHE_MAX_AGE` | retention by age: keep segments younger than this loose, e.g. `30d` (default for `--max-age`) |
| `RSTEST_CACHE_COMPACT_THRESHOLD` | default for [`--cache-compact-threshold`](cli.md#-cache-compact-threshold-n); an unparseable value is reported, not ignored |
| `RSTEST_WORKER_PATH` | extra directory prepended to the workers' `PYTHONPATH` to locate the `rstest_worker` package (for unusual installs where the project interpreter can't import it) |
| `RSTEST_MAX_MESSAGE_BYTES` | cap on one worker-to-orchestrator message (default 256 MiB); raise it only if a huge suite hits the limit |
| `RSTEST_WALL_TTL_DAYS` | how long a project's recorded wall time (`.rstest_cache/wall.json`, used by the monorepo planner to weight projects) stays valid. Default `30`; `0` keeps it forever |
| `RSTEST_CACHE_DIR` | base dir for the interpreter-probe cache **only** (`<dir>/rstest/interp-probes-v1.json`), which speeds up repeated `--python` version resolution. It does **not** relocate `.rstest_cache/` (durations/flakes); use `RSTEST_CACHE` for that. Defaults to `$XDG_CACHE_HOME` (or `~/.cache`) on Unix and `%LOCALAPPDATA%` on Windows; if none resolve, probing just isn't persisted |
| `RSTEST_FLAKE_RETENTION_DAYS` | how long a test's flake/failure history (`.rstest_cache/flakes.json`) stays relevant. A test with no flake or failure inside this window reads as fixed: its entry is dropped and it stops carrying "flaked _N_x before" annotations. Defaults to `90`; `0` keeps history forever |
