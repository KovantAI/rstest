# Suite diagnostics

```console
$ rstest --doctor
```

runs your suite normally, then answers the question every slow suite
raises: *where does the time actually go?* The diagnosis comes from data
the runner already owns — per-test wall time, per-test CPU time, and
per-fixture setup time — so it adds almost nothing to the run.

## A real report

```text
================== rstest doctor ==================
4442 tests, 185.8s test time (wall 67.7s, 8 workers)

WAIT-BOUND: 95% of test time (176.5s) is waiting, not computing (sleeps / IO / timeouts).
    54.20s waiting of   54.25s  tests/test_proxy_functional.py::test_proxy_https_multi_conn_limit
    10.97s waiting of   10.97s  tests/test_proxy_functional.py::test_proxy_https_connect
  ... and 33 more

PARALLEL FLOOR: the longest test (54.2s) exceeds the ideal per-worker share (23.2s at -n 8);
no worker count can finish faster than its longest test. Gate tests:
    54.25s  tests/test_proxy_functional.py::test_proxy_https_multi_conn_limit

FIXTURE HOTSPOTS (setup time across all workers):
     0.79s   4442x  scope=function blockbuster
     0.54s    157x  scope=function transport

SLOWEST FILES:
   150.46s (81.0%)  tests/test_proxy_functional.py
     8.60s ( 4.6%)  tests/test_client_functional.py
===================================================
```

(That's [aiohttp]'s real suite. One file is 81% of total test time, almost
all of it waiting on 10-second proxy timeouts.)

[aiohttp]: https://github.com/aio-libs/aiohttp

## Reading each section

### WAIT-BOUND

Compares each test's wall time with its CPU time. A test whose wall time
vastly exceeds its CPU time isn't computing — it's sleeping, waiting on a
socket, or waiting out a timeout. These tests waste wall-clock no matter
how fast the runner is; fixing them (mock the clock, shrink the timeout,
use event-driven waits) is usually the single biggest speedup available in
a suite.

In profiling of popular open-source suites, this is the dominant pattern:
rich spends 74% of its test time in three `sleep()`-based tests; aiohttp
spends 95% waiting on proxy timeouts.

### PARALLEL FLOOR

No worker count can finish faster than the longest single test. If your
longest test exceeds the ideal per-worker share, the report names the gate
tests — splitting or shrinking them raises your parallel ceiling.

### FIXTURE HOTSPOTS

Total setup time per fixture, with two pieces of advice:

- A *function-scoped* fixture that ran hundreds of times and costs real
  time is a candidate for a wider scope — one real-world suite re-parsed
  the same RSA key 206 times (≈20% of its total runtime) in what could
  have been a session fixture.
- A *session-scoped* fixture that ran more than once ran **once per
  worker** — the report reminds you it must be safe to duplicate.

### SLOWEST FILES

Test time aggregated by file — where to look first, and the input for
deciding what to split under `--dist load`.

## Workflow

Doctor is cheap enough to run on a whim and most valuable on a cadence:

```console
$ rstest --doctor          # after a suite "feels slow"
$ rstest -n 4 --doctor     # diagnosing parallel scaling
```

## JSON output for CI

```console
$ rstest --doctor-json doctor.json
```

writes the same analysis as a versioned JSON document (`"schema": 1`):
totals (tests, test time, CPU time, wall, workers), the wait-bound test
list, parallel-floor gate tests, fixture timings, and slowest files.
Combine with `--doctor` to also print the human report. See
[Doctor JSON](../reference/report-json.md#doctor-json) for the full field
schema.

Persist it as a CI artifact per run and you have suite-health trending:
diff two reports to see what a PR added — new long-poles, fixture cost
growth, wait-time regressions. A ready-made GitHub Actions recipe
(baseline via the actions cache, jq comparison into the job summary)
is in [CI quickstart](ci-quickstart.md#suite-health-trending-with-doctor).
