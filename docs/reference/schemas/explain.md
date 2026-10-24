<!-- Generated from the Rust type by `cargo test -p rstest-cli schema`. Do not edit by hand; regenerate with RSTEST_BLESS_SCHEMAS=1. -->

## Explain

Source: `explain --json`

The merged per-nodeid dossier. `null` fields are simply absent from every cache (e.g. a never-flaked test has no `flakes` entry). `found` is false when the nodeid appears in no cache at all.

| Field | Type | Required | Description |
|---|---|---|---|
| `coverage` | Coverage or null | yes | Files this test covered, if a warm coverage index has it. |
| `duration_seconds` | number or null | yes | Last recorded call-phase duration in seconds (see `history` for the spread). |
| `flakes` | FlakeStats or null | yes | Cross-run flake/fail counts + last-event epoch, if the test has any. |
| `found` | boolean | yes | Whether the node id appears in any cache. |
| `history` | HistorySummary or null | yes | Recent runs, if the per-test history has any. |
| `last_outcome` | string or null | yes | `"passed"` if the test was green on the last incremental run; `null` otherwise (absence is not proof of failure; see `flakes` for fail history). |
| `meta` | Meta | yes | Envelope: producer, document kind, schema version and rstest version. |
| `nodeid` | string | yes | The node id that was explained, as given. |
| `source_line` | integer or null | yes | Source def line (1-based) recorded on the last incremental run, if known. |

### Coverage

The coverage footprint of one test: the source files it covered and the total number of covered lines across them.

| Field | Type | Required | Description |
|---|---|---|---|
| `file_count` | integer | yes | Number of source files the test covered. |
| `files` | array of string | yes | Covered source files, sorted, rootdir-relative (the coverage-index keys). |
| `line_count` | integer | yes | Covered lines, summed across those files. |

### FlakeStats (Explain)

| Field | Type | Required | Description |
|---|---|---|---|
| `failed` | integer | yes | Runs where the test hard-failed (quarantined failures included). |
| `flaky` | integer | yes | Runs where the test passed only after rerun(s). |
| `last_epoch` | integer | yes | Unix epoch of the last recorded event (flake or failure). |
| `last_failed_epoch` | integer | yes | Unix epoch of the last hard failure. 0 = none, or a cache written before this field existed (readers then fall back to `last_epoch` when `failed` is non-zero). |

### HistorySummary

The test's recent runs from the per-test history (oldest first).

| Field | Type | Required | Description |
|---|---|---|---|
| `auto_quarantine_at` | integer | yes | Flips at which `--auto-quarantine` catches a test (`[tool.rstest] auto-quarantine` or the default; `0` = off). |
| `auto_quarantined_next_run` | boolean | yes | Whether the next run auto-quarantines this test (`flips` reached `auto_quarantine_at`), unless the `--auto-quarantine-max` cap trips. |
| `call_median` | number or null | yes | Median call-phase seconds over passing runs. |
| `call_p95` | number or null | yes | 95th percentile (nearest rank) call-phase seconds over passing runs. |
| `env_changed` | boolean or null | yes | When the latest run failed: whether the environment fingerprint (interpreter, lockfiles, installed packages) differs between the last passing run and the first failing one. |
| `first_fail_commit` | string or null | yes | When the latest non-skipped run failed: commit of the first run in that failing streak (skips inside the streak are ignored). Suffixed `-dirty` like `last_pass_commit`. |
| `flips` | integer | yes | Flips in the recorded runs: a pass only on rerun, or a failure right after a pass (the `--auto-quarantine` rule). |
| `last_pass_commit` | string or null | yes | Commit of the most recent passing run; suffixed `-dirty` when that run had uncommitted changes. |
| `outcomes` | array of string | yes | Outcome per run, oldest first: `passed`, `failed`, `errors`, `skipped`, `xfailed`, `xpassed`, `quarantined`, `flaky` or `crashed`. |
| `runs` | integer | yes | Number of recorded runs. |
| `setup_median` | number or null | yes | Median setup-phase seconds over passing runs. |
| `suspect_commits` | array of SuspectCommit or null | yes | When the latest run failed: the commits after `last_pass_commit` up to `first_fail_commit` (`git log last..first`), newest first. Empty when both are the same commit; null when git can't resolve them here. |

### Meta (Explain)

Envelope metadata for the explain document.

| Field | Type | Required | Description |
|---|---|---|---|
| `kind` | string | yes | Constant discriminator: always `"explain"`. |
| `rstest_version` | string | yes | The rstest version that wrote the document. |
| `runner` | string | yes | Constant producer tag: always `"rstest"`. |
| `schema` | integer | yes | Document schema version. |

### SuspectCommit

A commit between the test's last pass and its first failure.

| Field | Type | Required | Description |
|---|---|---|---|
| `author` | string | yes | Author name. |
| `sha` | string | yes | Abbreviated commit hash. |
| `subject` | string | yes | First line of the commit message. |
