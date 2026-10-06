<!-- Generated from the Rust type by `cargo test -p rstest-cli schema`. Do not edit by hand; regenerate with RSTEST_BLESS_SCHEMAS=1. -->

## Bisect

Source: `bisect --bisect-json`

The `--bisect-json` document (schema 1). A run that ended without a verdict (refused, or an error) carries `error` and no `rootdir`/`cwd`.

| Field | Type | Required | Description |
|---|---|---|---|
| `culprits` | array of string | yes | The culprit test ids, in the order they run before the victim. Empty when the victim is not order-dependent or the run ended without a verdict. |
| `cwd` | string | no | Directory `reproduce_command` runs from. |
| `error` | string | no | Why the run ended without a verdict (absent on a completed bisect). |
| `meta` | BisectMeta | yes | Envelope: producer, document kind and schema version. |
| `nodeid` | string | yes | The victim test's node id, relative to `rootdir`. |
| `order_dependent` | boolean | yes | Whether the victim fails only after the culprits run first. |
| `reproduce_command` | string or null | yes | A command that reproduces the failure, or `null` when the victim is not order-dependent. |
| `rootdir` | string | no | The pytest rootdir the node ids are relative to. |

### BisectMeta

Envelope metadata for the bisect document.

| Field | Type | Required | Description |
|---|---|---|---|
| `kind` | string | yes | Constant discriminator: always `"bisect"`. |
| `runner` | string | yes | Constant producer tag: always `"rstest"`. |
| `schema` | integer | yes | Document schema version. |
