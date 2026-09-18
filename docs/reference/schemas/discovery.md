<!-- Generated from the Rust type by `cargo test -p rstest-cli schema`. Do not edit by hand; regenerate with RSTEST_BLESS_SCHEMAS=1. -->

## Discovery

Source: `--collect-only --report-json`

The `--collect-only --report-json` discovery document (schema 1). Field order is alphabetical to match the historical `serde_json::Map` output (serde_json has no `preserve_order` here), so the emitted bytes are unchanged by the move to a typed struct.

| Field | Type | Required | Description |
|---|---|---|---|
| `collect_errors` | array of CollectError | yes | Per-collector import/collection errors (empty on a clean collection). |
| `meta` | DiscoveryMeta | yes |  |
| `tests` | array of DiscoveredTest | yes | One entry per collected test item, in collection order. |

### CollectError

A collection-time error (one collector that failed to import/collect).

| Field | Type | Required | Description |
|---|---|---|---|
| `longrepr` | string | yes | The failure text (traceback / repr). |
| `path` | string | yes | The path pytest was collecting when it failed. |

### DiscoveredTest

One discovered test item.

| Field | Type | Required | Description |
|---|---|---|---|
| `file` | string | yes | Absolute source file, or empty when pytest reported no location. |
| `lineno` | integer or null | no | 0-based definition line, or null when pytest reported none. |
| `markers` | array of string | yes | All pytest marker names on the item (own + inherited). |
| `nodeid` | string | yes | The pytest node id. |

### DiscoveryMeta

Envelope metadata for the discovery document.

| Field | Type | Required | Description |
|---|---|---|---|
| `count` | integer | yes | Number of collected test items (`tests.len()`). |
| `kind` | string | yes | Constant discriminator: always `"discovery"`. |
| `rootdir` | string | yes | Absolute project root; `file` paths are resolved against it. |
| `runner` | string | yes | Constant producer tag: always `"rstest"`. |
| `schema` | integer | yes | Document schema version. |
