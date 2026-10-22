# Flaky tests

Four tools, one lifecycle: [`--reruns`](../reference/cli.md#-reruns-n)
rescues a flake *within* a run, the **flake history** and the **run
history** remember it *across* runs,
[`--auto-quarantine`](../reference/cli.md#-auto-quarantinen) (on by default)
ring-fences the tests that history shows flipping between pass and fail, and
[`--quarantine`](../reference/cli.md#-quarantine-file) ring-fences the tests a
team has explicitly decided to tolerate while they get fixed. Neither kind of
quarantine hides a test, and neither lets a new failure sneak through.

## Detect: `--reruns`

```console
$ rstest -n auto --reruns 2
```

A test that fails and then passes on retry is **flaky**: the run stays
green, the test is counted (`1 flaky`), listed in its own section, and
flagged in JUnit (`flaky` property) and `--report-json`. See
[`--reruns`](../reference/cli.md#-reruns-n) for per-test budgets
(`@pytest.mark.flaky`) and crash-aware retry semantics.

Coming from pytest-rerunfailures? rstest's retry reads `--reruns`,
`--only-rerun` and the mark's budget (`reruns=` or positional, as in
`flaky(3)`) and `condition=`. It does not carry over:

- the mark's `reruns_delay`, `only_rerun` and `rerun_except` keywords: ignored
- `--reruns-delay` and `--rerun-except`: forwarded to pytest, where they do
  nothing with the plugin installed and are a usage error (exit 4) without it

A marked test is retried at every worker count, `-n 0` included, with or
without `--reruns`.

Retries and `-x` / `--maxfail` compose: a failed attempt that may still be
rerun does not count toward the limit, so a flake can't stop the run before
its retry; a test counts once its last attempt fails.

See [`@pytest.mark.flaky`](../reference/markers.md#pytestmarkflaky).

Reruns answer "don't redden this run." They don't answer "which tests
keep doing this?": that's the history.

## Remember: the flake history

Every run (except `--dist each` and `rstest replay`) merges its events into
`.rstest_cache/flakes.json`:

```json
{
  "tests/test_ws.py::test_reconnect": { "flaky": 7, "failed": 2, "last_epoch": 1783850000, "last_failed_epoch": 1783800000 },
  "tests/test_api.py::test_poll":     { "flaky": 1, "failed": 0, "last_epoch": 1783700000, "last_failed_epoch": 0 }
}
```

- `flaky`: runs where the test passed only after rerun(s)
- `failed`: runs where it hard-failed (quarantined failures included)
- `last_epoch`: when it last misbehaved; also drives **aging** (below)
- `last_failed_epoch`: when it last hard-failed (`0` = never). Older
  caches lack it; readers fall back to `last_epoch` when `failed` is
  non-zero. Full field reference:
  [flake log schema](../reference/output-schemas.md#flake-log).

The file is **sparse**: only tests that ever flaked or failed get an
entry, so a green suite writes nothing and the file stays small at any
suite size. No flag needed: recording is automatic, like the duration
cache. In CI, persist it the same way you persist `.rstest_cache` for
scheduling (see [CI quickstart](ci-quickstart.md)) and the counts
accumulate across runs; without persistence you still get history on
developer machines and self-hosted runners.

`failed` counts **every** red run that writes the cache, including local runs
and each `--watch` cycle, so a test you broke on purpose while editing picks
up `failed` counts too. Read `flaky` as the flake signal; a high `failed`
count on a developer machine may just be your own edit loop. Those failures
also move the test to the front of `--order fail-fast`. To keep a scratch
history apart from the one you care about, point `RSTEST_CACHE` at another
directory for that session.

History **ages out**. A test with no flake or failure inside the
retention window (default **90 days**, from `last_epoch`) reads as fixed:
every reader ignores its entry, so it stops carrying the annotation below,
and a test you actually fixed goes quiet on its own while the ranked
candidate list stays current instead of haunted by last quarter's
offenders. The stale entry stays in the file until the next run that
records a flake or failure (or a `--cache-pull` merge) rewrites it; a
green run writes nothing, so it prunes nothing. Tune with
`RSTEST_FLAKE_RETENTION_DAYS`; set it to `0` to keep history forever. A
value that is not a whole number of days silently falls back to 90.

Everything that reads the history applies the same window:

- the **run annotations** in the flaky and quarantined sections (below);
- [`--reruns-only-known-flaky`](#target-reruns-only-known-flaky), which
  retries only tests with a `flaky > 0` record;
- [`--order fail-fast`](../reference/cli.md#-order-throughputfail-fast),
  which dispatches recently failed and flaky tests first;
- [`rstest explain`](../reference/cli-commands.md#explain),
  which reports a test's flake and failure counts.

The per-test run history behind
[`--auto-quarantine`](#let-the-history-decide-auto-quarantine) (below) is a
separate file, `.rstest_cache/history.msgpack`, that keeps each test's last
50 runs whether they passed or not. It drops runs older than the same
retention window.

The flaky and quarantined sections annotate each test from this file:

```text
=========== flaky tests (passed after rerun) ===========
  tests/test_ws.py::test_reconnect  (1 rerun; flaked 7x before, failed 2x)
```

A test with `flaky: 7` is not "unlucky": it's the ranked candidate
list for the next step.

## Target: `--reruns-only-known-flaky`

By default `--reruns N` retries *every* failure. That backfires on a
deterministic mass-failure (one broken migration or import failing 50
tests identically) where each retry just re-fails for zero recovery, at
50× the wall-time cost.

`--reruns-only-known-flaky` spends the budget only on tests the history
already knows are flaky (a `flaky > 0` record):

```console
$ rstest -n auto --reruns 2 --reruns-only-known-flaky
```

A hard-failure-only record (`failed > 0`, `flaky == 0`), the signature of
a deterministic failure, does *not* qualify, so mass-failures fail fast
while genuine known-flakes are still rescued. An explicit
`@pytest.mark.flaky` always bypasses the gate (the author already declared
it), and it composes with `--only-rerun`.

The catch: this flag *spends* flaky history; it does not *build* it. The
gate suppresses the very rerun that would record a new flake as `flaky > 0`,
so a flagged run can never learn a brand-new flake on its own. Seed the
history with a separate learning run (plain `--reruns` **without** this
flag, e.g. nightly or pre-merge) that lets unknown failures rerun and
records the ones that recover; then run the hot path with the flag to spend
budget only on what that history knows. Persist `.rstest_cache` across CI
runs so the history survives. Full semantics:
[`--reruns-only-known-flaky`](../reference/cli.md#-reruns-only-known-flaky).

## Ring-fence: `--quarantine`

Write the known offenders to a file (commit it: the quarantine set is
a team decision and its diff history is the audit trail):

```text
# quarantine.txt: tracked in JIRA-1234; remove entries when fixed
tests/test_ws.py::test_reconnect
tests/test_legacy_sync.py::*  # JIRA-1301
```

One nodeid or `*` glob per line; `#` comments (whole-line or trailing,
after whitespace) and blank lines are skipped. Then:

```console
$ rstest -n auto --quarantine quarantine.txt
```

A **failure** matching the list is demoted to a `quarantined` outcome, and
with no other failure the run exits 0:

```text
=========== quarantined failures (known-flaky, non-fatal) ===========

--- QUARANTINED tests/test_ws.py::test_reconnect ---  (flaked 7x, failed 2x before)
ConnectionResetError: [Errno 54] Connection reset by peer
...

41 passed, 1 quarantined in 12.31s
```

The exact semantics:

- **Failures outside the list still fail the run.** Quarantine never
  becomes a blanket mute: a new failure exits 1 as always.
- A run whose only failures are quarantined **exits 0**. Exit codes
  ≥ 2 (usage/internal errors) are never touched.
- A listed test that **passes** is a plain pass, no penalty for
  being on the list on a good day.
- The traceback still prints, in its own section. A quarantined test
  is a tracked liability, not an invisible one.
- `--lf` still reruns quarantined failures: locally they behave like
  the failures they are.

## Let the history decide: `--auto-quarantine`

You don't have to curate the list by hand. Every run reads the per-test run
history (`.rstest_cache/history.msgpack`, the last 50 runs of each test; see
[`rstest explain`](../reference/cli-commands.md#explain)) and quarantines the
tests that keep flipping between pass and fail. It is on by default, and the
run says which tests it caught before any test starts:

```console
$ rstest -n auto --reruns 2
rstest: --auto-quarantine: 2 test(s) flipped pass/fail 3+ times in recent history:
  tests/test_ws.py::test_reconnect
  tests/test_poll.py::test_eventually
```

A **flip** is a failure right after a passing run, or a run that passed only
on a rerun. Skipped runs are stepped over. A test with 3 or more flips in its
stored history is auto-quarantined:

| History (oldest first) | Flips | Auto-quarantined at the default 3 |
|---|---|---|
| `✓✗✓✗✓✗` | 3 | yes |
| `✓~✓~✓~` (`~` = passed on rerun) | 3 | yes |
| `✓✓✓✗✗✗` (broke and stays broken) | 1 | no: a real regression still fails the run |
| `✗✗✗✗` (never passed) | 0 | no |

Everything else matches `--quarantine`: a failure of an auto-quarantined test
is demoted to `quarantined` (same section, counts, JUnit property and exit
code), a pass is a plain pass, `--reruns` still retries it first, and it never
counts toward `-x` / `--maxfail`. `rstest explain <nodeid>` shows a test's
strip, its flip count, and whether the next run auto-quarantines it:

```text
  history    ✓✗✓✗✓✗ (6 runs, oldest first)
             ...
             flipped 3x: auto-quarantined on the next run (at 3+ flips)
```

**Release is automatic.** The set is recomputed every run, so a fixed test
drops out once enough of its flips leave the history window (the last
`RSTEST_HISTORY_RUNS` runs, default 50, and no older than
`RSTEST_FLAKE_RETENTION_DAYS`). Nothing to edit or remove.

**Cap.** When more auto-quarantined tests fail in one run than 5% of the
tests in that run (rounded up, at least one), rstest quarantines none of them
for that run and warns:

```text
rstest: --auto-quarantine: 14 auto-quarantined test(s) failed, over the cap of 10 (5% of 200 tests in this run); quarantining none of them, the environment looks broken (raise the cap with --auto-quarantine-max=PCT)
```

Many flappers failing at once points to a broken environment (a database
down, a network outage), and that run should be red. Tests from the
`--quarantine` file are still quarantined and don't count toward the cap.

**Tuning.** `--auto-quarantine=5` raises the threshold, `--auto-quarantine=0`
turns it off, and `--auto-quarantine-max=PCT` moves the cap. Attach values
with `=`. Commit the policy in `pyproject.toml` instead of repeating flags:

```toml
[tool.rstest]
quarantine = "quarantine.txt"   # relative to this pyproject.toml
auto-quarantine = 3
auto-quarantine-max = 5
```

**Composing with the file.** The run quarantines both sets. Use the file for
tests the team has decided to tolerate (with an owner and a tracking issue),
and the auto set as the safety net for flakes nobody has triaged yet.

**See the whole suite.** `rstest flaky` ranks every test that flipped, with
its recent outcome strip, whether it is getting worse, and which ones the
next run will auto-quarantine
([`rstest flaky`](../reference/cli-commands.md#flaky)). `--json` feeds a
dashboard, and `rstest report` writes an HTML page with the same ranking plus
failing and flaky tests per run
([`rstest report`](../reference/cli-commands.md#report)).

**On CI**, persist `.rstest_cache` between runs (see
[CI quickstart](ci-quickstart.md)), or share it with
[`--cache-pull` / `--cache-push`](ci-shared-cache.md), which carries the run
history between shards and runners. A runner that starts with no history has
an empty auto set, so auto-quarantine does nothing there.

## Find hidden network calls

A test that quietly calls a real service flakes whenever that service is slow,
rate-limits CI, or is down, and no amount of reruns fixes it. `--doctor` lists
every test's outbound connections
([NETWORK ACCESS](doctor.md#network-access)), and
[pytest-socket](https://github.com/miketheman/pytest-socket) fails them:

```console
$ rstest -n auto --doctor                      # see what the suite reaches
$ rstest -n auto -- \
    --allow-hosts=127.0.0.1,::1,10.0.0.0/8     # then keep it offline
```

Mock the call, mark a test that needs the network on purpose with
pytest-socket's `@pytest.mark.allow_hosts([...])`, or allow intended hosts
(CI service containers) in `--allow-hosts` and `[tool.rstest] network-allow`.

## What CI consumers see

- **Terminal / summary line**: a separate `N quarantined` count; the
  run summary is green when nothing outside the quarantine set failed.
  The auto set and any cap warning go to stderr, so the CI log records
  what was quarantined and why.
- **HTML** (`--html`): auto-quarantined tests carry an `auto` badge, every
  test shows its earlier runs as a strip, and the header counts the auto set.
- **JUnit** (`--junitxml`): the testcase carries a
  `quarantined="true"` property and **no `<failure>` element**, so
  JUnit-gating CI (and dashboards that count failures) stays green
  while still being able to track the quarantine set.
- **`--report-json`**: per-test `"quarantined": true` plus
  a `quarantined` key in `meta.counts`. See
  [Run snapshot](../reference/report-json.md).
- **Monorepos**: pass one file at the root; it's forwarded to every
  project as an absolute path. Patterns match each project's
  **project-relative** nodeids (the same nodeids the child prints).
  `--auto-quarantine` and `--auto-quarantine-max` are forwarded too, and
  each project reads its own history.

## Quarantine vs `--reruns`

| | `--reruns` | `--auto-quarantine` | `--quarantine` |
|---|---|---|---|
| Scope | one run | policy across runs, from history | policy across runs, from a file |
| Covers | intermittent failures that pass on retry | tests that keep flipping, triaged or not | tests the team decided to tolerate, flipping or failing |
| Cost | retries burn suite time every run | none, just accounting | none, just accounting |
| Who decides | nobody | the history (threshold and cap) | the team, in review |
| Visibility | flaky section + property | quarantined section + property; set printed on stderr | quarantined section + property; committed list |
| Release | n/a | automatic, as flips age out | remove the line |
| New failures | still fail | still fail | still fail |

They compose: `--reruns 2 --quarantine quarantine.txt` retries
everything, and only quarantined tests (from the file or the history) may
fail without reddening the run.

## Lifecycle

1. `--reruns` keeps the suite green; both histories record who needed it.
2. A test that keeps flipping is auto-quarantined. Its failures stop
   reddening the run but still print, so it shows up in every run log.
3. When a test needs an owner, add it to `quarantine.txt` with a comment
   linking the tracking issue. This also keeps it quarantined if it starts
   failing every run, which the auto set deliberately does not.
4. Fix the test; remove the entry. The auto set releases it on its own. If
   it was really fixed, the history stops accruing and ages out after the
   retention window: if it comes back to the auto set, it wasn't.

Before quarantining a test that fails only in some orders or only in
parallel, check whether it is flaky at all or order-dependent: an order
dependency has a fix, and quarantine is for real nondeterminism. See
[Diagnosing a parallel-only failure](parallel-safety.md#diagnosing-a-parallel-only-failure).
The auto set can't tell the two apart, so an order-dependent test that flips
gets auto-quarantined too; `rstest bisect` finds the polluter.

The failure mode to avoid is a quarantine list that only ever grows.
`flakes.json` and the run history self-age (see
[aging](#remember-the-flake-history) above), but `quarantine.txt` is
committed and hand-curated on purpose. Treat an addition like a TODO with an
owner, and audit the list periodically. An entry whose test no longer
appears in the flake history is either fixed (remove it) or abandoned (fix
the test). The auto set is printed every run, and `rstest flaky` lists it with
trends: a test that stays on it for weeks needs an owner too.
