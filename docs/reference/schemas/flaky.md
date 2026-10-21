<!-- Generated from the Rust type by `cargo test -p rstest-cli schema`. Do not edit by hand; regenerate with RSTEST_BLESS_SCHEMAS=1. -->

## Flaky

Source: `flaky --json`

The ranked flake view. Tests are sorted by flips (most first), then by newest flip.

| Field | Type | Required | Description |
|---|---|---|---|
| `auto_quarantine` | integer | yes | Flips at which `--auto-quarantine` catches a test (`0` = off), from `[tool.rstest] auto-quarantine` or the default. |
| `flaky_tests` | integer | yes | Tests with at least one flip, before `--top` cut the list. |
| `meta` | Meta | yes | Envelope metadata. |
| `tests` | array of FlakyTest | yes | The ranked tests, at most `--top` of them. |
| `tests_in_history` | integer | yes | Tests with any recorded run. |

### FlakyTest

One test that flipped at least once in its recorded history.

| Field | Type | Required | Description |
|---|---|---|---|
| `auto_quarantined` | boolean | yes | Whether `--auto-quarantine` quarantines it at the current threshold. |
| `failed` | integer | yes | Runs that failed (`failed`, `errors`, `crashed` or `quarantined`). |
| `flaky` | integer | yes | Runs that passed only on rerun. |
| `flips` | integer | yes | Flips in the recorded history: a `flaky` pass, or a failure right after a pass. |
| `last_flip_epoch` | integer | yes | Epoch seconds of the newest flip. |
| `nodeid` | string | yes | The test's nodeid. |
| `outcomes` | array of string | yes | Outcome per run, oldest first (same values as `explain`'s history). |
| `runs` | integer | yes | Recorded runs of this test. |
| `trend` | string | yes | `failing` when the last 3 pass/fail results all failed; otherwise `worse` / `better` when the newer half of the history has at least 2 more / fewer flips than the older half, else `steady`. |

### Meta (Flaky)

Envelope metadata for the flaky document.

| Field | Type | Required | Description |
|---|---|---|---|
| `kind` | string | yes | Constant discriminator: always `"flaky"`. |
| `rstest_version` | string | yes | The rstest version that wrote the document. |
| `runner` | string | yes | Constant producer tag: always `"rstest"`. |
| `schema` | integer | yes | Document schema version. |
