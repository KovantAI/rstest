<!-- Generated from the Rust type by `cargo test -p rstest-cli schema`. Do not edit by hand; regenerate with RSTEST_BLESS_SCHEMAS=1. -->

## Doctor report

Source: `--doctor-json`

| Field | Type | Required | Description |
|---|---|---|---|
| `coverage_waste` | CoverageWaste or null | yes | Slow tests whose every covered line is also covered by another test - delete/merge candidates. `None` unless a per-test coverage index was warm (`--cov --cov-context=test`) and at least one test qualified. |
| `cpu_time_seconds` | number | yes | Sum of whole-protocol CPU time (the worker process plus child processes it waited for), over tests where it was measured. |
| `drift` | Drift or null | yes | Tests whose typical call time crept up across their recorded runs, in steps too small for `--durations-regress`. `null` unless one did. |
| `fixture_grouping` | FixtureGrouping or null | yes | Modules and classes `--dist load` kept on one worker for their fixture setup cost. `null` when the run grouped none (lazy collection, `--shuffle`, `--order fail-fast`, one worker, or no costly scope). |
| `fixtures` | array of FixtureEntry | yes | Fixture setup cost summed across all workers, costliest first (top 50). |
| `fork_prewarm` | boolean | yes | Whether this run already used `--fork-pool` (Unix fork-prewarm). Gates the "try --fork-pool" hint so it isn't suggested when already on. |
| `leaks` | array of Leak | no | Tests that leaked threads / fds (created by the test, still open after its teardown). Empty unless leak-check instrumentation ran (`--doctor` / `--fail-on-leak`). |
| `network` | NetworkAccess or null | yes | Tests that opened outbound network connections (outside loopback and `[tool.rstest] network-allow`). `null` when none did. |
| `parallel_efficiency` | ParallelEfficiency or null | yes | Realized speedup and worker load balance; present only on multi-worker pool runs. |
| `parallel_floor` | ParallelFloor or null | yes | Present only when the longest test outlasts both a worker's even share of the test time and 1 second, plus 10%: no worker count can finish faster than that test. |
| `rstest_version` | string | yes | The rstest version that wrote the report. |
| `schema` | integer | yes | Document schema version, bumped when the shape changes incompatibly. |
| `slowest_files` | array of FileEntry | yes | Test time per file, slowest first (top 20). |
| `startup_seconds` | number | yes | Wall from pool spawn to every worker's first event (imported core + started collecting), part of `wall_seconds`. A fixed per-run tax that `--fork-pool` (Unix) cuts at high `-n`; 0.0 on single-worker runs. Surfaced so a startup-bound suite is legible. |
| `test_time_seconds` | number | yes | Sum of each test's whole protocol (setup + call + teardown), so time spent in function fixtures counts. |
| `tests` | integer | yes | Tests with a recorded call duration (tests skipped before their call phase drop out): the population every time figure is computed over. |
| `timeouts` | TimeoutAdvice or null | yes | Suggested `--timeout` and per-test `@pytest.mark.timeout` overrides, from each test's slowest recorded passing run (call phase). `null` until some test in this run has 5 passing runs in the run history. |
| `wait_bound` | WaitBound or null | yes | Present only when waiting is a notable share (>= 20% and >= 1s); `--doctor-fail-on` gates `wait_*` on the measured values regardless. |
| `wall_seconds` | number | yes | Wall-clock time of the whole run, in seconds. |
| `workers` | integer | yes | Workers that ran tests: 1 for a single-worker (`-n 0` / `-n 1`) run. |

### CoverageWaste

Slow tests that add no unique coverage: every line each one executes is also executed by some other test, so it can be deleted or merged without dropping any covered line. Pure suite bloat on the time axis.

| Field | Type | Required | Description |
|---|---|---|---|
| `redundant_tests` | integer | yes | Count of redundant slow tests found (`tests` shows the slowest of them). |
| `tests` | array of WasteTest | yes | The slowest redundant tests, worst first (capped). |
| `wasted_seconds` | number | yes | Sum of the durations of every redundant slow test (not just the shown ones) - the time reclaimable by pruning them. |

### Drift

Slow creep: a test whose median call time rose from the older third of its recorded passing runs to the middle third to the newest, by at least 1.5x and 0.2s overall.

| Field | Type | Required | Description |
|---|---|---|---|
| `drifting_tests` | integer | yes | Tests in this run that drift. |
| `tests` | array of DriftTest | yes | The largest rises, worst first (top 10). |

### DriftTest

| Field | Type | Required | Description |
|---|---|---|---|
| `middle_median` | number | yes | Median over the middle third. |
| `newer_median` | number | yes | Median over the newest third. |
| `nodeid` | string | yes | The test. |
| `older_median` | number | yes | Median call-phase seconds over the oldest third of its passing runs. |
| `runs` | integer | yes | Passing runs the medians come from. |

### FileEntry

| Field | Type | Required | Description |
|---|---|---|---|
| `file` | string | yes | Test file, the path part of its tests' nodeids. |
| `pct` | number | yes | `total_seconds` as a percentage of `test_time_seconds`. |
| `total_seconds` | number | yes | Summed whole-protocol time of the file's tests, in seconds. |

### FixtureEntry

| Field | Type | Required | Description |
|---|---|---|---|
| `constant` | boolean | no | Scope-promotion advisor: a function-scoped fixture that produced the same immutable builtin value on every call in every worker, with no per-test teardown or narrower-scoped inputs (checked worker-side), a candidate for `@pytest.fixture(scope="session")`. |
| `count` | integer | yes | Setups across all workers. |
| `name` | string | yes | Fixture name. |
| `projected_saving_seconds` | number | no | Projected wall-time saved by promoting this candidate to session scope: the largest per-worker-session `(calls - 1) * mean_setup`, i.e. the redundant re-setups removed on the worker that benefits most. 0 unless `constant`. |
| `scope` | string | yes | pytest scope: `function`, `class`, `module`, `package` or `session`. |
| `total_seconds` | number | yes | Setup time summed across all workers, in seconds. |

### FixtureGrouping

What fixture-cost-aware dispatch saved: each module or class it kept on one worker, its setups this run, and how many plain chunked dispatch would have caused (an estimate from the chunk size and worker count).

| Field | Type | Required | Description |
|---|---|---|---|
| `groups` | array of GroupEntry | yes | Every group, most saved first. |
| `saved_seconds` | number | yes | Setup seconds saved over every group, against plain chunked dispatch. |

### GateTest

| Field | Type | Required | Description |
|---|---|---|---|
| `duration` | number | yes | Its whole-protocol time (setup + call + teardown), in seconds. |
| `nodeid` | string | yes | The test that gates the wall time. |

### GroupEntry

| Field | Type | Required | Description |
|---|---|---|---|
| `chunked_setups` | integer | yes | About how many times plain chunked dispatch would have set them up. |
| `saved_seconds` | number | yes | `(chunked_setups - setups) * setup_seconds`, never negative. |
| `scope` | string | yes | The module's or class's nodeid. |
| `setup_seconds` | number | yes | One setup's recorded seconds. |
| `setups` | integer | yes | Times its module/class fixtures were set up this run. |
| `tests` | integer | yes | Tests in it this run dispatched. |

### Leak

A test that ended with more threads / open fds than it started: a resource it opened and never released (its own teardown included).

| Field | Type | Required | Description |
|---|---|---|---|
| `fds` | integer | yes | Fds the test opened that are still open after its teardown (0 if only threads leaked). |
| `nodeid` | string | yes | The test that leaked. |
| `threads` | integer | yes | Threads the test created that outlived its teardown (0 if only fds leaked). |

### NetworkAccess

Tests that reached the network, from the worker's network guard.

| Field | Type | Required | Description |
|---|---|---|---|
| `tests` | array of NetworkTest | yes | Those tests, most destinations first (top 20). |
| `tests_with_network` | integer | yes | Tests that connected outside loopback and the allowlist. |

### NetworkTest

| Field | Type | Required | Description |
|---|---|---|---|
| `destinations` | array of string | yes | Destinations it connected to, as `host:port` (with the IP when it was resolved from a name). |
| `nodeid` | string | yes | The test. |

### ParallelEfficiency

Realized parallel speedup measured from an actual run. Unlike `ParallelFloor` (a static pre-run estimate), this is the after-the-fact "why isn't `-n auto` faster?". Only for multi-worker pool runs.

| Field | Type | Required | Description |
|---|---|---|---|
| `efficiency_pct` | number | yes | 100 * realized / ideal: how busy the workers were, up to 100%. |
| `ideal_speedup` | integer | yes | Worker count (`-n`) - the ceiling for a purely CPU-bound suite. |
| `imbalance_pct` | number | yes | 100 * (busiest - idlest) / busiest. High = uneven distribution. |
| `long_pole_seconds` | number | yes | Slowest single test (setup + call + teardown): the hard floor no worker count beats. |
| `realized_speedup` | number | yes | test_time / wall. At most `ideal_speedup`, since each worker runs one test at a time; a wait-bound suite run with `-n` above the core count can realize more than the core count. |
| `workers_busy` | array of WorkerLoad | yes | Busy time summed per worker, descending - the load-balance picture. |

### ParallelFloor

| Field | Type | Required | Description |
|---|---|---|---|
| `gate_tests` | array of GateTest | yes | The tests that exceed the floor, longest first (at most 10). |
| `ideal_share_seconds` | number | yes | A worker's even share of the test time (`test_time_seconds / workers`), in seconds. |
| `longest_seconds` | number | yes | The longest single test (setup + call + teardown), in seconds. |

### TimeoutAdvice

Timeouts that catch a hang without killing a slow-but-healthy test. Each test's own limit is the larger of 3x its slowest recorded passing run and that run plus 1s, rounded up to whole seconds.

| Field | Type | Required | Description |
|---|---|---|---|
| `override_count` | integer | yes | Judged tests whose own limit is above `suite_timeout`: they need a per-test `@pytest.mark.timeout`. |
| `overrides` | array of TimeoutOverride | yes | The highest of those, worst first (top 20). |
| `suite_timeout` | integer | yes | Suggested suite-wide `--timeout`, in seconds: the limit that covers 95% of the judged tests. |
| `tests_judged` | integer | yes | Tests in this run with at least 5 passing runs in the history. |

### TimeoutOverride

| Field | Type | Required | Description |
|---|---|---|---|
| `nodeid` | string | yes | The test. |
| `runs` | integer | yes | Passing runs that slowest time comes from. |
| `slowest_seconds` | number | yes | Its slowest recorded passing call-phase time, in seconds (the phase `--timeout` limits; fixture setup/teardown not included). |
| `timeout` | integer | yes | Its suggested `@pytest.mark.timeout`, in seconds. |

### WaitBound

| Field | Type | Required | Description |
|---|---|---|---|
| `tests` | array of WaitTest | yes | The biggest waiters, most waiting first (top 50): tests of at least 0.2s that spent 60% or more of their time waiting. |
| `wait_pct` | number | yes | `wait_seconds` as a percentage of `test_time_seconds`. |
| `wait_seconds` | number | yes | Test time spent waiting rather than on CPU (`test_time_seconds - cpu_time_seconds`). |

### WaitTest

| Field | Type | Required | Description |
|---|---|---|---|
| `duration` | number | yes | Its whole-protocol time (setup + call + teardown), in seconds. |
| `nodeid` | string | yes | The waiting test. |
| `wait` | number | yes | The part of `duration` not spent on CPU, in seconds. |

### WasteTest

| Field | Type | Required | Description |
|---|---|---|---|
| `also_covered_by` | integer | yes | Distinct OTHER tests that between them also cover those lines. |
| `covered_lines` | integer | yes | Lines this test covered, all shared with at least one other test. |
| `duration` | number | yes | Its whole-protocol time (setup + call + teardown), in seconds. |
| `nodeid` | string | yes | The redundant test. |

### WorkerLoad

| Field | Type | Required | Description |
|---|---|---|---|
| `busy_seconds` | number | yes | Test time this worker spent running tests, in seconds. |
| `tests` | integer | yes | Tests this worker ran. |
| `worker` | string | yes | Worker id (`gw0`, `gw1`, ...), or `serial` for tests with no recorded worker. |
