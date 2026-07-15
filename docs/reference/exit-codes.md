# Exit codes

rstest uses pytest's exit-code vocabulary:

| Code | Meaning |
|---|---|
| 0 | All tests passed |
| 1 | Some tests failed |
| 2 | Interrupted (e.g. collection errors abort the run, as in pytest); also a **usage error from rstest's own argument parser** — a missing flag value (`--python` with no argument) or an unexpected argument (`-n -5`) |
| 3 | Internal error (including a worker lost beyond the restart budget) |
| 4 | **Usage error from the vendored pytest core** — an unrecognized argument forwarded to it, or a bad pytest option. A bad *value* for an rstest flag (a non-integer `-n`) exits **1** |
| 5 | No tests collected |

The two "usage error" codes split by which parser rejected the input: rstest's
own CLI parser exits **2**, the vendored pytest core exits **4**.

## Gating flags and their exit codes

Flags that gate CI have exit semantics beyond the table above:

| Flag | Exit codes |
|---|---|
| [`--try`](cli.md#-try) | `0` worth adopting, `1` marginal, `2` not worth it |
| [`--migrate-check`](cli.md#-migrate-check) / `--migrate-check-json` | `0` clean, `1` parallel-only failures found |
| [`--durations-regress`](cli.md#-durations-regress-ratio) | `1` on a duration regression over the threshold |
| [`--cov-fail-under`](../guides/coverage.md) | `1` when coverage falls below the target |
| [`--changed-strict`](cli.md#-changed-strict) | `5` when nothing is affected (instead of `0`) |

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
