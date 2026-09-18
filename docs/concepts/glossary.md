# Glossary

## Everyday terms

Start here — the terms you actually hit first coming from pytest.

**`-n` (worker count)** — how many parallel worker processes run your tests.
`-n auto` (the default) uses your cores; `-n 4` uses four; `-n 0` (or `-n 1`)
turns parallelism off and runs one plain pytest session. You rarely need to
set it. See [Byte-exact mode](#byte-exact-mode) for what `-n 0` gives you.

**Collection** — pytest's first phase: *finding* your tests before running any.
It imports your test files and builds the list of test items. "Workers
collected different test sets" means two workers disagreed on that list —
usually a randomized or time-based test id (see
[Troubleshooting](../reference/troubleshooting.md)).

**pytest-xdist (xdist)** — the original pytest plugin for running tests in
parallel across worker processes. rstest replaces it (you don't install or
configure xdist), but reuses its vocabulary — `gw0`/`gw1` worker names, the
`-n` flag, `--dist` modes — so xdist users feel at home. See
[Migrating from xdist](../guides/migrate-from-xdist.md).

**`--dist` mode (test distribution)** — controls *which worker* a test lands
on. The default spreads individual tests across workers for speed. You only
change it when tests must stay together:

- `--dist loadfile` — all tests in one file run on the same worker (use when
  tests in a file share state and must run together).
- `--dist loadscope` — tests sharing a class/module fixture stay together.
- `--dist loadgroup` — tests marked `@pytest.mark.xdist_group("name")` stay
  together.

If you've never needed this, you don't need it now.

**Parallel-safe** — a test that gives the same result whether it runs alone or
alongside others. A test is *not* parallel-safe if it depends on another test
running first, or fights another test over a shared resource (the same file,
port, database row, or global variable). See
[Parallel safety](../guides/parallel-safety.md).

**`@pytest.mark.serial`** — a marker you put on a test that must **not** run in
parallel with anything. rstest runs all `serial` tests by themselves, after the
parallel tests finish — the escape hatch for a test that isn't parallel-safe
yet. (The exclusive run itself is the [Serial phase](#serial-phase).)

**Sharding (`--shard K/N`)** — splitting your suite across `N` separate CI
*machines*, each running its slice `K`. Different from `-n`: `-n` uses multiple
cores on *one* machine; `--shard` uses multiple machines. You only need this
for very large suites in CI. See [Sharding](../guides/sharding.md).

**Warm vs cold run** — rstest remembers how long each test took (in
`.rstest_cache/`). The **first** run is "cold" — no timings yet, so scheduling
isn't optimal. From the **second** ("warm") run on, it starts the slowest
tests first and gets faster. **Don't judge rstest's speed on the first run.**

**Worker** — a Python process (`gw0`, `gw1`, ...) running your project's
interpreter with the vendored pytest core; executes tests and streams
reports to the orchestrator.

**Orchestrator** — the `rstest` binary: spawns workers, dispatches tests,
merges results, renders output.

**Byte-exact mode**{#byte-exact-mode} — `-n 0` and `-n 1` are identical: one in-process
pytest session, no worker, no `[gwN]` attribution, byte-exact pytest
behavior — the compatibility anchor. There is no worker identity below
`-n 2` (unlike pytest-xdist, whose `-n 1` spawns a `gw0` worker — see
[xdist migration](../guides/migrate-from-xdist.md)). The flags that need
pytest's own terminal (`--co`, `-s`, `--capture`, `--pdb`, `--trace`)
switch to this mode automatically. See [Compatibility](compatibility.md)
for the guarantee and [Architecture](architecture.md) for how it falls
back. One opt-in exception: passing [`--reruns`](../reference/cli.md#-reruns-n)
runs `-n 0`/`-n 1` as a degenerate one-worker pool so retries fire, trading
byte-exactness for the reruns you asked for.

**Flaky** — a test that failed and then passed within the
[`--reruns`](../reference/cli.md#-reruns-n) budget; reported green but
counted and listed.

**Selection** — the set of tests chosen to run; under
[`--changed`](../reference/cli.md#-changedrev), derived from the import
graph.

## Internals

The machinery below the everyday surface — useful when you're debugging
scheduling or reading the architecture docs, not for day-to-day use.

**Master / controller** — pytest-xdist's term for its central coordinating
process. rstest has no such process — the Rust **orchestrator** plays that
role — so "master-side" xdist hooks are *emulated* per worker. See
[xdist hook emulation](xdist-hooks.md).

**Vendored core** — the unmodified copy of pytest shipped inside
`rstest_worker._vendor`; provides all test semantics. Never conflicts with
an installed pytest.

**Item dispatch** — distributing individual tests (not files) to workers
by index into the verified collection.

**Long-pole** — a test whose duration exceeds the ideal per-worker share;
it caps the wall time of any parallel run. Dispatched first, individually.

**Chunk** — a contiguous run of collection order dispatched as one unit,
preserving module-fixture locality.

**nextitem invariant** — a worker never runs its final pending test until
it knows the successor (teardown scoping requires it); queues must always
end explicitly.

**Designate** — the worker chosen to host the serial phase and to ship the
full collection id list.

**Serial phase**{#serial-phase} — `@pytest.mark.serial` tests running exclusively on the
designate after all other workers finish.
