<!-- Generated from the Rust type by `cargo test -p rstest-cli schema`. Do not edit by hand; regenerate with RSTEST_BLESS_SCHEMAS=1. -->

## Doctor report

Source: `--doctor-json`

| Field | Type | Required | Description |
|---|---|---|---|
| `coverage_waste` | CoverageWaste or null | no | Slow tests that can be deleted together without dropping any covered line - delete/merge candidates. `None` unless THIS run wrote a per-test coverage index (`--cov --cov-context=test` alongside `--doctor`; an index left by an earlier run is ignored as stale) and at least one test qualified. |
| `cpu_time_seconds` | number | yes | Sum of call-phase CPU time, over tests where it was measured. |
| `fixtures` | array of FixtureEntry | yes |  |
| `leaks` | array of Leak | no | Tests that leaked threads / fds (net positive after teardown). Empty unless leak-check instrumentation ran (`--doctor` / `--fail-on-leak`). |
| `parallel_efficiency` | ParallelEfficiency or null | no |  |
| `parallel_floor` | ParallelFloor or null | no |  |
| `rstest_version` | string | yes |  |
| `schema` | integer | yes |  |
| `slowest_files` | array of FileEntry | yes |  |
| `test_time_seconds` | number | yes |  |
| `tests` | integer | yes |  |
| `wait_bound` | WaitBound or null | no |  |
| `wall_seconds` | number | yes |  |
| `workers` | integer | yes |  |

### CoverageWaste

Slow tests that add no unique coverage: chosen greedily (slowest first) so that deleting ALL of them together still leaves every covered line covered by some kept test. Of two tests covering identical lines, only one is listed. Pure suite bloat on the time axis.

| Field | Type | Required | Description |
|---|---|---|---|
| `redundant_tests` | integer | yes | Size of the deletable set (`tests` shows the slowest of them). |
| `tests` | array of WasteTest | yes | The slowest redundant tests, worst first (capped). |
| `wasted_seconds` | number | yes | Sum of the durations of the whole deletable set (not just the shown ones) - the time reclaimable by pruning them together. |

### FileEntry

| Field | Type | Required | Description |
|---|---|---|---|
| `file` | string | yes |  |
| `pct` | number | yes |  |
| `total_seconds` | number | yes |  |

### FixtureEntry

| Field | Type | Required | Description |
|---|---|---|---|
| `count` | integer | yes |  |
| `name` | string | yes |  |
| `scope` | string | yes |  |
| `total_seconds` | number | yes |  |

### GateTest

| Field | Type | Required | Description |
|---|---|---|---|
| `duration` | number | yes |  |
| `nodeid` | string | yes |  |

### Leak

A test that ended with more threads / open fds than it started — a resource it opened and never released (its own teardown included).

| Field | Type | Required | Description |
|---|---|---|---|
| `fds` | integer | yes | Net open fds leaked (0 if only threads leaked). |
| `nodeid` | string | yes |  |
| `threads` | integer | yes | Net threads leaked (0 if only fds leaked). |

### ParallelEfficiency

Realized parallel speedup measured from an actual run. Unlike `ParallelFloor` (a static pre-run estimate), this is the after-the-fact "why isn't `-n auto` faster?". Only for multi-worker pool runs.

| Field | Type | Required | Description |
|---|---|---|---|
| `efficiency_pct` | number | yes | 100 * realized / ideal. >100% signals wait-bound overlap. |
| `ideal_speedup` | integer | yes | Worker count (`-n`) - the ceiling for a purely CPU-bound suite. |
| `imbalance_pct` | number | yes | 100 * (busiest - idlest) / busiest. High = uneven distribution. |
| `long_pole_seconds` | number | yes | Slowest single test: the hard floor no worker count beats. |
| `realized_speedup` | number | yes | test_time / wall. May exceed `ideal_speedup` for wait-bound suites, where overlapping sleeps/IO run more tests at once than there are cores. |
| `workers_busy` | array of WorkerLoad | yes | Busy time summed per worker, descending - the load-balance picture. |

### ParallelFloor

| Field | Type | Required | Description |
|---|---|---|---|
| `gate_tests` | array of GateTest | yes |  |
| `ideal_share_seconds` | number | yes |  |
| `longest_seconds` | number | yes |  |

### WaitBound

| Field | Type | Required | Description |
|---|---|---|---|
| `tests` | array of WaitTest | yes |  |
| `wait_pct` | number | yes |  |
| `wait_seconds` | number | yes |  |

### WaitTest

| Field | Type | Required | Description |
|---|---|---|---|
| `duration` | number | yes |  |
| `nodeid` | string | yes |  |
| `wait` | number | yes |  |

### WasteTest

| Field | Type | Required | Description |
|---|---|---|---|
| `also_covered_by` | integer | yes | Distinct KEPT tests (not themselves in the deletable set) that between them also cover those lines. |
| `covered_lines` | integer | yes | Product lines this test covered, all also covered by a kept test. |
| `duration` | number | yes |  |
| `nodeid` | string | yes |  |

### WorkerLoad

| Field | Type | Required | Description |
|---|---|---|---|
| `busy_seconds` | number | yes |  |
| `tests` | integer | yes |  |
| `worker` | string | yes |  |
