# Exit codes

rstest uses pytest's exit-code vocabulary:

| Code | Meaning |
|---|---|
| 0 | All tests passed |
| 1 | Some tests failed |
| 2 | Interrupted (e.g. collection errors abort the run, as in pytest); also a command-line usage error caught by the argument parser — a missing flag value (`--python` with no argument) or an unexpected argument (`-n -5`) |
| 3 | Internal error (including a worker lost beyond the restart budget) |
| 4 | Usage error from the vendored pytest core — e.g. an unrecognized argument forwarded to it, or a bad pytest option. A bad *value* for an rstest flag (a non-integer `-n`) exits **1** |
| 5 | No tests collected |

## Special cases

- **`--changed` with nothing affected exits 0 without running** — by
  exit code alone that is indistinguishable from "everything passed";
  under [`--changed-strict`](cli.md#-changed-strict) it exits **5**
  instead, so gating pipelines see the difference.
- **Monorepo mode** merges per-project exits with the same rules as
  worker merging below: any severe code (2–4) dominates, then 1, and 5
  only when every project collected nothing. A project skipped by
  `--changed` contributes no exit code.

## Merge rules across workers

A parallel run produces one exit status from many worker sessions:

- Any session reporting 2–4 wins (highest severity).
- Otherwise, any failure anywhere → 1.
- `5` (no tests) only if **every** worker collected nothing — a `-m`
  filter that deselects one worker's whole share must not poison the run.
- Crash-fabricated failures count: a run where a worker died mid-test
  exits 1 even though no session saw the failure.
