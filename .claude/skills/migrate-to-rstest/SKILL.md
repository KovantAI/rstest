---
name: migrate-to-rstest
description: >-
  Adopt and tune rstest, the fast, parallel, pytest-compatible Python test
  runner. Two jobs: (1) MIGRATE/READINESS: switch a suite or its CI from pytest
  to rstest, check if it's parallel-safe, or fix what blocks the first parallel
  run ("-n auto fails but plain pytest passes", "workers collected different
  test sets", shared state, leaked globals, fixed ports); (2) SPEED:
  make a slow suite faster by finding the slowest tests/files, the tests that
  cap parallelism, wait-bound (sleep/timeout) tests, and expensive fixtures. Trigger
  whenever the user wants to adopt, configure or speed up rstest, OR (even
  without "rstest") wants to
  run pytest tests in parallel / across workers, asks if their suite is
  parallel-safe or "rstest-ready", wants to make a slow test suite faster / cut
  CI test time, or asks which tests are slowest or where test time goes. This is
  a multi-step workflow with real judgment calls; use it rather than ad-hoc
  running a command. Do NOT use for ordinary single-process pytest work
  (writing/running tests, converting unittest, fixing fixtures/conftest,
  version-upgrade breakage, mutation/test-quality), for making application
  code thread-safe, or for triaging a red CI run or flaky test on a suite that
  already runs on rstest (use rstest-triage).
---

# Adopt & tune rstest

rstest runs an existing pytest suite **in parallel by default**, byte-exact to
pytest at `-n 0`. Two reasons people come to it; handle whichever the user
needs (often both):

- **Readiness / migration**: get the suite green under parallel execution and
  switch over (incl. CI). The work is finding the few tests that aren't
  *parallel-safe* and fixing them.
- **Speed**: a suite is slow; find where the time actually goes and cut it.

Both lanes are driven by a self-documenting rstest command that does the
analysis; this skill is for the judgment around it (which fix vs. stopgap, what
to leave alone, what to change). Read the matching reference when you reach it:

- `references/fix-playbook.md`: the fix for every `migrate-check` verdict.
- `references/doctor-playbook.md`: the speed action for every `--doctor` finding.
- `references/flag-map.md`: pytest/pytest-xdist → rstest flags, `[tool.rstest]`,
  and the CI setup (GitHub Action, sharding, shared cache).

Once the suite is on rstest, later failures (a red CI run that passes locally,
a polluter, a recurring flake) belong to the `rstest-triage` skill, which
drives `replay`, `bisect`, `audit` and `explain`.

Install rstest into the project's environment first (`uv pip install rstest`).
It runs its own vendored pytest 9.1.1 core, not the installed pytest, but it
loads the project's conftest and installed plugins. A suite pinned to pytest 8
may need the upgrade steps in `docs/guides/upgrade-to-pytest9.md`.

---

## Lane A: readiness / migration

1. **Baseline + contract.** Quickest: `rstest try` runs the suite under both
   pytest and `rstest -n auto` and reports parity (identical outcomes?) plus the
   speedup: the whole "is it safe and worth it" answer in one command. (Or do
   it by hand: record `pytest` pass/fail + wall time, then confirm `rstest -n 0`
   matches pytest exactly.) A parity diff is either an unstable id or a real
   compatibility issue. Surface it, don't paper over it; `migrate-check`
   classifies it.
2. **Preflight + fix.** `rstest migrate-check --migrate-check-json findings.json`. Work each
   finding via `fix-playbook.md`; re-run until it reports **ready**. (It blocks
   on unstable-id "WILL bail" findings before the parallel phase can even run;
   fix those first.)
3. **Configure + CI.** `[tool.rstest]` defaults; swap the CI command to `rstest`
   (or the bundled GitHub Action), persist `.rstest_cache` so scheduling and
   flake history carry over, upload the replay journal on failure, and add a
   `--migrate-check-json` gate (`flag-map.md`).
4. **Verify.** `rstest` green, outcomes match the pytest baseline, report the
   speedup.

### Judgment calls (where you earn your keep)

- **Prefer the upstream fix over the stopgap.** Stable `ids=`, a reset fixture,
  an ephemeral port (`bind 0`), a mocked clock *remove* the problem; `-n 0` /
  `-n 4` / `@pytest.mark.serial` only cap parallelism. Offer both, recommend the
  fix.
- **Leave pre-existing failures alone.** Anything `migrate-check` calls
  NOT PARALLEL-SPECIFIC or INTRINSIC FLAKE was already failing under pytest, not
  rstest's to fix. Report the count; don't delete/skip it to force green.
  Conflating "already red" with "rstest broke it" is the #1 migration mistake.
- **Ask before editing** tests or CI: show the diff and the why. (Non-
  interactive: default to the upstream fix and say so.)
- **Gate CI on *new* issues**, tolerate triaged ones with
  `--migrate-allow <substring>`.
- **`migrate-check` or `audit`?** `migrate-check` is the onboarding preflight
  (unstable ids first, then classification). Once it's clean, `rstest audit`
  is the faster repeat check and prints a paste-ready `@pytest.mark.serial`
  block for the isolation and wall-clock failures.

---

## Lane B: speed

1. **Diagnose.** `rstest --doctor` (or `--doctor-json` for a machine-readable,
   versioned report). It runs the suite once and reports, from data it already
   has: **slowest files**, **wait-bound** tests (wall ≫ cpu: sleeping/waiting),
   the **parallel floor** (the longest test caps any worker count), and
   **fixture hotspots** (setup time, with scope advice).
2. **Act per finding** using `references/doctor-playbook.md`. The biggest wins
   are almost always wait-bound tests (mock the clock / shrink timeouts) and the
   parallel-floor gate test (split it). Tuning `-n`/`--dist`/`collect`/
   `--fork-pool` is secondary; the report tells you when it helps. Running
   fewer tests (`--changed`, `--since-green`, `--incremental`) is its own lever
   for local loops and PR jobs.
3. **Re-measure.** Compare wall time before/after; report the delta.
4. **Keep it fast.** Offer CI gates so the win doesn't erode:
   `--durations-regress 2` (fail when a test doubles its cached time) and
   `--doctor-fail-on 'parallel_efficiency<30'` (see `doctor-playbook.md`).

Note: the *parallel-safety* findings ("which tests aren't parallel-safe", "why
does -n auto fail") belong to Lane A's `migrate-check`, not `--doctor`;
`--doctor` is purely about where time goes.

---

A good outcome: green at `-n auto`, parity with pytest preserved, measurably
faster, CI gated, and the edits are ones the upstream maintainers would accept,
not just things that quiet the runner.
