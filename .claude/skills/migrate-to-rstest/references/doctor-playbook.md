# Doctor playbook — the speed action for every `--doctor` finding

`rstest --doctor` runs the suite once and reports where time goes, from data it
already collects (per-test wall vs. cpu, per-fixture setup, per-file totals).
For each section it prints, this is what it means and what to do — ordered by
how much speedup it usually buys. `--doctor-json` writes the same as a versioned
doc for CI trending.

The rule of thumb: **the biggest win is almost always wait-bound tests, then the
parallel-floor gate test.** Runner flags (`-n`, `--dist`, `--collect`) are
secondary — they rebalance work, they don't remove it.

## WAIT-BOUND — the usual #1 win

A test whose **wall time ≫ cpu time** isn't computing — it's sleeping, waiting
on a socket, or waiting out a timeout. No worker count makes a sleep faster.
This is the dominant pattern in real suites (rich: 74% of test time in three
`sleep()` tests; aiohttp: 95% waiting on proxy timeouts).

**Action:** fix the wait, in order of preference —
- mock the clock (`freezegun`, fake `time.monotonic`) or patch `time.sleep`;
- shrink the timeout the test waits out, or make the wait event-driven;
- if it's a genuine external wait, mark it so it doesn't gate everything.

This is also the lane-A WALL-CLOCK fix — a wait-bound test that *also* fails
under load is the same test; fixing the wait solves both.

## PARALLEL FLOOR — raises the ceiling

No `-n` can finish faster than the single longest test. If the longest test
exceeds the ideal per-worker share, the report names the **gate test(s)**.

**Action:** split the gate test into smaller independent tests, or shrink what
it does (often it's also wait-bound — fix that first). Until the gate test
shrinks, adding workers past the floor buys nothing.

## FIXTURE HOTSPOTS — cheap, high-leverage

Total setup time per fixture, with scope advice:
- A **function-scoped** fixture that ran hundreds of times and costs real time →
  widen its scope (`module`/`session`) if its result is reusable. (One suite
  re-parsed the same RSA key 206×, ~20% of runtime, for want of a session
  fixture.)
- A **session-scoped** fixture that ran **more than once** ran once *per worker*
  — confirm it's safe to duplicate (unique ports/dirs/db names); this overlaps
  with lane-A's fixed-resource check.

**Action:** widen scope where the value is reusable; make duplicated session
fixtures parallel-safe (`bind 0`, per-worker names).

## SLOWEST FILES — where to look first

Test time aggregated by file. Not a fix on its own — it's the map: it tells you
which file holds the wait-bound/gate tests above, and it's the input for `--dist
load` balancing decisions.

**Action:** open the top file, find the wait-bound / gate tests inside it, fix
those. If one file is a huge share and its tests are independent, the default
`--dist load` already splits it across workers; `--dist loadfile` would *not*
(it pins the file to one worker) — only use loadfile when the file is
order-dependent.

## Runner tuning (secondary)

Only after the above:
- `-n` — more workers help only up to the parallel floor and the core count.
- `--collect lazy` — speeds up narrow `-k`/`-m` selections on large suites (no
  per-worker full collection); full runs of a few giant files prefer `--collect
  full` (or `lazy` + explicit `--dist load` for work-stealing).
- `--dist load` (default, duration-aware) is usually best; `loadfile`/`loadscope`
  trade balance for affinity (order-dependent suites, expensive shared fixtures).

See `flag-map.md` for the `[tool.rstest]` keys to make any of these the default.
