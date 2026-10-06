# Wait-bound / IO suites

A playbook for suites whose time goes to waiting (sleeps, network,
timeouts) rather than computing, where rstest gains the most.

## Who this is for

You maintain a backend suite that spends most of its time *waiting* (on
sockets, HTTP calls, database round-trips, `sleep()`s, and timeouts), not
computing. This is rstest's biggest-win case: waits overlap, so more tests
can be in flight at once than you have cores.

**One-line self-check:** run `rstest --doctor`. If it prints a
**WAIT-BOUND** line, this page is for you.

## 1. Confirm you're wait-bound

[`rstest --doctor`](doctor.md) runs your suite once and reports where the
time actually goes, from timing the runner already owns (per-test wall
time, per-test CPU time, per-fixture setup). The two sections that matter
here:

- **WAIT-BOUND**: compares each test's wall time against its CPU time. A
  test whose wall time vastly exceeds its CPU time isn't computing; it's
  sleeping or waiting on IO/a timeout. Both cover the whole test,
  fixture setup and teardown included, and on Linux and macOS CPU time
  includes child processes the test waited for, so a test that runs a
  CPU-heavy CLI through `subprocess.run` counts as computing, not waiting
  (oversubscribing `-n` will not help it). On Windows such a test counts
  as waiting ([no child CPU time](windows.md#diagnostics)). Doctor prints the share of
  test time spent waiting and names the worst offenders. In one real suite
  ([aiohttp]) this was **95% of test time (176.5s) waiting**, almost all
  of it on 10-second proxy timeouts.
- **PARALLEL FLOOR**: no worker count can finish faster than the longest
  single test. If your longest test exceeds the ideal per-worker share (at
  least 1 second) by more than 10%, doctor names the
  [gate tests](../concepts/glossary.md#gate-test); splitting or shrinking them is the only
  way to lower that floor (adding workers won't).

```console
$ rstest --doctor
```

If WAIT-BOUND dominates, the fastest lever is often to *fix the waits*
(mock the clock, shrink the timeout, use event-driven waits). Doctor
calls this out as usually the single biggest speedup in a suite. But even
without touching a test, you can overlap the existing waits harder by
tuning the worker count (next section).

[aiohttp]: https://github.com/aio-libs/aiohttp

## 2. Tune the worker count

Here is the key move for a wait-bound suite, and it is counter-intuitive:

- **`-n auto` only ever caps *downward*.** It never exceeds your logical
  core count, and on a small or few-file suite it settles below it
  ([how it sizes the pool](../getting-started/first-steps.md#controlling-parallelism)).
- **An explicit `-n N` is a fixed count, regardless.** Pin `-n <k>` and
  you get exactly `k` workers; the auto cap does not apply. So a
  wait-bound suite can set `-n` **above** the logical core count to keep
  more waits in flight at once, since a waiting worker isn't using a core.

Doctor's **PARALLEL EFFICIENCY** section shows this as a realized speedup
above the logical core count (for example `21.0x realized of 32x possible`
on a 14-core machine): overlapping sleeps and IO run more tests at once than
there are cores. The efficiency percentage is relative to the worker count,
so it stays at or below 100%. (The section is `-n ≥ 2` only.)
The [scheduler](../concepts/scheduling.md) helps here too: it dispatches
slow tests first (a cached duration of 1s or more), longest first, so a
54-second waiter starts at t=0 instead of stacking behind other work.

**The tuning loop.** Raise `-n` past your core count and watch two doctor
numbers until they flatten:

```console
$ rstest -n 16 --doctor      # start above cores for an IO-heavy suite
$ rstest -n 24 --doctor      # keep raising while wall time keeps dropping
$ rstest -n 32 --doctor      # stop when wall time flattens / stops improving
```

Each run, read: **wall time** (the header line, `wall Xs`) and the
**WAIT-BOUND %**. Keep raising `-n` while wall time keeps falling; stop
when it flattens. Two things put a floor under it, both named by doctor:
the **PARALLEL FLOOR** long pole (a single test no `-n` can beat), and the
point where you've run out of overlappable waiting. Once tuned, you can
guard the *outcome* in CI against regressions with a doctor metric that
tracks how well the run parallelized, e.g. `--doctor-fail-on
'parallel_efficiency<N'` or a `wall_seconds` ceiling
([`--doctor-fail-on`](../reference/cli.md#-doctor-fail-on-cond)). (Don't
gate on `wait_pct` for this; it measures how wait-bound the suite *is*,
not whether the worker count is well-tuned.)

!!! warning "Timing-sensitive tests degrade under high `-n`"
    This oversubscription trick is safe *because* the work is waiting, not
    computing. Tests that assert on rate-limit windows, token expiries, or
    tight elapsed-time bounds can degrade under high `-n`; that's load, not
    ordering. Contain them with `@pytest.mark.serial`, a clock mock, or a
    capped `-n`; see [Parallel safety](parallel-safety.md).

**Arm a hang backstop.** A wait-bound suite is exactly the one that hits a
real hang: a socket that never returns, a timeout that never fires. Set
[`--worker-timeout`](../reference/cli.md#-worker-timeout-secs): a test that
exceeds it is reported **failed** with a timeout message, its worker is killed
and replaced, and that worker's remaining tests redistribute so the run still
finishes. Concurrency exposes these; without a backstop one hung test can
stall a whole worker for the length of the run. A per-test cap,
`@pytest.mark.timeout`, is the finer-grained tool (see
[Markers](../reference/markers.md)).

An explicit `--worker-timeout` replaces the per-test hang watchdogs that
`--timeout` and `@pytest.mark.timeout` arm (3× the test's timeout + 10s) with
one fixed limit for every test, so set it above your longest
`mark.timeout`, or that test is killed before its own timeout fires. See
[Hung tests](../concepts/crash-handling.md#hung-tests-worker-timeout).

## 3. Per-worker isolation (DB / ports)

More workers means more concurrent copies of every shared resource: a
session-scoped fixture runs once per worker, so N workers means N databases,
N servers, N bound ports
([Session-scoped fixtures duplicate](parallel-safety.md#session-scoped-fixtures-duplicate)).
Each worker's resources must be safe to duplicate:

- **Ports:** bind port `0` (let the OS assign) instead of a fixed port.
- **Databases / directories:** derive a per-worker name or path from
  `RSTEST_WORKER_ID` (`gw0`, `gw1`, ...) or xdist's `workerinput`; see
  [Worker identity](parallel-safety.md#worker-identity).

pytest-django suffixes the test database per worker automatically. As you
raise `-n`, `rstest --doctor` flags the expensive session fixtures that ran
once per worker, so you can spot the ones that aren't per-worker-safe yet.

## 4. Measure the real win

Before committing to anything, get *your* numbers with
[`rstest try`](../reference/cli-commands.md#try):

```console
$ rstest try
```

It runs your suite once under plain `pytest` and once under
`rstest -n auto`, then reports whether outcomes are **identical** and how
much **faster** rstest is. The baseline needs pytest installed in the
project environment (`python -m pytest` must work), not just rstest.
Because `try` uses `-n auto`, a wait-bound suite's *real* ceiling is
higher, so treat the `try` speedup as a floor and then run the
`-n`-above-cores tuning loop from section 2 to find the actual best wall
time.

## Go deeper

- [Suite diagnostics](doctor.md): every doctor section (WAIT-BOUND,
  PARALLEL FLOOR, PARALLEL EFFICIENCY, fixture hotspots, resource leaks)
  and the JSON/markdown/CI-gate outputs.
- [Scheduling](../concepts/scheduling.md): slow-tests-first dispatch and
  why it beats collection-order schedulers on wait-heavy suites.
- [Parallel safety](parallel-safety.md): per-worker isolation, the
  `@pytest.mark.serial` escape hatch, and time-sensitive tests at high
  concurrency.
- [CLI reference](../reference/cli.md): `-n`, `--dist`, `--timeout`,
  `--worker-timeout`, and the doctor flags.
- [Migrating from pytest](migrate-from-pytest.md): the one-line switch
  and the `-n 0` single-worker escape hatch.
- [Environment variables](../reference/environment.md): the full
  `RSTEST_WORKER_ID` / `workerinput` contract.
