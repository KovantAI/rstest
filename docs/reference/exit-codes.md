# Exit codes

rstest uses pytest's exit-code vocabulary:

| Code | Meaning |
|---|---|
| 0 | All tests passed |
| 1 | Some tests failed; a gating flag fired (table below); or rstest itself rejected the run before or after dispatch (see below) |
| 2 | Interrupted (e.g. collection errors abort the run, as in pytest); also a **parse error from rstest's argument parser**: a missing flag value (`--python` with no argument), an unexpected argument (`-n -5`), or a `-n` value that is not a worker count (`-n logical`, `-n 4.0`) |
| 3 | Internal error (including a worker lost beyond the restart budget) |
| 4 | **Usage error from the vendored pytest core**: an unrecognized argument forwarded to it, or a bad pytest option |
| 5 | No tests collected |

**Exit 1 is not only "tests failed".** On a test run, only syntax errors
caught by rstest's argument parser exit 2. Every other error rstest raises
itself exits **1**, the same code as a test failure. The verdict subcommands
`try`, `migrate-check`, `xdist-removal-check`, `audit` and `bisect` are the
exception: their errors exit 2 (see
[below](#gating-flags-and-their-exit-codes)). Errors that exit 1 include:

- a bad value or combination for an rstest flag: `--dist no`,
  `--order bogus`, `--shard 1/2` with `-n 0` (or an `-n auto` that resolves
  to one worker), `--collect lazy` with `--dist loadscope`, and
  pytest-xdist's `--looponfail` / `-f` (use `--watch`);
- `--cache-pull`/`--cache-push` without `--cache-remote`, `cache-compact`
  without a remote, or a failed cache pull;
- no usable Python interpreter found;
- `--require-baseline` with a cold duration cache.

These print an `Error:` line on stderr. A bad `-n` value is the exception
among flag values: the argument parser rejects it, so it exits **2** with
clap's lowercase `error:` line:

```text
error: invalid value 'logical' for '--numprocesses <NUMPROCESSES>': expected a non-negative integer or "auto"
```

The same value in `[tool.rstest] numprocesses` is not an error: it is
reported as a warning and ignored (see
[Configuration file](cli.md#configuration-file)). If your CI must tell "tests failed"
from "rstest refused to run", check for a report file
([`--junitxml`](cli.md#-junitxml-path) / [`--report-json`](cli.md#-report-json-path)):
rejected runs write none.

## Gating CI on exit code and report

The process exit code alone can't separate every case. Combine it with
whether the `--report-json` file exists and its `meta.exitstatus`, which
records the test session's result **before** the post-run gates
(`--fail-on-leak`, `--durations-regress`, `--doctor-fail-on`,
`--cov-fail-under`, `--cov-diff-fail-under`) raise the exit to 1:

| Exit | Report file | `meta.exitstatus` | What happened |
|---|---|---|---|
| 0 | written | 0 | tests ran and passed |
| 0 | **none** | n/a | `--changed` selected nothing, so nothing ran (use [`--changed-strict`](cli.md#-changed-strict) to get 5 instead) |
| 1 | written | 1 | tests failed |
| 1 | written | 0 | tests passed, a post-run gate failed |
| 1 | **none** | n/a | a test run rstest refused: bad flag value, failed cache pull, no interpreter, `--require-baseline` with a cold cache |
| 2 | written | 2 | collection errors interrupted the run |
| 5 | written | 5 | no tests collected |
| 5 | **none** | n/a | `--changed-strict` and nothing affected |

A monorepo root reads this table differently:

- It is the exception to "no report": it still writes `--report-json` when
  `--changed` skips every project (see below), and it writes the merged
  report even when a project refused to run.
- Its `meta.exitstatus` (and each `meta.projects[*].exitstatus`) is the
  merged **process** exit of the projects, so post-run gate failures are
  already in it. At a root, `exit 1` with `meta.exitstatus` 1 can mean a
  failed test or a failed gate; read the project's own entry, or its
  per-project report, to tell them apart.

## Gating flags and their exit codes

Flags and subcommands that gate CI have exit semantics beyond the table above:

| Flag or subcommand | Exit codes |
|---|---|
| [`try`](cli-commands.md#try) | `0` outcomes identical to pytest, `1` they differ, `2` nothing to compare (either side hit a collection error or ran no tests), couldn't run pytest, rstest refused to dispatch, or an error |
| [`migrate-check`](cli-commands.md#migrate-check) / `--migrate-check-json` | `0` ready, `1` any WILL-bail unstable id **or** parallel-only failure, `2` the parallel pass produced no outcomes or an error |
| [`xdist-removal-check`](cli-commands.md#xdist-removal-check) / `--xdist-removal-json` | `0` ready to uninstall pytest-xdist, `1` a blocking finding not allowed by `--migrate-allow`, or a `--xdist-trial` session that didn't start or regressed, `2` an error |
| [`--durations-regress`](cli.md#-durations-regress-ratio) | `1` on a duration regression over the threshold |
| [`--cov-fail-under`](../guides/coverage.md) | `1` when coverage falls below the target |
| [`--changed-strict`](cli.md#-changed-strict) | `5` when nothing is affected (instead of `0`) |
| [`--cov-diff-fail-under`](cli.md#-cov-diff-fail-under-pct) | `1` when diff coverage is below the threshold |
| [`--doctor-fail-on`](cli.md#-doctor-fail-on-cond) | `1` when any condition breaches; an unknown metric or malformed condition errors (`1`) before the run |
| [`--fail-on-leak`](cli.md#-fail-on-leak) | `1` when any thread/fd leak is found |
| [`--require-baseline`](cli.md#-require-baseline) | `1` before the run when `--durations-regress` has no duration baseline |
| [`--quarantine`](cli.md#-quarantine-file) | `0` when every failure is on the quarantine list; `1` if any failure is outside it |
| [`audit`](cli-commands.md#audit) | `0` parallel-safe, `1` at least one parallel-only failure, `2` rstest refused to dispatch or an error |
| [`bisect`](cli-commands.md#bisect-nodeid) | `0` order-dependent culprit found, `1` not order-dependent, `2` nodeid not in the suite, a selection passed after `--`, or an error |
| [`replay`](cli-commands.md#replay) | the replayed run's own code; `1` with an `Error:` line when the journal is missing or unreadable |
| [`shard-verify`](cli-commands.md#shard-verify) | `0` shards agree and cover the suite, `1` any drop, overlap, missing/duplicate shard, or divergent collection |
| [`explain`](cli-commands.md#explain) | human mode: `1` for an unknown nodeid; with `--json`: always `0` |
| [`install-skills`](cli-commands.md#install-skills) | `0` every bundled skill installed or already up to date, `1` an installed skill differs from the bundled copy and was left alone (pass `--force` to overwrite it) |
| [`verify-vendor`](cli-commands.md#verify-vendor) | `0` vendored tree matches its manifest, `1` on any drift (a changed, missing or extra file), a missing `vendor.lock` or `_vendor/`, or no usable Python interpreter (pass one with `--python`) |

For `try`, `migrate-check`, `xdist-removal-check`, `audit` and `bisect`,
`1` is always a verdict ("found something"), never an rstest error: an
error inside them (no usable interpreter, a failed spawn) exits `2` and
prints an `Error:` line on stderr, so a CI gate can treat `1` as "fix the
suite" and `2` as "fix the job". A parse error from rstest's argument parser also exits `2` (with clap's
`error:` prefix, lowercase).

rstest's own flags such as `--python` may come before or after the
subcommand (`rstest --python X try` and `rstest try --python X` are the
same). A subcommand name after a test path or a pytest option's value
(`rstest -k foo try`) is a usage error (exit `2`), unless a file or
directory of that name exists, in which case it is a plain test run with
the test-run exit codes above.

When several gates fire, the exit is still `1`; each gate only raises a `0`
to `1`, never lowers a failing code.

## Special cases

- **`--changed` with nothing affected exits 0 without running.** By exit
  code alone that is indistinguishable from "everything passed", and **no
  `--junitxml` or `--report-json` file is written**, so a CI step that
  uploads or parses those files must tolerate their absence
  (`if-no-files-found: ignore`). Under
  [`--changed-strict`](cli.md#-changed-strict) it exits **5** instead, so
  gating pipelines see the difference. At a monorepo root the exit codes are
  the same, but `--report-json` is still written, with every project marked
  `"skipped": true`.
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
