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
set it. See [Single-worker mode](#single-worker-mode) for what `-n 0` gives you.

**Collection**: pytest's first phase, *finding* your tests before running any.
It imports your test files and builds the list of test items. "Workers
collected different test sets" means two workers disagreed on that list,
usually a randomized or time-based nodeid (see
[Troubleshooting](../reference/troubleshooting.md)).

**Lazy collection**: a collection strategy (`--collect lazy`) where each
test file is collected once, on one worker, on demand, instead of every
worker collecting the whole suite (`--collect full`). Picked automatically
for large warm-cache parallel runs. See [Lazy collection](lazy-collection.md).

**pytest-xdist (xdist)**: the original pytest plugin for running tests in
parallel across worker processes. rstest replaces it (you don't need xdist
to run in parallel, but you may keep it installed during migration), but
reuses its vocabulary (`gw0`/`gw1` worker names, the `-n` flag, `--dist`
modes), so xdist users feel at home. See
[Migrating from pytest-xdist](../guides/migrate-from-xdist.md).

**`--dist` mode (test distribution)**: controls *which worker* a test lands
on. The default spreads individual tests across workers for speed. You only
change it when tests must stay together:

- `--dist loadfile`: all tests in one file run on the same worker (use when
  tests in a file share state and must run together).
- `--dist loadscope`: tests sharing a class/module fixture stay together.
- `--dist loadgroup`: tests marked `@pytest.mark.xdist_group("name")` stay
  together.

These three are the **affinity modes**: they dispatch whole groups and turn
off slowest-first reordering (see
[Affinity modes](scheduling.md#affinity-modes)).

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

**Monorepo project**: a subdirectory with its own pytest configuration, found
when you run rstest from a repo root that has none. Each project runs as its
own isolated session under a shared worker budget. Its **slug** is its path
relative to the root with separators replaced by `-` (`libs/core` ->
`libs-core`), used in per-project output file names and cache directories.
See [Monorepo mode](monorepo.md).

**Warm vs cold run**: rstest remembers how long each test took (in
`.rstest_cache/`). The **first** run is "cold" (no timings yet), so scheduling
isn't optimal. From the **second** ("warm") run on, it starts the slowest
tests first and gets faster. **Don't judge rstest's speed on the first run.**

<span id="byte-exact-mode"></span>**Single-worker mode**{#single-worker-mode}:
what `-n 0` and `-n 1` run (and `-n auto` when it resolves to one worker):
one pytest session in one process, with no scheduling and no `[gwN]`
identity. Its output is byte-exact pytest output; flags such as `--pdb` and
`-s` force it (**passthrough**). See
[Single-worker mode](compatibility.md#single-worker-mode) for the full
guarantee and its `--reruns` exception.

**Vendored core**: the unmodified copy of pytest shipped inside
`rstest_worker._vendor`; provides all test semantics. Never conflicts with
an installed pytest.

**Long pole**: the slowest single test in the run (`long_pole_seconds` in
the doctor report). No worker count can finish the run faster than it. When
it (or any test) is longer than both the ideal per-worker share
(`test time / workers`) and 1 second, it sets the **parallel floor**: adding workers stops
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

**Duration-aware scheduling**{#duration-aware-scheduling}: dispatching tests using the per-test durations
recorded in `.rstest_cache/` by earlier runs, slowest first, so long tests
start early instead of stacking at the end. It needs one prior (warm) run.
See [Scheduling](scheduling.md).

**Flaky**: a test that failed and then passed on a retry, from the
[`--reruns`](../reference/cli.md#-reruns-n) budget or its own
`@pytest.mark.flaky(reruns=N)`; reported green but counted and listed.

**Doctor**: the diagnosis report [`--doctor`](../reference/cli.md#-doctor)
prints after a normal run: where test time goes (wait-bound share, parallel
floor, fixture hotspots, slowest files, leaks), also available as JSON or
Markdown. See [Suite diagnostics](../guides/doctor.md).

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

**Gate tests**: in the doctor report, the tests longer than both the ideal
per-worker share and 1 second: they set the [parallel floor](#parallel-floor), so
only splitting or shrinking them makes the run faster.

**Baseline**: whatever a comparison is measured against. It means different
things in different places: the plain-pytest run in
[`rstest try`](../reference/cli-commands.md#try); the last fully green commit
for [`--since-green`](../reference/cli.md#-since-green); and the duration
cache for [`--durations-regress`](../reference/cli.md#-durations-regress-ratio)
(which [`--require-baseline`](../reference/cli.md#-require-baseline) makes
mandatory).

**Journal**{#journal}: the record of which worker ran which test, in what order, that
a parallel run writes to `.rstest_cache/replay/` (not under `--shard`,
`--dist each`, `rstest replay` itself, or with `RSTEST_NO_REPLAY_JOURNAL=1`).
[`rstest replay`](../reference/cli-commands.md#replay) re-runs it to
reproduce a parallel-only failure.

**Replay**: [`rstest replay`](../reference/cli-commands.md#replay) re-runs a
recorded [journal](#journal), pinning every test to the same worker in the
same order, so a parallel-only failure reproduces on demand (including a CI
journal you download). See [Replay](../guides/replay.md).

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
Under full collection (`--collect full`, where every worker collects the
whole suite) a test travels as its index into the verified collection; under [lazy collection](lazy-collection.md) (`--collect lazy`)
it travels by nodeid, since lazy workers share no index space.

**LPT (longest-processing-time-first)**: the scheduling rule behind
[duration-aware scheduling](#duration-aware-scheduling) and `--shard`: take
the longest tests first, each to the next free worker (or, for `--shard`,
to the lightest bucket). See [Scheduling](scheduling.md#dispatch-order).

**Work-stealing**: under [lazy collection](lazy-collection.md) with an
explicit `--dist load`, an idle worker with no files left to take grabs half
of the busiest worker's undispatched tests, re-collecting that file. Off by
default: lazy otherwise keeps each file on one worker.

**Chunk**: a contiguous run of collection order dispatched as one unit,
preserving module-fixture locality.

**nextitem invariant**: a worker never runs its final pending test until
it knows the successor (teardown scoping requires it); queues must always
end explicitly.

**Designate**: the worker chosen to host the serial phase, which is the
lowest alive worker (promoted to the next one if it crashes). (The full
collection nodeid list is always shipped by worker `gw0`; every worker's
collection is verified by count and hash against whichever worker reports
first.)

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
