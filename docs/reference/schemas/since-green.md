<!-- Generated from the Rust type by `cargo test -p rstest-cli schema`. Do not edit by hand; regenerate with RSTEST_BLESS_SCHEMAS=1. -->

## Since green

Source: `since-green --json`

What differs between the last green run and now.

| Field | Type | Required | Description |
|---|---|---|---|
| `commit` | string or null | yes | Current `HEAD`. |
| `commits` | array of SuspectCommit or null | yes | Commits after the green commit up to `HEAD` (`git log green..HEAD`), newest first. Null when there is no commit on either side or git can't resolve the range here. |
| `env` | array of Change | yes | Watched environment variables set, unset or changed. |
| `found` | boolean | yes | Whether a green run was ever recorded here; every other field is empty or null when it wasn't. |
| `green_commit` | string or null | yes | Commit of the last green run. |
| `green_epoch` | integer or null | yes | Epoch seconds of the last green run. |
| `meta` | Meta | yes | Envelope metadata. |
| `packages` | array of Change | yes | Packages added, removed or at another version, by name. |
| `python` | Change or null | yes | Interpreter version change, if any. |

### Change

One value that differs: `None` on a side means absent there.

| Field | Type | Required | Description |
|---|---|---|---|
| `green` | string or null | yes | Value at the last green run; `null` when it was absent (added since). |
| `name` | string | yes | Package name, environment variable, or `python`. |
| `now` | string or null | yes | Value now; `null` when it is gone (removed since). |

### Meta (Since green)

Envelope metadata for the since-green document.

| Field | Type | Required | Description |
|---|---|---|---|
| `kind` | string | yes | Constant discriminator: always `"since-green"`. |
| `rstest_version` | string | yes | The rstest version that wrote the document. |
| `runner` | string | yes | Constant producer tag: always `"rstest"`. |
| `schema` | integer | yes | Document schema version. |

### SuspectCommit (Since green)

A commit in a suspect range (`git log <good>..<bad>`).

| Field | Type | Required | Description |
|---|---|---|---|
| `author` | string | yes | Author name. |
| `sha` | string | yes | Abbreviated commit hash. |
| `subject` | string | yes | First line of the commit message. |
