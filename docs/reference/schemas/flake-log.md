<!-- Generated from the Rust type by `cargo test -p rstest-cli schema`. Do not edit by hand; regenerate with RSTEST_BLESS_SCHEMAS=1. -->

## Flake log

Source: `.rstest_cache/flakes.json`

An object mapping each key to a FlakeStats value.

### FlakeStats

| Field | Type | Required | Description |
|---|---|---|---|
| `failed` | integer | no | Runs where the test hard-failed (quarantined failures included). |
| `flaky` | integer | no | Runs where the test passed only after rerun(s). |
| `last_epoch` | integer | no | Unix epoch of the last recorded event. |
