# Crash handling

A test that kills its worker process — a segfaulting C extension, an
`os._exit`, an OOM kill — costs one FAILED line, not the run.

## Attribution

Workers announce `item_start` before each test, so when a worker dies the
orchestrator knows exactly which test was in flight. (pytest-xdist infers
this from its queue, which can misattribute; the explicit signal cannot.)

## What happens

1. The in-flight test is reported **failed**, with a "crashed while
   running this test" message. It is **not retried** by default — a
   reliably-segfaulting test would otherwise kill workers in a loop.
   With [`--reruns`](../reference/cli.md#-reruns-n), it gets retried on
   the replacement worker within the rerun budget.
2. The worker's other outstanding tests requeue at the head of the
   dispatch queue and run elsewhere.
3. A replacement worker spawns under the same identity (`gw3` stays
   `gw3` — PASSIVE per-worker resources keyed on worker id, like
   pytest-django's `test_db_gw3`, stay bounded and get reused),
   re-collects, verifies its collection by hash, and rejoins. Note the
   distinction: resources PROVISIONED by master-side hooks should use
   uuid idents, not worker-id-derived ones — the replacement's
   re-provisioning can race the crashed node's cleanup (see
   [xdist hook emulation](xdist-hooks.md)).

## Budgets

Total restarts per run are capped (`max(workers, 4)`). Past the cap, a
dead worker is reported as an internal error with its remaining tests
listed as lost — a crash-loop ends loudly rather than spinning. Crashes
during collection are not restarted (an import-time crash would recur).

## Cleanup hooks and the serial phase

If the suite uses xdist's master-side hooks, a crashed worker's
`pytest_testnodedown` still runs — on a surviving worker, against the
dead worker's `workerinput` snapshot (details and the ordering caveat
with deterministic idents: [xdist hook
emulation](xdist-hooks.md)). If the
crashed worker was the designated serial-phase host, the lowest
surviving worker is promoted; if none can host it, the run reports the
serial tests as lost rather than silently dropping them.

## Exit codes

Crash-fabricated failures never pass through any worker session, so
session exit codes alone would read 0; recorded outcomes take precedence —
a run with a crashed test exits 1.
