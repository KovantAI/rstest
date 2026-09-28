<!-- Generated from the Rust type by `cargo test -p rstest-cli schema`. Do not edit by hand; regenerate with RSTEST_BLESS_SCHEMAS=1. -->

## Explain

Source: `explain --json`

The merged per-nodeid dossier. `null` fields are simply absent from every cache (e.g. a never-flaked test has no `flakes` entry). `found` is false when the nodeid appears in no cache at all.

| Field | Type | Required | Description |
|---|---|---|---|
| `coverage` | Coverage or null | yes | Files this test covered, if a warm coverage index has it. |
| `duration_seconds` | number or null | yes | Last recorded call-phase duration in seconds (latest value only; local caches keep no history). |
| `flakes` | FlakeStats or null | yes | Cross-run flake/fail counts + last-event epoch, if the test has any. |
| `found` | boolean | yes | Whether the node id appears in any cache. |
| `last_outcome` | string or null | yes | `"passed"` if the test was green on the last incremental run; `null` otherwise (absence is not proof of failure; see `flakes` for fail history). |
| `meta` | Meta | yes |  |
| `nodeid` | string | yes | The node id that was explained, as given. |
| `source_line` | integer or null | yes | Source def line (1-based) recorded on the last incremental run, if known. |

### Coverage

The coverage footprint of one test: the source files it covered and the total number of covered lines across them.

| Field | Type | Required | Description |
|---|---|---|---|
| `file_count` | integer | yes | Number of source files the test covered. |
| `files` | array of string | yes | Covered source files, sorted, cwd-relative (the coverage-index keys). |
| `line_count` | integer | yes | Covered lines, summed across those files. |

### FlakeStats (Explain)

| Field | Type | Required | Description |
|---|---|---|---|
| `failed` | integer | yes | Runs where the test hard-failed (quarantined failures included). |
| `flaky` | integer | yes | Runs where the test passed only after rerun(s). |
| `last_epoch` | integer | yes | Unix epoch of the last recorded event (flake or failure). |
| `last_failed_epoch` | integer | yes | Unix epoch of the last hard failure. 0 = none, or a cache written before this field existed (readers then fall back to `last_epoch` when `failed` is non-zero). |

### Meta

Envelope metadata for the explain document.

| Field | Type | Required | Description |
|---|---|---|---|
| `kind` | string | yes | Constant discriminator: always `"explain"`. |
| `rstest_version` | string | yes | The rstest version that wrote the document. |
| `runner` | string | yes | Constant producer tag: always `"rstest"`. |
| `schema` | integer | yes | Document schema version. |
