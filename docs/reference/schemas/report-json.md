<!-- Generated from the Rust type by `cargo test -p rstest-cli schema`. Do not edit by hand; regenerate with RSTEST_BLESS_SCHEMAS=1. -->

## Run report

Source: `--report-json`

The `--report-json` document (schema 5). Fields are declared alphabetically to match the historical output; borrows the run's data so the writer and the HTML embed serialize the same source without cloning.

| Field | Type | Required | Description |
|---|---|---|---|
| `collect_errors` | array of string | yes | Paths of collectors that failed to import/collect. |
| `meta` | SnapshotMeta | yes | Run metadata: argv, counts, exit status, schema version, shard. |
| `tests` | object of TestEntry | yes | Per-test outcomes, keyed by node id. |

### ShardJson

Sharding identity block (only under `--shard`).

| Field | Type | Required | Description |
|---|---|---|---|
| `collection_hash` | string | yes | sha256 of the full ordered nodeid list (identical across shards). |
| `collection_size` | integer | yes | Number of collected nodeids. |
| `k` | integer | yes | This shard's index (1-based). |
| `n` | integer | yes | Total shard count. |

### SnapshotMeta

Run-level envelope for the report document.

| Field | Type | Required | Description |
|---|---|---|---|
| `argv` | array of string | yes | The process argv the run was invoked with. |
| `counts` | object of integer | yes | Outcome counts (pytest accounting); all keys always present. |
| `duration_seconds` | number | yes | Wall-clock run duration, rounded to two decimals. |
| `exitstatus` | integer | yes | Test-session exit status, recorded before the post-run gates (`--fail-on-leak`, `--durations-regress`, `--doctor-fail-on`, `--cov-fail-under`, `--cov-diff-fail-under`) can raise the process exit to 1. |
| `runner` | string | yes | Constant producer tag: always `"rstest"`. |
| `schema` | integer | yes | Document schema version. |
| `shard` | ShardJson | no | Sharding identity; present only under `--shard K/N`. |
| `started_at_epoch` | integer | yes | Unix epoch (seconds) the run started. |
| `workers` | integer | yes | Worker count for the run (`-n`). |

### TestEntry

Per-test phase outcomes, mirroring the compat-harness recorder schema (rstest-research/harness/recorder.py) so `diff_snapshots.py` can gate rstest output directly against pytest baselines.

| Field | Type | Required | Description |
|---|---|---|---|
| `cached` | boolean | no | Not executed this run: unchanged since the last green run, so its prior pass was carried forward (`--incremental`). Still counts as passed. |
| `call` | string | no | Call-phase outcome: `"passed"`, `"failed"` or `"skipped"`. Absent when the test was skipped at setup; an xfail test records `"skipped"` with `wasxfail`. |
| `cpu` | number | no | Call-phase CPU time (process_time plus reaped child processes), present only when measured (`--doctor` or a live-stream run). Serialized when present so a report-json consumer can spot wait-bound tests (wall ≫ cpu); omitted on a plain run so the snapshot stays byte-comparable to the pytest baseline. |
| `crashed` | boolean | no | The outcome was fabricated because the worker died on this test (crash or --worker-timeout kill), or the test was running when SIGINT/SIGTERM stopped the run; not produced by pytest. |
| `duration` | number | no | Call-phase wall time in seconds, 4 decimal places. |
| `flaky` | boolean | no | Passed only after one or more reruns (--reruns or @pytest.mark.flaky). |
| `lineno` | integer | no | Source line of the test (0-based, from pytest's report.location), for editor mapping. Absent when pytest reports no location. |
| `longrepr` | string | no | Failure text (assertion repr / traceback), failures only. |
| `quarantined` | boolean | no | Failed, but matched the --quarantine list: reported distinctly, never fatal to the run. |
| `setup` | string | no | Setup-phase outcome: `"passed"`, `"failed"` or `"skipped"`. |
| `skip_reason` | string | no | pytest's skip message (first 200 characters), keeping its `Skipped: ` prefix. |
| `subtests_failed` | integer | no | Failed subtests (unittest `subTest` / the `subtests` fixture). Any makes `call` "failed"; each also counts as one `failed`, as in pytest. |
| `teardown` | string | no | Teardown-phase outcome: `"passed"`, `"failed"` or `"skipped"`. |
| `wasxfail` | boolean | no | `true` when the test was an expected failure (xfail or xpass); absent otherwise. |
| `worker` | string | no | Worker that produced the final outcome (pool runs only). |
