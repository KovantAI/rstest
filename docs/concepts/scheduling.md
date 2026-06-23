# Scheduling

How tests are distributed across workers in the default (`--dist load`)
mode.

## Collection and verification

Every worker collects the identical session (same args, same ini, same
conftest semantics). Workers verify agreement by item count + hash of the
node-id list; one designated worker ships the full list. Divergent
collections (typically a randomizing plugin without a fixed seed) abort
the run before any misassignment.

Seeding is barrier-free: each worker starts receiving work the moment its
own collection verifies against the reference — early collectors run
tests while stragglers finish collecting. The refusal guarantee is
per-worker: no worker is ever ASSIGNED work before its collection has
been cross-checked, so a divergent straggler aborts the run without
having received (or misrun) a single test — but tests on already-verified
workers may have started by then.

## Dispatch order

1. **Long-poles first.** Tests with a cached duration ≥ 1s dispatch first,
   longest first, one at a time — so they spread across workers instead of
   stacking. This is what beats file-affinity schedulers on wait-heavy
   suites: a 54-second test starting at t=0 instead of t=90 changes the
   whole run's wall time.
2. **Everything else in contiguous chunks.** Chunks preserve module
   locality (module/class fixtures set up once per worker visit) and cut
   protocol round-trips. Chunk size scales with suite size; refills happen
   when a worker half-drains.

The duration cache (`.rstest_cache/durations.json`) is written after every
run, so the first run is collection-ordered and every later run is
duration-aware.

## The nextitem invariant

pytest's teardown scoping depends on knowing each test's successor
(`nextitem`): a worker therefore never runs its last pending item until it
learns what comes next — or learns the queue is exhausted *for now*
(`no_more_items`, which runs the held item with `nextitem=None`). Workers
then keep listening: a failed test from any worker can be rerun on them
until an explicit end-of-session signal confirms every outcome is final.
Every dispatch path must keep at least one successor in flight or
explicitly release the queue; this invariant shaped most of the
scheduler's edge cases (three deadlocks' worth).

## The serial phase

`@pytest.mark.serial` items are excluded from the parallel queue. One
designated worker is held open; when every other worker's session has
fully finished (fixtures torn down, ports released), the serial items run
there exclusively, in collection order.

## Affinity modes

`--dist loadfile`, `loadscope`, and `loadgroup` replace the above with
keyed groups in collection order — a dispatch never splits a group, and
duration reordering is off (affinity is the point, at the cost of
long-pole splitting):

- `loadfile`: groups are whole files.
- `loadscope`: groups are fixture scopes — a class's tests, or a module's
  functions.
- `loadgroup`: groups are `@pytest.mark.xdist_group("name")` marks,
  consolidated across files; unmarked tests stay individual.

## Broadcast mode (`--dist each`)

`--dist each` is not distribution at all — every worker runs the **full
suite** (xdist `--dist=each`), so the run legitimately contains each test N
times. It is for multi-environment validation: run the same suite across N
workers configured differently. There is no item dispatch queue; each worker
is seeded with every index, and a crash replacement reruns only the dead
worker's remaining items.

Consequences:

- Outcomes are keyed `nodeid [gwN]`, since the same test appears once per
  worker.
- The duration cache is **not** written — N× runs would poison LPT
  scheduling on the next normal run.
- `--reruns` is rejected: every worker already runs the suite, so a rerun
  has no distinct meaning.
