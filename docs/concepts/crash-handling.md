# Crash handling

A test that kills its worker process (a segfaulting C extension, an
`os._exit`, an OOM kill) costs one FAILED line, not the run.

## Attribution

Workers announce `item_start` before each test, so when a worker dies the
orchestrator knows exactly which test was in flight. (pytest-xdist infers
this from its queue, which can misattribute; the explicit signal cannot.)

## What happens

1. The in-flight test is reported **failed**, with a "crashed while
   running this test" message. It is **not retried** by default: a
   reliably-segfaulting test would otherwise kill workers in a loop.
   With [`--reruns`](../reference/cli.md#-reruns-n), it is requeued
   within the rerun budget and retried on whichever worker takes it next.
2. The worker's other outstanding tests requeue at the head of the
   dispatch queue and run elsewhere.
3. A replacement worker spawns under the same identity (`gw3` stays
   `gw3`: **passive** per-worker resources keyed on worker id, like
   pytest-django's `test_db_gw3`, stay bounded and get reused),
   re-collects, verifies its collection by hash, and rejoins. Note the
   distinction: resources **provisioned** by controller-side hooks should use
   uuid idents, not worker-id-derived ones, because the replacement's
   re-provisioning can race the crashed node's cleanup (see
   [xdist hook emulation](xdist-hooks.md)).

Step 3 is limited by the restart budget (see [Budgets](#budgets)).

## Hung tests (`--worker-timeout`)

A test that hangs instead of crashing goes through the same machinery when
its hang watchdog fires. The watchdog's limit:

- [`--worker-timeout SECS`](../reference/cli.md#-worker-timeout-secs) when
  set, the same for every test.
- Otherwise, for a test that has a timeout
  ([`--timeout`](../reference/cli.md#-timeout-secs) or
  `@pytest.mark.timeout`), 3 × that timeout + 10 s. The marker value wins
  over `--timeout`: a test marked `timeout(300)` under `--timeout 30` gets
  910 s, not 100 s.
- A test without a timeout has no watchdog.

A worker stuck on one test past its limit, in any phase, is killed. The test
is reported failed with a timeout message instead of the crash message, and
steps 2 and 3 above follow unchanged. Under `--reruns` the timed-out test is
retried within the budget, and the kill counts against the same restart cap
below. Hangs outside a test (collection, session config) are not covered.

On Windows this watchdog is the only timeout enforcement: with
`--timeout 30` a 60 s test passes and a hung one is killed at 100 s, without
a traceback at the stuck line
([Running on Windows: timeouts](../guides/windows.md#timeouts)).

## Budgets

A run gets as many worker replacements as it has workers, and at least 4
(`max(workers, 4)`). Past the cap, a dead worker is not replaced and is
reported as a `<worker gwN>` internal error (exit 3): a crash-loop ends
loudly rather than spinning. The run carries on with the workers it has
left: the dead worker's in-flight test still fails as in step 1, and its
other tests move to the survivors as in step 2. If no worker is left to run
them, each test that never ran is reported as its own setup error, so every
test still appears in the summary, junit and report-json. Its message reads:

```text
not run: every worker that could run it crashed and the restart budget was spent
```

Under [lazy collection](lazy-collection.md), a test file no worker got to
collect is reported as a collection error starting `not collected:` with the
same reason. Under `--dist each` and `rstest replay` a worker's tests are
bound to it, so a dead worker's remaining tests are reported the same way
straight away, with the reason `not run: worker gwN crashed and the restart
budget was spent; under --dist each its tests cannot move to another worker`
(`replay` in place of `--dist each` for a replay). Crashes during collection
are not restarted (an import-time crash would recur).

## Cleanup hooks and the serial phase

If the suite uses xdist's controller-side hooks, a crashed worker's
`pytest_testnodedown` still runs under `--collect full`, on a surviving
worker, against the dead worker's `workerinput` snapshot (details and the
ordering caveat with deterministic idents: [xdist hook
emulation](xdist-hooks.md)). Under `--collect lazy` it does not run, and
the dead worker's per-worker resources are left behind. If the
crashed worker was the designated serial-phase host, the lowest
surviving worker is promoted; if none can host it, the run reports each
serial test as "not run" rather than silently dropping it.

## Exit codes

Crash-fabricated failures never pass through any worker session, so
session exit codes alone would read 0; recorded outcomes take precedence:
a run with a crashed test exits 1.
