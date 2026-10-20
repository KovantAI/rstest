<!-- Generated from the Rust type by `cargo test -p rstest-cli schema`. Do not edit by hand; regenerate with RSTEST_BLESS_SCHEMAS=1. -->

## Audit

Source: `audit --audit-json`

The `--audit-json` document (schema 1). Every field but `meta`, `ran` and `parallel_safe` is absent when the audit did not run.

| Field | Type | Required | Description |
|---|---|---|---|
| `inconclusive` | array of string | no | Tests that failed in the parallel pass but did not run in the follow-up runs, so could not be classified. They still fail the gate. |
| `intrinsic_flakes` | array of string | no | Tests that fail intermittently whatever the scheduling (intrinsic flakes). |
| `meta` | AuditMeta | yes | Envelope: producer, document kind and schema version. |
| `order_dependent` | array of string | no | Tests that pass under `--dist loadfile`: they depend on a sibling in their file running first, so keep the file together rather than serial. |
| `parallel_safe` | boolean | yes | Whether the suite is parallel-safe (no parallel-only failures). Always `false` when the audit did not run. |
| `preexisting_failures` | integer | no | Tests that already fail at `-n 0`: pre-existing, not a parallelism issue, and not counted against the gate. |
| `ran` | boolean | yes | Whether the parallel pass produced a run to audit. |
| `serial_candidates` | array of SerialCandidate | no | Tests fixable by pinning them to `@pytest.mark.serial`. |
| `serial_conftest` | string | no | A paste-able `conftest.py` block that marks every serial candidate. |
| `tests` | integer | no | Tests audited (0 when the selection matched nothing). |

### AuditMeta

Envelope metadata for the audit document.

| Field | Type | Required | Description |
|---|---|---|---|
| `kind` | string | yes | Constant discriminator: always `"audit"`. |
| `runner` | string | yes | Constant producer tag: always `"rstest"`. |
| `schema` | integer | yes | Document schema version. |

### SerialCandidate

One test fixable by `@pytest.mark.serial`.

| Field | Type | Required | Description |
|---|---|---|---|
| `fix` | string | yes | The underlying (non-stopgap) fix for this verdict. |
| `nodeid` | string | yes | The failing test's node id. |
| `verdict` | string | yes | The classification verdict title. |
