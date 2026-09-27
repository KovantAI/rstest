<!-- Generated from the Rust type by `cargo test -p rstest-cli schema`. Do not edit by hand; regenerate with RSTEST_BLESS_SCHEMAS=1. -->

## Run report

Source: `--report-json`

The `--report-json` document (schema 5). Fields are declared alphabetically to match the historical output; borrows the run's data so the writer and the HTML embed serialize the same source without cloning.

| Field | Type | Required | Description |
|---|---|---|---|
| `collect_errors` | array of string | yes | Paths of collectors that failed to import/collect. |
| `meta` | SnapshotMeta | yes |  |
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
| `exitstatus` | integer | yes | Process exit status. |
| `runner` | string | yes | Constant producer tag: always `"rstest"`. |
| `schema` | integer | yes | Document schema version. |
| `shard` | ShardJson or null | no | Sharding identity; present only under `--shard K/N`. |
| `started_at_epoch` | integer | yes | Unix epoch (seconds) the run started. |
| `workers` | integer | yes | Worker count for the run (`-n`). |

### TestEntry

Per-test phase outcomes, mirroring the compat-harness recorder schema (rstest-research/harness/recorder.py) so `diff_snapshots.py` can gate rstest output directly against pytest baselines.

| Field | Type | Required | Description |
|---|---|---|---|
| `cached` | boolean | yes | Not executed this run: unchanged since the last green run, so its prior pass was carried forward (`--incremental`). Still counts as passed. |
| `call` | string or null | no |  |
| `cpu` | number or null | no | Call-phase CPU time (process_time), present only when measured (`--doctor` or a live-stream run). Serialized when present so a report-json consumer can spot wait-bound tests (wall ≫ cpu); omitted on a plain run so the snapshot stays byte-comparable to the pytest baseline. |
| `crashed` | boolean | yes | The outcome was fabricated because the worker died on this test (crash or --worker-timeout kill), not produced by pytest. |
| `duration` | number or null | no |  |
| `flaky` | boolean | yes | Passed only after one or more reruns (--reruns). |
| `lineno` | integer or null | no | Source line of the test (0-based, from pytest's report.location), for editor mapping. None when pytest reports no location. |
| `longrepr` | string or null | no | Failure text (assertion repr / traceback), failures only. |
| `quarantined` | boolean | yes | Failed, but matched the --quarantine list: reported distinctly, never fatal to the run. |
| `setup` | string or null | no |  |
| `skip_reason` | string or null | no |  |
| `teardown` | string or null | no |  |
| `wasxfail` | boolean | yes |  |
| `worker` | string or null | no | Worker that produced the final outcome (pool runs only). |
