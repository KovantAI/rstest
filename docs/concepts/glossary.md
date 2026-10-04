# Glossary

Short definitions of the terms used across these docs, starting with the ones you meet first.

## Everyday terms

Start here: the terms you actually hit first coming from pytest.

**Worker**: a Python process (`gw0`, `gw1`, ...) running your project's
interpreter with the vendored pytest core; executes tests and streams
reports to the orchestrator.

**Orchestrator**: the `rstest` binary, which spawns workers, dispatches
tests, merges results, and renders output.

**`-n` (worker count)**: how many parallel worker processes run your tests.
`-n auto` (the default) uses your cores, but never more workers than the
selected test files on a first run (cached timings lift that to the test
count), and fewer on a suite whose cached run time is only a few seconds;
`-n 4` uses exactly four; `-n 0` (or `-n 1`)
turns parallelism off and runs one plain pytest session. You rarely need to
set it. See [Byte-exact mode](#byte-exact-mode) for what `-n 0` gives you.

**Collection**: pytest's first phase, *finding* your tests before running any.
It imports your test files and builds the list of test items. "Workers
collected different test sets" means two workers disagreed on that list,
usually a randomized or time-based nodeid (see
[Troubleshooting](../reference/troubleshooting.md)).

**pytest-xdist (xdist)**: the original pytest plugin for running tests in
parallel across worker processes. rstest replaces it (you don't install or
configure xdist), but reuses its vocabulary (`gw0`/`gw1` worker names, the
`-n` flag, `--dist` modes), so xdist users feel at home. See
[Migrating from xdist](../guides/migrate-from-xdist.md).

**`--dist` mode (test distribution)**: controls *which worker* a test lands
on. The default spreads individual tests across workers for speed. You only
change it when tests must stay together:

- `--dist loadfile`: all tests in one file run on the same worker (use when
  tests in a file share state and must run together).
- `--dist loadscope`: tests sharing a class/module fixture stay together.
- `--dist loadgroup`: tests marked `@pytest.mark.xdist_group("name")` stay
  together.

If you've never needed this, you don't need it now.

**Parallel-safe**: a test that gives the same result whether it runs alone or
alongside others. A test is *not* parallel-safe if it depends on another test
running first, or fights another test over a shared resource (the same file,
port, database row, or global variable). See
[Parallel safety](../guides/parallel-safety.md).

**`@pytest.mark.serial`**: a marker you put on a test that must **not** run in
parallel with anything. rstest runs all `serial` tests by themselves, after the
parallel tests finish. It's the escape hatch for a test that isn't parallel-safe
yet. (The exclusive run itself is the [Serial phase](#serial-phase).)

**Sharding (`--shard K/N`)**: splitting your suite across `N` separate CI
*machines*, each running its slice `K`. Different from `-n`: `-n` uses multiple
cores on *one* machine; `--shard` uses multiple machines. You only need this
for very large suites in CI. See [Sharding](../guides/sharding.md).

**Warm vs cold run**: rstest remembers how long each test took (in
`.rstest_cache/`). The **first** run is "cold" (no timings yet), so scheduling
isn't optimal. From the **second** ("warm") run on, it starts the slowest
tests first and gets faster. **Don't judge rstest's speed on the first run.**

**Byte-exact mode**{#byte-exact-mode}: `-n 0`: one process, runs exactly like plain
pytest. You get the same per-test outcomes and, with no `--output` set,
pytest's own terminal output, with rstest's extras (doctor report, coverage
report, quarantined failures, gate messages) appended after it. An explicit
`--output` switches back to rstest's renderer. `--junitxml` is pytest's own
document too, with rstest's `flaky` / `quarantined` properties added.
`-n 0` and `-n 1` are identical. Both run one
pytest session in a single Python process, with no scheduling and no `[gwN]`
attribution (the compatibility anchor). Also called
**single-worker mode** (the `-n` help and banner hint), **pytest-exact mode**
(the run banner shown with an explicit `--output`), or **passthrough** when a terminal flag forces it; all
name this same mode. There is no worker identity below
`-n 2` (unlike pytest-xdist, whose `-n 1` spawns a `gw0` worker; see
[xdist migration](../guides/migrate-from-xdist.md)). The flags that need
pytest's own terminal (`--co`/`--collect-only`, `-s`, `--capture=...`,
`--pdb`, `--trace`, `--sw`/`--stepwise`, `--sw-skip`/`--stepwise-skip`,
`--sw-reset`/`--stepwise-reset`, and rstest's `--debug`) switch to this mode
automatically. See [Compatibility](compatibility.md)
for the guarantee and [Architecture](architecture.md) for how it falls
back. One opt-in exception: passing [`--reruns`](../reference/cli.md#-reruns-n)
runs `-n 0`/`-n 1` as a degenerate one-worker pool so retries fire, trading
byte-exactness for the reruns you asked for.

**Vendored core**: the unmodified copy of pytest shipped inside
`rstest_worker._vendor`; provides all test semantics. Never conflicts with
an installed pytest.

**Long pole**: the slowest single test in the run (`long_pole_seconds` in
the doctor report). No worker count can finish the run faster than it. When
it (or any test) is longer than the ideal per-worker share
(`test time / workers`), it sets the **parallel floor**: adding workers stops
helping. Slow tests from the duration cache are dispatched first,
individually, so the long pole starts early. Written "long-pole" only as an
adjective ("long-pole tests").

**Wait-bound**: a test (or suite) whose wall time far exceeds its CPU time:
it is sleeping or waiting on IO or a timeout, not computing. `--doctor`
reports the share of test time spent waiting. See
[Wait-bound / IO suites](../guides/wait-bound.md).

**Parallel floor**{#parallel-floor}: the lower bound on wall time set by the longest single
test: no worker count can finish faster. `--doctor` names the gate tests when
the longest test exceeds the ideal per-worker share. See
[Suite diagnostics](../guides/doctor.md#parallel-floor).

**Duration-aware scheduling**: dispatching tests using the per-test durations
recorded in `.rstest_cache/` by earlier runs, slowest first, so long tests
start early instead of stacking at the end. It needs one prior (warm) run.
See [Scheduling](scheduling.md).

**Flaky**: a test that failed and then passed within the
[`--reruns`](../reference/cli.md#-reruns-n) budget; reported green but
counted and listed.

**Selection**: the set of tests chosen to run; under
[`--changed`](../reference/cli.md#-changedrev), derived from the
per-test coverage index when it is warm, with the import graph as the
fallback (see [Caching](caching.md)).

**Quarantine**: a list of known-flaky tests
([`--quarantine FILE`](../reference/cli.md#-quarantine-file)) whose failures
are reported (counted, printed, flagged in junit and report-json) but never
fail the run. Unlike `--reruns`, nothing is retried; the failure is just
made non-fatal.

**Gate**: a check that sets the exit code on top of the test outcomes, so CI
can fail on it: [`--doctor-fail-on`](../reference/cli.md#-doctor-fail-on-cond),
[`--fail-on-leak`](../reference/cli.md#-fail-on-leak),
[`--durations-regress`](../reference/cli.md#-durations-regress-ratio),
[`--cov-diff-fail-under`](../reference/cli.md#-cov-diff-fail-under-pct),
[`--changed-strict`](../reference/cli.md#-changed-strict). Its output lines
are the "gate messages". Not the same as **gate tests**, below.

**Gate tests**: in the doctor report, the tests longer than the ideal
per-worker share: they set the [parallel floor](#parallel-floor), so
only splitting or shrinking them makes the run faster.

**Baseline**: whatever a comparison is measured against. It means different
things in different places: the plain-pytest run in
[`rstest try`](../reference/cli-commands.md#try); the last fully green commit
for [`--since-green`](../reference/cli.md#-since-green); and the duration
cache for [`--durations-regress`](../reference/cli.md#-durations-regress-ratio)
(which [`--require-baseline`](../reference/cli.md#-require-baseline) makes
mandatory).

**Journal**: the record of which worker ran which test, in what order, that
every parallel run writes to `.rstest_cache/replay/`.
[`rstest replay`](../reference/cli-commands.md#replay) re-runs it to
reproduce a parallel-only failure.

**Hang watchdog**: the per-test time limit after which the orchestrator kills
a stuck worker and reports the test failed:
[`--worker-timeout`](../reference/cli.md#-worker-timeout-secs), or
3 × the test's timeout + 10s. See
[Crash handling](crash-handling.md).

## Internals

The machinery below the everyday surface: useful when you're debugging
scheduling or reading the architecture docs, not for day-to-day use.

**Controller (xdist "master")**: pytest-xdist's central coordinating
process, called "master" in older xdist code and in the `"master"`
`worker_id` value. rstest has no such process (the Rust **orchestrator**
plays that role), so controller-side xdist hooks are *emulated* per worker. See
[xdist hook emulation](xdist-hooks.md).

**Item dispatch**: distributing individual tests (not files) to workers.
In the default eager mode a test travels as its index into the verified
collection; under [lazy collection](lazy-collection.md) (`--collect lazy`)
it travels by nodeid, since lazy workers share no index space.

**Chunk**: a contiguous run of collection order dispatched as one unit,
preserving module-fixture locality.

**nextitem invariant**: a worker never runs its final pending test until
it knows the successor (teardown scoping requires it); queues must always
end explicitly.

**Designate**: the worker chosen to host the serial phase, which is the
lowest alive worker (promoted to the next one if it crashes). (The full
collection nodeid list is always shipped by worker `gw0`; the others verify their
collection against it by count and hash.)

**Serial phase**{#serial-phase}: `@pytest.mark.serial` tests running exclusively on the
designate after all other workers finish.

**Cache segment**: one immutable file a run or shard pushes to the
[shared cache](caching.md#shared-cache-backend); a pull merges the base with
every segment, so parallel jobs never overwrite each other.

**Shard bucket**: one of the `N` duration-balanced slices of the suite that
`--shard K/N` splits the collection into; shard `K` runs bucket `K`. See
[Sharding](../guides/sharding.md).

**Fork pool**: with [`--fork-pool`](../reference/cli.md#-fork-pool) (Unix),
workers are forked from one process that has already imported the vendored
core, instead of each importing it from scratch.
