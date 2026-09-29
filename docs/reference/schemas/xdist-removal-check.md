<!-- Generated from the Rust type by `cargo test -p rstest-cli schema`. Do not edit by hand; regenerate with RSTEST_BLESS_SCHEMAS=1. -->

## Xdist-removal-check

Source: `xdist-removal-check --xdist-removal-json`

The `--xdist-removal-json` document (schema 1). Fields are alphabetical, as in the other migrate documents.

| Field | Type | Required | Description |
|---|---|---|---|
| `findings` | array of RemovalFinding | yes | Every finding, blocking ones first. |
| `meta` | MigrateMeta | yes | Envelope: `kind` is `"xdist-removal-check"`. |
| `ready` | boolean | yes | Whether pytest-xdist can be uninstalled now (no non-allowed blocking finding, and the trial passed when it ran). |
| `trial` | TrialReport or null | no | The `--xdist-trial` result: `null` when the trial was not requested. |
| `xdist_version` | string or null | no | The installed pytest-xdist version: `null` when it is not installed or the interpreter could not be probed. |

### MigrateMeta

Envelope metadata shared by the migrate documents.

| Field | Type | Required | Description |
|---|---|---|---|
| `kind` | string | yes | Constant discriminator: the producing subcommand (`"migrate-check"`, `"xdist-removal-check"`). |
| `runner` | string | yes | Constant producer tag: always `"rstest"`. |
| `schema` | integer | yes | Document schema version. |

### RemovalFinding

One thing that breaks or changes when pytest-xdist is removed.

| Field | Type | Required | Description |
|---|---|---|---|
| `allowed` | boolean | yes | Whether this location is on the `--migrate-allow` list. |
| `blocking` | boolean | yes | Whether it breaks the run once xdist is gone (`false`: a behavior change or a warning). |
| `fix` | string | yes | The suggested fix. |
| `kind` | string | yes | Finding kind: `addopts_flag`, `addopts_ignored`, `required_plugin`, `ini_key`, `import`, `hook`, `hasplugin_gate`, `plugin_import`, or `plugin_gate`. |
| `location` | string | yes | Where it is: a config source (`pytest.ini addopts`, `PYTEST_ADDOPTS`) or `path:line`. |
| `text` | string | yes | The offending flag, key, or source line. |
| `why` | string | yes | What happens once pytest-xdist is gone. |

### TrialReport

Result of the `--xdist-trial` run with `-p no:xdist`.

| Field | Type | Required | Description |
|---|---|---|---|
| `compared` | boolean | yes | Whether a run with pytest-xdist loaded was compared against (`false` when it isn't installed, so nothing can be called a regression). |
| `error` | string or null | no | The tail of the child's stderr when the session never started. |
| `failed` | integer | yes | Tests that failed with xdist hidden. |
| `regressions` | array of string | yes | Tests that pass with pytest-xdist loaded but fail (or are no longer collected) with it hidden. |
| `started` | boolean | yes | Whether the session with xdist hidden started and reported outcomes. |
| `tests` | integer | yes | Tests that ran with xdist hidden. |
