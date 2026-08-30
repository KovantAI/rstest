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

The `4442 tests` count is tests with a **recorded call duration**, which is
what doctor analyzes — slightly fewer than the 4,469 the suite *collects*
([benchmarks](../reference/benchmarks.md)), because skips and zero-duration
tests contribute no timing. This suite is heavily wait-bound, so its
**PARALLEL EFFICIENCY** section (see below) reports over 100% and is omitted
from the sample for brevity; it appears in any `-n > 1` run.

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

### PARALLEL EFFICIENCY

Where PARALLEL FLOOR is a static ceiling, this is the *realized* speedup
measured from the run just finished: `test time / wall`, compared against
the worker count. "1.5× realized of 4× possible (38%)" means the run
converted only 38% of its worker budget into wall-clock savings — the
direct answer to "why isn't `-n auto` faster?".

Two things cap it, both named in the section:

- **long pole** — the slowest single test (same floor as PARALLEL FLOOR).
- **worker load** — busy time summed per worker, plus the imbalance
  between the busiest and idlest worker. A high imbalance means the
  scheduler couldn't spread the work evenly (usually a few long tests
  pinned to one worker); consider splitting them or `--dist load`.

Efficiency **over 100%** is normal for wait-bound suites: overlapping
sleeps/IO run more tests at once than there are cores, so the report flags
it and points back at WAIT-BOUND. Only emitted for multi-worker runs.

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

writes the same analysis as a versioned JSON document (`"schema": 2`):
totals (tests, test time, CPU time, wall, workers), the wait-bound test
list, parallel-floor gate tests, parallel-efficiency (realized speedup and
per-worker load), fixture timings, and slowest files.
Combine with `--doctor` to also print the human report. See
[Doctor JSON](../reference/report-json.md#doctor-json) for the full field
schema.

Persist it as a CI artifact per run and you have suite-health trending:
diff two reports to see what a PR added — new long-poles, fixture cost
growth, wait-time regressions. A ready-made GitHub Actions recipe
(baseline via the actions cache, jq comparison into the job summary)
is in [CI quickstart](ci-quickstart.md#suite-health-trending-with-doctor).

## Markdown output and GitHub job summaries

Under GitHub Actions, any doctor run appends the report to
`$GITHUB_STEP_SUMMARY` automatically — `rstest --doctor-json doctor.json`
in a workflow puts the analysis on the run page with no extra step. To
write the markdown to a custom path instead (or outside Actions):

```console
$ rstest --doctor-md doctor.md
```

## Gating a PR on doctor metrics

JSON trending is advisory — someone has to look. To make the signal
*enforce* itself, gate the run on a threshold with `--doctor-fail-on`:

```console
$ rstest -n auto --doctor-fail-on 'parallel_efficiency<30' \
                 --doctor-fail-on 'wait_pct>50'
```

The run exits non-zero if any condition fires (here: efficiency below 30%,
or more than half of test time spent waiting). Repeatable; the gate is the
union of all conditions. A metric that didn't apply to the run — e.g.
`parallel_efficiency` at `-n 1` — is skipped, never failed, and a typo'd
metric aborts before the run rather than silently passing. Full metric and
operator list: [`--doctor-fail-on`](../reference/cli.md#--doctor-fail-on-cond).
