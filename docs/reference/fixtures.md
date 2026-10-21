# Fixtures

The fixtures rstest provides. All are function-scoped unless noted.

## Worker identity

| Fixture | Scope | Value |
|---|---|---|
| `worker_id` | session | the worker running the test: `gw0`, `gw1`, ...; `"master"` below `-n 2`. pytest-xdist's fixture, same contract ([details](../guides/parallel-safety.md#worker-identity)) |
| `testrun_uid` | session | one id shared by every worker of a run (xdist's `testrun_uid`); a fresh one per session below `-n 2` |

## Per-worker resources

Fixes for the most common parallel-safety blockers, each a one-line change
in the fixture that hard-coded the resource
([guide](../guides/parallel-safety.md#built-in-per-worker-resources)).

| Fixture | Scope | Value |
|---|---|---|
| `rstest_free_port` | function | a TCP port on `127.0.0.1` the OS reported free, never repeated within a worker |
| `rstest_postgres_url` | session | a URL to a Postgres database of the worker's own (`<base>_gw3`), cloned from the base database in `RSTEST_POSTGRES_URL` / `DATABASE_URL` and dropped at the end ([details](../guides/parallel-safety.md#a-postgres-database-per-worker)) |
| `rstest_worker_name` | session | `name(base)` -> `"<base>_<worker>"` (`app_gw3`, `app_master`), for any other per-worker name |
| `rstest_home` | function | a fresh home directory: `HOME`, `USERPROFILE` and the `XDG_*` dirs point inside it until the test ends |

These four ship as a pytest plugin named `rstest_resources` in the rstest
package, so they work under plain pytest and pytest-xdist too while rstest is
installed. Turn it off with `-p no:rstest_resources`, for example when one of
your own fixtures has the same name (yours wins regardless: a conftest
fixture overrides a plugin fixture of the same name).
