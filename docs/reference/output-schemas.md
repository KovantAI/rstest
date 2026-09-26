# Output schemas

Generated field references and full [JSON Schemas](https://json-schema.org/)
(draft-07) for the rstest JSON outputs that are built from typed Rust structs.

**This list is partial.** Outputs still assembled by hand, and so not covered
here yet, include the run snapshot (`--report-json`), discovery JSON, streaming
JSON (`--output json` / `--stream-json`), migrate-check JSON, `audit` /
`bisect` JSON, `explain --json` and `--cov-diff-json`. Those are documented in
prose in [Report JSON](report-json.md) and the [CLI reference](cli.md).

Everything on this page is **generated from the Rust types that produce the
output**. A golden test in the build regenerates these artifacts and fails CI
if a covered type changes without the docs being refreshed
(`RSTEST_BLESS_SCHEMAS=1 cargo test -p rstest-cli schema`), so the covered
schemas track the code. The guarantee applies only to what the typed struct
describes: a field added outside the struct, or an output assembled from
`serde_json::json!`, is not checked.

The doctor report carries a `schema` version integer; incompatible changes bump
it. The flake log is an unversioned cache file.
For worked examples, the conditions under which each doctor section is
present, and version history, see the prose walkthrough:
[Report JSON: Doctor JSON](report-json.md#doctor-json). The run snapshot
itself is documented only there, in [Shape](report-json.md#shape).

--8<-- "docs/reference/schemas/doctor-report.md"

??? note "Doctor report: full JSON Schema (draft-07)"

    ```json
    --8<-- "docs/reference/schemas/doctor-report.schema.json"
    ```

--8<-- "docs/reference/schemas/flake-log.md"

??? note "Flake log: full JSON Schema (draft-07)"

    ```json
    --8<-- "docs/reference/schemas/flake-log.schema.json"
    ```
