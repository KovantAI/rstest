<!-- Generated from the Rust type by `cargo test -p rstest-cli schema`. Do not edit by hand; regenerate with RSTEST_BLESS_SCHEMAS=1. -->

## Migrate-check

Source: `migrate-check --migrate-check-json`

The `--migrate-check-json` document (schema 1). Field order is alphabetical to match the historical `serde_json` map output (no `preserve_order`), so the emitted bytes are unchanged by the move to typed structs.

| Field | Type | Required | Description |
|---|---|---|---|
| `meta` | MigrateMeta | yes |  |
| `parallel` | ParallelReport or null | no | Parallel-phase result: `null` when the phase was skipped (WILL-bail ids force `-n 0`) or could not capture outcomes. |
| `ready` | boolean | yes | Whether the suite is parallel-ready (no blocking findings). |
| `tests_collected` | integer | yes | Tests collected (union across the two collection runs). |
| `unstable_ids` | array of UnstableSite | yes | Unstable-nodeid findings, grouped by test site. |
| `will_bail_count` | integer | yes | Count of per-process-unstable ids that force `-n 0`. |

### Finding

One parallel-only failure finding.

| Field | Type | Required | Description |
|---|---|---|---|
| `allowed` | boolean | yes | Whether this nodeid is on the allow list. |
| `fix` | string | yes | The suggested fix. |
| `nodeid` | string | yes | The failing test's node id. |
| `polluter` | PolluterJson or null | no | The bisected polluter, or `null` when none was found. |
| `verdict` | string | yes | The classification verdict title. |
| `why` | string | yes | Why it fails only under parallelism. |

### MigrateMeta

Envelope metadata for the migrate-check document.

| Field | Type | Required | Description |
|---|---|---|---|
| `kind` | string | yes | Constant discriminator: always `"migrate-check"`. |
| `runner` | string | yes | Constant producer tag: always `"rstest"`. |
| `schema` | integer | yes | Document schema version. |

### ParallelReport

Result of the `-n auto` parallel phase. Fields other than `ran` are absent when the phase did not actually run to completion.

| Field | Type | Required | Description |
|---|---|---|---|
| `findings` | array of Finding or null | no | Per-test parallel-only findings (empty when ready). |
| `preexisting` | integer or null | no | Tests that already fail at `-n 0` (pre-existing, not a parallelism bug). |
| `ran` | boolean | yes | Whether the parallel phase actually ran. |
| `ready` | boolean or null | no | Whether it passed (present only once the phase ran). |

### PolluterJson

A finding's polluter: the file that, run first, reproduces the failure.

| Field | Type | Required | Description |
|---|---|---|---|
| `file` | string or null | no | The polluting file (absent for `not_reproducible`). |
| `kind` | string | yes | Polluter kind: `other_file`, `same_file`, or `not_reproducible`. |

### UnstableSite

One unstable-nodeid finding, grouped by test site (`file::test`).

| Field | Type | Required | Description |
|---|---|---|---|
| `allowed` | boolean | yes | Whether this site is on the `--allow-unstable` list. |
| `fix` | string | yes | The upstream fix for the worst instability kind here. |
| `kinds` | object of integer | yes | Count of unstable ids at this site, keyed by instability kind. |
| `sample` | string | yes | A sample parametrize id from this site. |
| `site` | string | yes | The test site (`file::test`). |
| `will_bail` | boolean | yes | Whether the site WILL bail at `-n auto` (per-process-unstable id). |
