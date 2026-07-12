# Flaky tests

Three tools, one lifecycle: [`--reruns`](../reference/cli.md#-reruns-n)
rescues a flake *within* a run, the **flake history** remembers it
*across* runs, and [`--quarantine`](../reference/cli.md#-quarantine-file)
ring-fences the tests a team has explicitly decided to tolerate while
they get fixed — without hiding them and without letting new failures
sneak through.

## Detect: `--reruns`

```console
$ rstest -n auto --reruns 2
```

A test that fails and then passes on retry is **flaky**: the run stays
green, the test is counted (`1 flaky`), listed in its own section, and
flagged in junit (`flaky` property) and `--report-json`. See
[`--reruns`](../reference/cli.md#-reruns-n) for per-test budgets
(`@pytest.mark.flaky`) and crash-aware retry semantics.

Reruns answer "don't redden this run." They don't answer "which tests
keep doing this?" — that's the history.

## Remember: the flake history

Every run (except `--dist each`) merges its events into
`.rstest_cache/flakes.json`:

```json
{
  "tests/test_ws.py::test_reconnect": { "flaky": 7, "failed": 2, "last_epoch": 1783850000 },
  "tests/test_api.py::test_poll":     { "flaky": 1, "failed": 0, "last_epoch": 1783700000 }
}
```

- `flaky` — runs where the test passed only after rerun(s)
- `failed` — runs where it hard-failed (quarantined failures included)
- `last_epoch` — when it last misbehaved

The file is **sparse**: only tests that ever flaked or failed get an
entry, so a green suite writes nothing and the file stays small at any
suite size. No flag needed — recording is automatic, like the duration
cache. In CI, persist it the same way you persist `.rstest_cache` for
scheduling (see [CI quickstart](ci-quickstart.md)) and the counts
accumulate across runs; without persistence you still get history on
developer machines and self-hosted runners.

The flaky and quarantined sections annotate each test from this file:

```text
=========== flaky tests (passed after rerun) ===========
  tests/test_ws.py::test_reconnect  (1 rerun; flaked 7x before, failed 2x)
```

A test with `flaky: 7` is not "unlucky" — it's the ranked candidate
list for the next step.

## Ring-fence: `--quarantine`

Write the known offenders to a file (commit it — the quarantine set is
a team decision and its diff history is the audit trail):

```text
# quarantine.txt — tracked in JIRA-1234; remove entries when fixed
tests/test_ws.py::test_reconnect
tests/test_legacy_sync.py::*
```

One nodeid or `*` glob per line; `#` comments and blank lines are
skipped. Then:

```console
$ rstest -n auto --quarantine quarantine.txt
```

A **failure** matching the list is demoted to a `quarantined` outcome:

```text
=========== quarantined failures (known-flaky, non-fatal) ===========

--- QUARANTINED tests/test_ws.py::test_reconnect ---  (flaked 7x, failed 2x before)
ConnectionResetError: [Errno 54] Connection reset by peer
...

1 failed, 41 passed, 1 quarantined in 12.31s
```

The exact semantics:

- **Failures outside the list still fail the run.** Quarantine never
  becomes a blanket mute — a new failure exits 1 as always.
- A run whose only failures are quarantined **exits 0**. Exit codes
  ≥ 2 (usage/internal errors) are never touched.
- A listed test that **passes** is a plain pass — no penalty for
  being on the list on a good day.
- The traceback still prints, in its own section. A quarantined test
  is a tracked liability, not an invisible one.
- `--lf` still reruns quarantined failures — locally they behave like
  the failures they are.

## What CI consumers see

- **Terminal / summary line**: a separate `N quarantined` count; the
  run summary is green when nothing outside the list failed.
- **junit** (`--junitxml`): the testcase carries a
  `quarantined="true"` property and **no `<failure>` element**, so
  junit-gating CI (and dashboards that count failures) stays green
  while still being able to track the quarantine set.
- **`--report-json`** (schema 5): per-test `"quarantined": true` plus
  a `quarantined` key in `meta.counts`. See
  [Run snapshot](../reference/report-json.md).
- **Monorepos**: pass one file at the root; it's forwarded to every
  project as an absolute path. Patterns match each project's
  **project-relative** nodeids (the same ids the child prints).

## Quarantine vs `--reruns`

| | `--reruns` | `--quarantine` |
|---|---|---|
| Scope | one run | policy across runs |
| Covers | intermittent failures that pass on retry | tests failing consistently or too often to retry away |
| Cost | retries burn suite time every run | none — no retries, just accounting |
| Visibility | flaky section + property | own count, section, property; committed list |
| New failures | still fail | still fail |

They compose: `--reruns 2 --quarantine quarantine.txt` retries
everything, and only quarantined tests may fail without reddening the
run.

## Lifecycle

1. `--reruns` keeps the suite green; the history records who needed it.
2. When a test's history shows a pattern, add it to `quarantine.txt`
   with a comment linking the tracking issue.
3. Fix the test; remove the entry. If it was really fixed, the history
   stops accruing — if the entry comes back in review, it wasn't.

The failure mode to avoid is a quarantine list that only ever grows.
The list is diffable and reviewable on purpose: treat an addition like
a TODO with an owner, and audit entries whose `last_epoch` is old —
those are either fixed (remove the entry) or abandoned (fix the test).
