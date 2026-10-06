# Glossary

Short definitions of the terms used across these docs, alphabetical within
each section. Coming from pytest, start with [Everyday terms](#everyday-terms).

## Everyday terms

The terms you actually hit first coming from pytest.

**Baseline**{ #baseline }: whatever a comparison is measured against. It means
different things in different places: the plain-pytest run in
[`rstest try`](../reference/cli-commands.md#try); the last fully green commit
for [`--since-green`](../reference/cli.md#-since-green); and the
[duration cache](#duration-cache) for
[`--durations-regress`](../reference/cli.md#-durations-regress-ratio)
(which [`--require-baseline`](../reference/cli.md#-require-baseline) makes
mandatory).

**CI gate**{ #ci-gate }: a check that sets the exit code on top of the test
outcomes, so CI can fail on it:
[`--doctor-fail-on`](../reference/cli.md#-doctor-fail-on-cond),
[`--fail-on-leak`](../reference/cli.md#-fail-on-leak),
[`--durations-regress`](../reference/cli.md#-durations-regress-ratio),
[`--cov-diff-fail-under`](../reference/cli.md#-cov-diff-fail-under-pct),
[`--changed-strict`](../reference/cli.md#-changed-strict). Its output lines
are the "gate messages". Unrelated to the doctor's
[gate tests](#gate-test).

**Collection**{ #collection }: pytest's first phase, *finding* your tests
before running any. It imports your test files and builds the list of test
items. "Workers collected different test sets" means two workers disagreed on
that list, usually a randomized or time-based nodeid (see
[Unstable parametrize ids](compatibility.md#unstable-parametrize-ids)).

**Distribution mode (`--dist`)**{ #dist-mode }: controls *which worker* a
test lands on. The default spreads individual tests across workers for speed.
You only change it when tests must stay together:

- `--dist loadfile`: all tests in one file run on the same worker (use when
  tests in a file share state and must run together).
- `--dist loadscope`: tests sharing a class/module fixture stay together.
- `--dist loadgroup`: tests marked `@pytest.mark.xdist_group("name")` stay
  together.

These three are the **affinity modes**: they dispatch whole groups and turn
off slowest-first reordering (see
[Affinity modes](scheduling.md#affinity-modes)). If you've never needed this,
you don't need it now.

**Doctor**{ #doctor }: the diagnosis report
[`--doctor`](../reference/cli.md#-doctor) prints after a normal run: where
test time goes (wait-bound share, parallel floor, fixture hotspots, slowest
files, leaks), also available as JSON or Markdown. See
[Suite diagnostics](../guides/doctor.md).

**Duration cache**{ #duration-cache }: `.rstest_cache/durations.json`, the
per-test call-phase wall times (fixture setup and teardown excluded) that
earlier runs recorded. It
drives [duration-aware scheduling](#duration-aware-scheduling), `--shard`
balancing and `-n auto` sizing, and is the baseline for `--durations-regress`.
A failed test's duration is never recorded. Shared across CI jobs with
[`--cache-push` / `--cache-pull`](caching.md#shared-cache-backend).

**Duration-aware scheduling**{ #duration-aware-scheduling }: dispatching
tests using the [duration cache](#duration-cache), slowest first, so long
tests start early instead of stacking at the end. It needs one prior (warm)
run. See [Scheduling](scheduling.md).

**Flaky**{ #flaky }: a test that failed and then passed on a retry, from the
[`--reruns`](../reference/cli.md#-reruns-n) budget or its own
`@pytest.mark.flaky(reruns=N)`; reported green but counted and listed.

**Gate test**{ #gate-test }: the doctor report's name for a test that sets
the [parallel floor](#parallel-floor). Not a [CI gate](#ci-gate).

**Hang watchdog**{ #hang-watchdog }: the per-test time limit after which the
orchestrator kills a stuck worker and reports the test failed. See
[Hung tests](crash-handling.md#hung-tests-worker-timeout).

**Journal**{ #journal }: the record of which worker ran which test, in what
order, that a parallel run writes to `.rstest_cache/replay/` (not under
`--shard`, `--dist each`, `rstest replay` itself, or with
`RSTEST_NO_REPLAY_JOURNAL=1`).
[`rstest replay`](../reference/cli-commands.md#replay) re-runs it to
reproduce a parallel-only failure.

**Lazy collection**{ #lazy-collection }: a collection strategy
(`--collect lazy`) where each test file is collected once, on one worker, on
demand, instead of every worker collecting the whole suite
(`--collect full`). Picked automatically for large warm-cache parallel runs.
See [Lazy collection](lazy-collection.md).

**Long pole**{ #long-pole }: the slowest single test in the run, setup +
call + teardown (`long_pole_seconds` in the doctor report). Its duration is
the [parallel floor](#parallel-floor). Tests whose cached call time in the
[duration cache](#duration-cache) is long are dispatched first,
individually, so the long pole usually starts early; a test whose cost sits
in its fixtures is not pulled forward, because the cache holds call time
only. Written "long-pole" only as an adjective ("long-pole tests").

**Monorepo project**{ #monorepo-project }: a subdirectory with its own pytest
configuration, found when you run rstest from a repo root that has none. Each
project runs as its own isolated session under a shared worker budget. Its
**slug** is its path relative to the root with separators replaced by `-`
(`libs/core` -> `libs-core`), used in per-project output file names and cache
directories. See [Monorepo mode](monorepo.md).

**Orchestrator**{ #orchestrator }: the `rstest` binary, which spawns workers,
dispatches tests, merges results, and renders output.

**Parallel floor**{ #parallel-floor }: the lower bound on wall time set by
the [long pole](#long-pole): no worker count can finish faster than the
slowest single test. The doctor reports a PARALLEL FLOOR section only when the
long pole clearly exceeds the ideal per-worker share (test time ÷ workers, or
1 second if that is larger, plus 10% slack), and lists the tests above that
threshold as **gate tests**: only splitting or shrinking them makes the run
faster. See [Suite diagnostics](../guides/doctor.md#parallel-floor).

**Parallel-safe**{ #parallel-safe }: a test that gives the same result whether
it runs alone or alongside others. A test is *not* parallel-safe if it depends
on another test running first, or fights another test over a shared resource
(the same file, port, database row, or global variable). See
[Parallel safety](../guides/parallel-safety.md).

**Passthrough**{ #passthrough }: what rstest calls a flag that needs pytest's
own terminal or stdin (`--co`, `-s`, `--capture=...`, `--pdb`, `--trace`,
the stepwise flags, `--debug`). Any such flag switches the run to
[single-worker mode](#single-worker-mode) whatever `-n` says, and `--reruns`
has no effect there. See
[Passthrough-IO flags](../reference/cli.md#passthrough-io-flags).

**Performance cores**{ #performance-cores }: on a hybrid CPU (Apple silicon,
recent Intel), the fast cores, as opposed to the slower efficiency cores.
`-n auto` counts every logical core, but a CPU-bound suite typically speeds up
only until the worker count reaches the performance-core count, then
flattens. A wait-bound suite is not limited this way.

**Polluter and victim**{ #polluter }: a <span id="victim"></span>**victim**
passes alone but fails after some other test ran before it on the same
worker; that earlier test is the **polluter**, whose leftover state (a global,
an environment variable, a file, a database row) breaks it.
[`rstest bisect`](../reference/cli-commands.md#bisect-nodeid) finds the
polluter and [`rstest audit`](../reference/cli-commands.md#audit) sweeps the
suite for victims.

**pytest-xdist (xdist)**{ #xdist }: the original pytest plugin for running
tests in parallel across worker processes. rstest replaces it (you don't need
xdist to run in parallel, but you may keep it installed during migration),
but reuses its vocabulary (`gw0`/`gw1` worker names, the `-n` flag, `--dist`
modes), so xdist users feel at home. See
[Migrating from pytest-xdist](../guides/migrate-from-xdist.md).

**Quarantine**{ #quarantine }: a list of known-flaky tests
([`--quarantine FILE`](../reference/cli.md#-quarantine-file)) whose failures
are reported (counted, printed, flagged in junit and report-json) but never
fail the run. Unlike `--reruns`, nothing is retried; the failure is just
made non-fatal.

**Replay**{ #replay }: [`rstest replay`](../reference/cli-commands.md#replay)
re-runs a recorded [journal](#journal), pinning every test to the same worker
in the same order, so a parallel-only failure reproduces on demand (including
a CI journal you download). See [Replay](../guides/replay.md).

**Rootdir**{ #rootdir }: pytest's root directory for the session, found from
your arguments and config files exactly as pytest finds it. rstest keeps
`.rstest_cache/` there, next to `.pytest_cache/`, so a run from a
subdirectory reads and writes the project's one cache. `--rootdir` and `-c`
move it. See [Caching](caching.md).

**Selection**{ #selection }: the set of tests chosen to run; under
[`--changed`](../reference/cli.md#-changedrev), derived from the per-test
coverage index when it is warm, with the import graph as the fallback (see
[Selecting changed tests](../guides/changed.md)).

**Serial marker (`@pytest.mark.serial`)**{ #serial-marker }: a marker you
put on a test that must **not** run in parallel with anything. rstest runs all
`serial` tests by themselves, after the parallel tests finish. It's the escape
hatch for a test that isn't parallel-safe yet. (The exclusive run itself is
the [serial phase](#serial-phase).)

**Sharding (`--shard K/N`)**{ #sharding }: splitting your suite across `N`
separate CI *machines*, each running its slice `K`. Different from `-n`: `-n`
uses multiple cores on *one* machine; `--shard` uses multiple machines. You
only need this for very large suites in CI. See
[Sharding](../guides/sharding.md).

<span id="byte-exact-mode"></span>**Single-worker mode**{ #single-worker-mode }:
what `-n 0` and `-n 1` run (and `-n auto` when it resolves to one worker):
one pytest session in one process, with no scheduling and no
[worker id](#worker-id). Its output is byte-exact pytest output;
[passthrough](#passthrough) flags such as `--pdb` and `-s` force it. See
[Single-worker mode](compatibility.md#single-worker-mode) for the full
guarantee and its `--reruns` exception.

**Vendored core**{ #vendored-core }: the unmodified copy of pytest shipped
inside `rstest_worker._vendor`; provides all test semantics. Never conflicts
with an installed pytest.

**Wait-bound**{ #wait-bound }: a test (or suite) whose wall time far exceeds
its CPU time: it is sleeping or waiting on IO or a timeout, not computing.
`--doctor` reports the share of test time spent waiting. See
[Wait-bound / IO suites](../guides/wait-bound.md).

**Warm vs cold run**{ #warm-cold-run }: rstest remembers how long each test
took (the [duration cache](#duration-cache)). The **first** run is "cold" (no
timings yet), so scheduling isn't optimal. From the **second** ("warm") run
on, it starts the slowest tests first and gets faster. **Don't judge rstest's
speed on the first run.**

**Worker**{ #worker }: a Python process running your project's interpreter
with the vendored pytest core; executes tests and streams reports to the
orchestrator. Each pool worker has a [worker id](#worker-id).

**Worker count (`-n`)**{ #worker-count }: how many parallel worker processes
run your tests. `-n auto` (the default) uses your cores, but never more
workers than the selected test files on a first run (cached timings lift that
to the test count), and fewer on a suite whose cached run time is only a few
seconds; `-n 4` uses exactly four; `-n 0` (or `-n 1`) turns parallelism off
and runs one plain pytest session. You rarely need to set it. See
[Single-worker mode](#single-worker-mode) for what `-n 0` gives you.

**Worker id (`gwN`)**{ #worker-id }: a pool worker's identity, `gw0`, `gw1`,
..., in pytest-xdist's format. Tests read it from the `worker_id` fixture,
`PYTEST_XDIST_WORKER`, `RSTEST_WORKER_ID` or `workerinput["workerid"]`. A
worker replaced after a crash keeps its id. There is no worker id in
[single-worker mode](#single-worker-mode): the `worker_id` fixture returns
`"master"` there, as under xdist without `-n`.

## Measuring compatibility

How rstest checks that it matches pytest. See
[Compatibility](compatibility.md#what-verified-means).

**Battery**{ #battery }: the four real suites (pandas, aiohttp,
django-allauth, rich) that rstest runs under both pytest and rstest, with
their real plugins, diffing per-test outcomes. Rerun in full whenever the
vendored pytest is updated.

**Corpus**{ #corpus }: the wider public-suite corpus: 33 well-known projects
listed in `corpus/suites.toml` (plus two benchmark-only entries), each run
under pytest and rstest to measure [parity](#parity) at scale. See
[Measured at scale](compatibility.md#measured-at-scale).

**Parity**{ #parity }: short for **outcome parity**, the share of tests whose
per-test outcome under rstest (every phase, skip reason class, xfail flag)
matches a plain-pytest baseline run of the same suite. Every known source of
divergence is catalogued in
[Parity divergences](../reference/parity-divergences.md).

## Internals

The machinery below the everyday surface: useful when you're debugging
scheduling or reading the architecture docs, not for day-to-day use.

**Cache segment**{ #cache-segment }: one immutable file a run or shard pushes
to the [shared cache](caching.md#shared-cache-backend); a pull merges the base
with every segment, so parallel jobs never overwrite each other.

**Chunk**{ #chunk }: a contiguous run of collection order dispatched as one
unit, preserving module-fixture locality.

**Controller (xdist "master")**{ #controller }: pytest-xdist's central
coordinating process, called "master" in older xdist code and in the
`"master"` `worker_id` value. rstest has no such process (the Rust
**orchestrator** plays that role), so controller-side xdist hooks are
*emulated* per worker. See [xdist hook emulation](xdist-hooks.md).

**Designate**{ #designate }: the worker chosen to host the serial phase,
which is the lowest alive worker (promoted to the next one if it crashes).
(The full collection nodeid list is always shipped by worker `gw0`; every
worker's collection is verified by count and hash against whichever worker
reports first.)

**Fork pool**{ #fork-pool }: with [`--fork-pool`](../reference/cli.md#-fork-pool)
(Unix), workers are forked from one process that has already imported the
vendored core, instead of each importing it from scratch.

**Item dispatch**{ #item-dispatch }: distributing individual tests (not
files) to workers. Under full collection (`--collect full`, where every worker
collects the whole suite) a test travels as its index into the verified
collection; under [lazy collection](lazy-collection.md) (`--collect lazy`) it
travels by nodeid, since lazy workers share no index space.

**LPT (longest-processing-time-first)**{ #lpt }: the scheduling rule behind
[duration-aware scheduling](#duration-aware-scheduling) and `--shard`: take
the longest tests first, each to the next free worker (or, for `--shard`, to
the lightest bucket). See [Scheduling](scheduling.md#dispatch-order).

**nextitem invariant**{ #nextitem-invariant }: a worker never runs its final
pending test until it knows the successor (teardown scoping requires it);
queues must always end explicitly.

**Run uid**{ #run-uid }: one id per run (a uuid4 in hex, xdist's
`testrun_uid` format), shared by every worker and by every project of a
monorepo run. Tests read it from the `testrun_uid` fixture,
`RSTEST_RUN_UID`, `PYTEST_XDIST_TESTRUNUID` or `workerinput["testrun_uid"]`;
it also names the run's [journal](#journal) file. Setting `RSTEST_RUN_UID`
yourself (the same value on every CI shard, for example) replaces the
generated one. See [Environment variables](../reference/environment.md).

**Serial phase**{ #serial-phase }: `@pytest.mark.serial` tests running
exclusively on the [designate](#designate) after all other workers finish.

**Shard bucket**{ #shard-bucket }: one of the `N` duration-balanced slices of
the suite that `--shard K/N` splits the collection into; shard `K` runs bucket
`K`. See [Sharding](../guides/sharding.md).

**Work-stealing**{ #work-stealing }: under
[lazy collection](lazy-collection.md) with an explicit `--dist load`, an idle
worker with no files left to take grabs half of the busiest worker's
undispatched tests, re-collecting that file. Off by default: lazy otherwise
keeps each file on one worker.
