# Wait-bound / IO suite playbook

A playbook for suites whose time goes to waiting (sleeps, network, timeouts) rather than computing, where rstest gains the most.

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
  sleeping or waiting on IO/a timeout. Doctor prints the share of test
  time spent waiting and names the worst offenders. In one real suite
  ([aiohttp]) this was **95% of test time (176.5s) waiting**, almost all
  of it on 10-second proxy timeouts.
- **PARALLEL FLOOR**: no worker count can finish faster than the longest
  single test. If your longest test exceeds the ideal per-worker share,
  doctor names the **gate tests**; splitting or shrinking them is the only
  way to raise the ceiling (adding workers won't).

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

- **`-n auto` only ever caps *downward*.** It starts at one worker per
  available logical core, then caps by what the suite can use (test-file
  count and cached total runtime). It never exceeds your core count, and
  on a few-file suite it may settle *below* it. See
  [`-n, --numprocesses`](../reference/cli.md#-n-numprocesses-nauto).
- **An explicit `-n N` is a fixed count, regardless.** Pin `-n <k>` and
  you get exactly `k` workers; the auto cap does not apply. So a
  wait-bound suite can set `-n` **above** the logical core count to keep
  more waits in flight at once, since a waiting worker isn't using a core.

This is the same effect doctor reports as **PARALLEL EFFICIENCY over
100%**: "overlapping sleeps/IO run more tests at once than there are
cores." Doctor flags efficiency above 100% as normal for wait-bound
suites and points you back at WAIT-BOUND. (The section is `-n > 1` only.)
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

> Caveat: this over-subscription trick is safe *because* the work is
> waiting, not computing. Tests that assert on rate-limit windows, token
> expiries, or tight elapsed-time bounds can degrade under high `-n`;
> that's load, not ordering. Contain them with `@pytest.mark.serial`, a
> clock mock, or a capped `-n`; see
> [Parallel safety](parallel-safety.md).

**Arm a hang backstop.** A wait-bound suite is exactly the one that hits a
real hang: a socket that never returns, a timeout that never fires. Set
[`--worker-timeout`](../reference/cli.md#-worker-timeout-secs): a test that
exceeds it is reported **failed** with a timeout message, its worker is killed
and replaced, and that worker's remaining tests redistribute so the run still
finishes. Concurrency exposes these; without a backstop one hung test can
stall a whole worker for the length of the run. (A per-test cap, `@pytest.mark.timeout`,
is the finer-grained tool; see [Markers](../reference/markers.md).)

## 3. Per-worker isolation (DB / ports)

More workers means more concurrent copies of every shared resource. A
session-scoped fixture runs **once per worker**, not once per run: N
workers means N databases, N servers, N bound ports
([Parallel safety](parallel-safety.md)). So each worker's resources must
be safe to duplicate:

- **Ports:** bind port `0` (let the OS assign) instead of a fixed port.
- **Databases / directories:** derive a per-worker name or path.

Tests and fixtures read their worker identity from the environment:

```python
import os

worker = os.environ.get("RSTEST_WORKER_ID")  # "gw0", "gw1", ...; unset at -n 0/1 (unless --reruns)
```

Plugins that check pytest-xdist's `workerinput` get the same answer via
`request.config.workerinput["workerid"]`; that path works under both
runners. Full contract:
[Parallel safety](parallel-safety.md) and
[Environment variables](../reference/environment.md).

**Django is handled for you.** rstest announces each worker exactly like
an xdist worker (`gw0`, `gw1`, …), so pytest-django suffixes the test
database per worker automatically (`test_app_gw0`, `test_app_gw1`, …),
with no extra flags. rstest's own corpus only exercises pytest-django on
SQLite `:memory:`, where each process has a private database anyway, so
confirm the suffixing on your Postgres or MySQL setup with one parallel run.
See the
[Django on ephemeral CI worked example](ci-quickstart.md#worked-example-django-on-ephemeral-ci).
The general rule holds for anything else: key the resource on
`RSTEST_WORKER_ID` (or `workerinput`) so N workers don't collide.

`rstest --doctor` prints a warning for every session fixture that ran more
than once, with this exact caveat, a quick way to spot resources you
haven't made per-worker-safe yet.

## 4. Measure the real win

Before committing to anything, get *your* numbers with
[`rstest try`](migrate-from-pytest.md):

```console
$ rstest try
```

It runs your suite once under plain `pytest` and once under
`rstest -n auto`, then reports whether outcomes are **identical** and how
much **faster** rstest is. No flags, no config. (It needs `pytest`
installed on its own for the baseline.) Note that `try` uses `-n auto`.
For a wait-bound suite your *real* ceiling is higher, so treat the `try`
speedup as a floor and then run the `-n`-above-cores tuning loop from
section 2 to find the actual best wall time.

## Go deeper

- [Suite diagnostics](doctor.md): every doctor section (WAIT-BOUND,
  PARALLEL FLOOR, PARALLEL EFFICIENCY, fixture hotspots, resource leaks)
  and the JSON/markdown/CI-gate outputs.
- [Scheduling](../concepts/scheduling.md): slow-tests-first dispatch and
  why it beats file-affinity schedulers on wait-heavy suites.
- [Parallel safety](parallel-safety.md): per-worker isolation, the
  `@pytest.mark.serial` escape hatch, and time-sensitive tests at high
  concurrency.
- [CLI reference](../reference/cli.md): `-n`, `--dist`, `--timeout`,
  `--worker-timeout`, and the doctor flags.
- [Migrating from pytest](migrate-from-pytest.md): the one-line switch
  and the `-n 0` byte-exact escape hatch.
- [Environment variables](../reference/environment.md): the full
  `RSTEST_WORKER_ID` / `workerinput` contract.
