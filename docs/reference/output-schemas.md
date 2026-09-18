# Output schemas

rstest's stable, machine-readable JSON outputs, each with a field reference and
its full [JSON Schema](https://json-schema.org/) (draft-07).

Everything on this page is **generated from the Rust types that produce the
output** — the runner is the single source of truth, so the reference here can
never silently drift from what the CLI actually emits. A golden test in the
build regenerates these artifacts and fails CI if a type changes without the
docs being refreshed (`RSTEST_BLESS_SCHEMAS=1 cargo test -p rstest-cli schema`).

Each schema carries a `schema` version integer; incompatible changes bump it.
For the prose walkthrough of the run snapshot and doctor JSON envelopes, see
[Report JSON](report-json.md).

--8<-- "docs/reference/schemas/doctor-report.md"

??? note "Doctor report — full JSON Schema (draft-07)"

    ```json
    --8<-- "docs/reference/schemas/doctor-report.schema.json"
    ```

--8<-- "docs/reference/schemas/flake-log.md"

??? note "Flake log — full JSON Schema (draft-07)"

    ```json
    --8<-- "docs/reference/schemas/flake-log.schema.json"
    ```
