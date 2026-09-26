# Exit codes

rstest uses pytest's exit-code vocabulary:

| Code | Meaning |
|---|---|
| 0 | All tests passed |
| 1 | Some tests failed; a gating flag fired (table below); or rstest itself rejected the run before or after dispatch (see below) |
| 2 | Interrupted (e.g. collection errors abort the run, as in pytest); also a **parse error from rstest's argument parser**: a missing flag value (`--python` with no argument) or an unexpected argument (`-n -5`) |
| 3 | Internal error (including a worker lost beyond the restart budget) |
| 4 | **Usage error from the vendored pytest core**: an unrecognized argument forwarded to it, or a bad pytest option |
| 5 | No tests collected |

**Exit 1 is not only "tests failed".** Only syntax errors caught by rstest's
argument parser exit 2. Every other error rstest raises itself exits **1**,
the same code as a test failure. That includes:

- a bad value or combination for an rstest flag: a non-integer `-n`,
  `--dist no`, `--order bogus`, `--shard 1/2` with `-n 0` (or an `-n auto`
  that resolves to one worker), `--collect lazy` with `--dist loadscope`;
- `--cache-pull`/`--cache-push` without `--cache-remote`, `cache-compact`
  without a remote, or a failed cache pull;
- no usable Python interpreter found;
- `--require-baseline` with a cold duration cache.

These print an `Error:` line on stderr. If your CI must tell "tests failed"
from "rstest refused to run", check for a report file
([`--junitxml`](cli.md#-junitxml-path) / [`--report-json`](cli.md#-report-json-path)):
rejected runs write none.

## Gating flags and their exit codes

Flags that gate CI have exit semantics beyond the table above:

| Flag | Exit codes |
|---|---|
| [`try`](cli-commands.md#try) | `0` outcomes identical to pytest, `1` they differ, `2` couldn't run pytest or rstest refused to dispatch |
| [`migrate-check`](cli-commands.md#migrate-check) / `--migrate-check-json` | non-zero (`0` clean) when any WILL-bail unstable id **or** parallel-only failure is found |
| [`--durations-regress`](cli.md#-durations-regress-ratio) | `1` on a duration regression over the threshold |
| [`--cov-fail-under`](../guides/coverage.md) | `1` when coverage falls below the target |
| [`--changed-strict`](cli.md#-changed-strict) | `5` when nothing is affected (instead of `0`) |
| [`--cov-diff-fail-under`](cli.md#-cov-diff-fail-under-pct) | `1` when diff coverage is below the threshold |
| [`--doctor-fail-on`](cli.md#-doctor-fail-on-cond) | `1` when any condition breaches; an unknown metric or malformed condition errors (`1`) before the run |
| [`--fail-on-leak`](cli.md#-fail-on-leak) | `1` when any thread/fd leak is found |
| [`--require-baseline`](cli.md#-require-baseline) | `1` before the run when `--durations-regress` has no duration baseline |
| [`--quarantine`](cli.md#-quarantine-file) | `0` when every failure is on the quarantine list; `1` if any failure is outside it |
| [`audit`](cli-commands.md#audit) | `0` parallel-safe, `1` at least one parallel-only failure, `2` rstest refused to dispatch |
| [`bisect`](cli-commands.md#bisect-nodeid) | `0` order-dependent culprit found, `1` not order-dependent, `2` nodeid not in the suite or a selection passed after `--` |
| [`shard-verify`](cli-commands.md#shard-verify) | `0` shards agree and cover the suite, `1` any drop, overlap, missing/duplicate shard, or divergent collection |
| [`explain`](cli-commands.md#explain) | human mode: `1` for an unknown nodeid; with `--json`: always `0` |
| [`verify-vendor`](cli-commands.md#verify-vendor) | `0` vendored tree matches its manifest, non-zero on any drift |

When several gates fire, the exit is still `1`; each gate only raises a `0`
to `1`, never lowers a failing code.

## Special cases

- **`--changed` with nothing affected exits 0 without running.** By exit
  code alone that is indistinguishable from "everything passed", and **no
  `--junitxml` or `--report-json` file is written**, so a CI step that
  uploads or parses those files must tolerate their absence
  (`if-no-files-found: ignore`). Under
  [`--changed-strict`](cli.md#-changed-strict) it exits **5** instead, so
  gating pipelines see the difference.
- **Monorepo mode** merges per-project exits with the same rules as
  worker merging below: any severe code (2–4) dominates, then 1, and 5
  only when every project collected nothing. A project skipped by
  `--changed` contributes no exit code.

## Merge rules across workers

A parallel run produces one exit status from many worker sessions:

- Any session reporting 2–4 wins (highest severity).
- Otherwise, any failure anywhere → 1.
- `5` (no tests) only if **every** worker collected nothing: a `-m`
  filter that deselects one worker's whole share must not poison the run.
- Crash-fabricated failures count: a run where a worker died mid-test
  exits 1 even though no session saw the failure.
