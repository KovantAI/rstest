---
name: rstest-triage
description: >-
  Debug and triage test failures on a suite that already runs on rstest: a CI
  run went red but the tests pass locally, a test fails only in parallel or only
  after some other test, a flaky test keeps coming back, a worker crashed or
  hung, or the team needs a policy for known-flaky tests. Drives rstest's
  diagnostic commands (`rstest replay` to re-run a CI schedule locally from its
  journal, `rstest bisect` to find the polluting test, `rstest audit` to sweep
  for parallel-only failures, `rstest explain` for one test's history) and the
  flaky-test controls (`--reruns`, `--reruns-only-known-flaky`, `--quarantine`,
  `--shuffle`). Use it whenever the user has an rstest failure to explain or fix,
  mentions a replay journal, `latest.json`, a polluter, order dependence,
  `flakes.json`, quarantine, or asks "why does this only fail on CI / in
  parallel", even if they don't name the command. Not for first-time migration
  from pytest or for speed tuning (use migrate-to-rstest), and not for plain
  test failures that also fail under ordinary pytest.
---

# Triage rstest failures

rstest owns dispatch (which worker runs which test, in what order), so unlike
pytest-xdist it can record a schedule, pin it back, and reason about ordering.
That makes most "only fails sometimes" problems reproducible. The job here is
to pick the right tool for what the user actually has, get to a reproduction,
name the cause, and recommend a fix that removes the problem rather than one
that only hides it.

Exact flags, exit codes and gotchas for every command are in
`references/commands.md`. Read it before running a command you haven't used
in this session: several have sharp edges (argument placement, `=` for seeds,
options that are refused) that cost a round trip if you guess.

## Step 1: classify before reaching for tools

Ask, or find out, one thing first: **does the test fail on its own at `-n 0`?**

```console
$ rstest -n 0 "tests/test_x.py::test_y"
```

- **Fails alone** → a plain bug or an environment gap. No rstest tool will
  explain it better than the traceback. Debug it as an ordinary test failure.
- **Passes alone** → the failure depends on context: what ran before it, what
  ran next to it, or machine load. Continue.

This matters because every tool below assumes the test is correct in isolation.
Running `bisect` on a test that fails alone just reports "not order-dependent"
after spending runs to find out.

## Step 2: pick the tool by what the user has

| The user has... | Start with | Why |
|---|---|---|
| A red CI run that passes locally | `rstest replay --journal <latest.json>` | Pins the exact per-worker assignment and order CI used; recreating it by hand is guesswork |
| One victim test that fails after others | `rstest bisect <nodeid>` | Delta-debugs the predecessor set at `-n 0` down to the minimal polluter(s), prints a repro command |
| "Which of our tests aren't parallel-safe?" | `rstest audit` (add `--audit-repeat 5` for races) | Sweeps the suite at `-n auto` against a serial oracle, classifies each parallel-only failure, emits a paste-ready `serial` block |
| A suspicion of order dependence, no victim yet | `rstest --shuffle` | A seeded random order flushes it out; the printed seed reproduces it |
| "Is this test flaky, and how often?" | `rstest explain <nodeid>` | Reads the caches (duration, flake/fail counts, last outcome, coverage) without running anything |
| A hang (run never finishes, or a worker stalls) | `--timeout SECS` on the command line, plus `--stream-json` | Fails the stuck test in-process with a traceback at the stuck line and arms a backstop; the stream names the test even if the job is killed. See "Hangs" in `references/commands.md` |
| A crashed worker | the run's own report | rstest names the test that was running, the worker's exit code or signal, and its last stderr lines |

The usual chain for a CI-only failure is **replay → find the polluter → fix**:
replay reproduces the failure locally, the failing test becomes the victim, a
bisect names the polluter, and the fix goes into the polluter or the victim's
setup. Which bisect to use depends on where the polluter sits (next
paragraph).

**Watch for this trap.** `bisect` only searches the tests that come *before*
the victim in collection order. On CI the polluter often ran first only because
the scheduler put it there, while it collects *after* the victim. Bisect then
reports that the failure "does not reproduce from collection order" and
suggests concurrency, which is wrong. When you have a journal, search the
worker's recorded order instead with the bundled script:

```console
$ python <skill-dir>/scripts/journal_bisect.py latest.json "tests/test_report.py::test_totals"
worker gw1: 6 test(s) ran before tests/test_report.py::test_totals
minimal polluter set (2 runs):
  tests/test_zz_debug.py::test_enable_debug
reproduce:
  rstest -n 0 -q -p no:randomly tests/test_zz_debug.py::test_enable_debug tests/test_report.py::test_totals
```

It runs `rstest -n 0` on shrinking subsets of that worker's predecessors, in
their recorded order, and prints the minimal set plus a repro command (`--list`
prints the full ordered command without running anything). Only when the
worker's own order does **not** reproduce is the cause concurrency or load
(two workers sharing a port, file or database, or a timing assertion). Then go
to `audit` or the discriminator runs in `references/commands.md`.

## Step 3: fix, then stopgap only if needed

Prefer the fix that removes the cause. Offer the stopgap when the user can't
change the test now, and say which one it is.

| Cause | Fix | Stopgap |
|---|---|---|
| Polluter leaks state (module global, env var, un-undone monkeypatch, `warnings` filter, registry) | Make the polluter restore state, or give the victim an autouse fixture that resets it | `--dist loadfile` if the pair shares a file |
| Shared resource between workers (fixed port, fixed path, one DB name) | Ephemeral port (`bind 0`), `tmp_path`, per-worker names from `worker_id` | `@pytest.mark.serial`, or `xdist_group` + `--dist loadgroup` |
| Timing assertion missed under load | Mock the clock, assert behavior instead of elapsed time | `-n` lower, or `@pytest.mark.serial` |
| Genuine nondeterminism (race, unseeded random) | Make it deterministic | `--reruns 2`, then quarantine if it persists |

Two judgment calls that come up often:

- **Don't quarantine an order dependency.** It has a deterministic fix, and
  quarantine only hides it while the polluter keeps breaking other tests.
  Quarantine is for real nondeterminism the team has decided to tolerate while
  it's tracked.
- **`@pytest.mark.serial` is a stopgap, not a fix.** It runs the test alone
  after the parallel phase, which caps parallelism. `audit`'s paste-ready block
  is convenient; still name the real fix next to it.

## Step 4: flaky-test policy (when the user asks for one)

The three controls form a lifecycle, and mixing them up is the common mistake:

1. `--reruns N` rescues a flake within one run and records it in
   `.rstest_cache/flakes.json`.
2. `--reruns-only-known-flaky` spends the rerun budget only on tests that
   history already marks flaky, so a mass failure (one broken import failing
   50 tests) fails fast instead of retrying 50 times. It **spends** history but
   never **builds** it, so pair it with a separate learning run (plain
   `--reruns`, nightly or pre-merge) and persist `.rstest_cache` in CI.
3. `--quarantine quarantine.txt` demotes failures of listed tests to a
   non-fatal `quarantined` outcome. The file is committed, so each entry
   should carry a comment linking its tracking issue, **on its own line
   above the entry**: a trailing `# comment` on the same line becomes part of
   the pattern and the entry silently stops matching. New failures outside the
   list still fail the run.

Recommend persisting `.rstest_cache` across CI runs whenever flake history
matters; without it, `explain` and `--reruns-only-known-flaky` have nothing to
read on CI.

Two traps to steer around when writing the policy:

- **Always pass `--reruns N` when relying on `@pytest.mark.flaky`.** With the
  marker alone, a parallel run retries the test and prints `1 flaky` but
  still exits 1 (a known rstest bug), so CI goes red on a green-looking
  summary.
- **The marker's `only_rerun=`, `condition=` and `reruns_delay=` keywords are
  ignored.** To retry only transient errors, use the `--only-rerun REGEX`
  flag, which applies to every rerun in the run.

## Report back

Tell the user, in order: how you classified it (Step 1), the command that
reproduced it and its output, the named cause (polluter nodeid, shared
resource, timing), the recommended fix as a diff, and the stopgap if one
applies. Ask before editing tests, conftest or CI config; in a non-interactive
run, apply the upstream fix and say so.
