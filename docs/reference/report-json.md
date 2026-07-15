# Report JSON

```console
$ rstest --report-json results.json
```

writes a per-test outcome snapshot after the run. The schema is stable and
intended for tooling (dashboards, flake tracking, result diffing).

## Shape

```json
{
  "meta": {
    "runner": "rstest", "schema": 5, "exitstatus": 0,
    "counts": { "passed": 12, "failed": 0, "errors": 0, "skipped": 1,
                "xfailed": 0, "xpassed": 0, "flaky": 0, "quarantined": 0,
                "collect_errors": 0 },
    "duration_seconds": 4.21, "started_at_epoch": 1765500000,
    "workers": 8, "argv": ["rstest", "-n", "8"]
  },
  "collect_errors": [],
  "tests": {
    "tests/test_login.py::test_ok": {
      "setup": "passed",
      "call": "passed",
      "teardown": "passed",
      "duration": 0.0123,
      "lineno": 14
    },
    "tests/test_login.py::test_skipped_one": {
      "setup": "skipped",
      "teardown": "passed",
      "skip_reason": "needs postgres"
    }
  }
}
```

Per-test fields (absent when not applicable):

| Field | Type | Meaning |
|---|---|---|
| `setup` / `call` / `teardown` | `"passed"` / `"failed"` / `"skipped"` | phase outcomes; a skipped test has no `call` |
| `duration` | seconds | call-phase wall time, 4 decimal places |
| `lineno` | int | 0-based source line of the test (pytest `report.location`); omitted when pytest reports none. The file is the nodeid's path |
| `wasxfail` | `true` | the test was an expected failure (xfail/xpass) |
| `skip_reason` | string | first 200 chars |
| `flaky` | `true` | passed only after [`--reruns`](cli.md#-reruns-n) retries |
| `quarantined` | `true` | failed, but matched the [`--quarantine`](cli.md#-quarantine-file) list — non-fatal |
| `longrepr` | string | failure text (assertion repr / traceback), failures only, capped at 20k chars |
| `crashed` | `true` | the failure was fabricated by the orchestrator — worker crash or `--worker-timeout` kill; pytest never reported it. `longrepr` says which |
| `worker` | `"gw2"` | worker that produced the final outcome (pool runs only) |

`meta.schema` is the document version, currently `5`. History: `1` was
the unversioned original (phases, `duration`, `wasxfail`,
`skip_reason`, `flaky`, `worker`); `2` added `longrepr` and `crashed`
plus the version field itself; `3` added the envelope — `counts`
(pytest-accounting outcome totals, all keys always present, identical
to the terminal summary line's numbers: never re-derive them by
walking `tests`), `duration_seconds`, `started_at_epoch`, `workers`,
and `argv`; `4` added per-test `lineno`; `5` added per-test
`quarantined` and the `quarantined` counts key. Parse it —
incompatible changes will bump it.

`collect_errors` lists the file paths of collectors that failed outright.

At a [monorepo](../guides/monorepo.md) root, the document is the MERGED
result of every project: test keys are root-relative nodeids
(`libs/core/tests/test_x.py::test_y`), collect-error paths are prefixed
the same way, and `meta.projects` maps each project to
`{"exitstatus": N, "counts": {...}}` or `{"skipped": true}`;
`meta.counts` holds the grand totals across projects.

For *suite-health* data (timings analysis, wait-bound tests, fixture
costs) use [`--doctor-json`](cli.md#-doctor-json-path) instead; the two
schemas are independent.

## Discovery JSON

Pairing `--report-json` with `--collect-only` (or `--co`) collects the
suite **without running it** and writes a discovery document — the
machine-readable surface editor/CI integrations consume to build a test
tree, instead of parsing pytest's text output.

```console
$ rstest --collect-only --report-json discovery.json
```

```json
{
  "meta": {
    "runner": "rstest", "kind": "discovery", "schema": 1,
    "count": 3, "rootdir": "/abs/path/to/project"
  },
  "tests": [
    { "nodeid": "tests/test_login.py::test_ok",
      "file": "/abs/path/to/project/tests/test_login.py",
      "lineno": 14, "markers": [] },
    { "nodeid": "tests/test_login.py::test_slow",
      "file": "/abs/path/to/project/tests/test_login.py",
      "lineno": 22, "markers": ["serial"] },
    { "nodeid": "tests/test_api.py::test_q[1]",
      "file": "/abs/path/to/project/tests/test_api.py",
      "lineno": 9, "markers": ["parametrize", "xdist_group"] }
  ],
  "collect_errors": [
    { "path": "tests/test_broken.py", "longrepr": "ImportError: ..." }
  ]
}
```

This is a **distinct document** from the run snapshot above — its own
`meta.kind` (`"discovery"`) and `meta.schema` (currently `1`).

Per-test fields:

| Field | Type | Meaning |
|---|---|---|
| `nodeid` | string | pytest node id (parametrized variants are separate entries) |
| `file` | string | absolute path to the test file (editor-ready URI) |
| `lineno` | int / `null` | 0-based source line; `null` when pytest reports none |
| `markers` | string[] | every pytest marker **name** on the item — own and inherited from class/module (`pytestmark`), sorted and de-duplicated. Includes `serial`, `flaky`, `skip`, `xfail`, `parametrize`, `xdist_group`, and any custom marks. Names only (no args/reason) |

`collect_errors` lists files that failed to import/collect, with their
`longrepr`. The process exit code matches pytest's collection result
(`2` when any collector errored).

Discovery needs a single pytest session, so at a
[monorepo](../guides/monorepo.md) root it is **refused** (each project has
its own rootdir and config — one session can't represent them). Run it once
per project instead, with the working directory set to that project:

```console
$ cd libs/core && rstest --collect-only --report-json discovery.json
```

`meta.rootdir` is then that project's root, and `file` paths are absolute
within it.

## Streaming JSON

```console
$ rstest --output json
```

is a **third, separate** shape: a live newline-delimited JSON stream on
stdout — one object per line, emitted as the run proceeds rather than a
single document written at the end. It's built for editors and CI tooling
that update a test tree incrementally. See
[`--output`](cli.md#-output-dotsverbosebargithubjson) for the flag.

Two event kinds, discriminated by `event`:

```json
{"event": "testreport", "nodeid": "tests/test_api.py::test_get", "when": "call", "outcome": "passed", "duration": 0.0123, "wasxfail": false, "lineno": 41, "worker": "gw2"}
{"event": "sessionfinish", "exitstatus": 1, "duration": 4.21, "counts": {"passed": 28, "failed": 1, "errors": 0, "skipped": 0, "xfailed": 0, "xpassed": 0, "flaky": 0, "quarantined": 0, "collect_errors": 0}}
```

One `testreport` is emitted **per phase** (`setup`, `call`, `teardown`), so
a single test produces up to three lines — mirroring pytest's own report
granularity. Fields:

| Field | Type | Meaning |
|---|---|---|
| `event` | string | always `"testreport"` |
| `nodeid` | string | pytest node id |
| `when` | string | phase: `setup` / `call` / `teardown` |
| `outcome` | string | `passed` / `failed` / `skipped` |
| `duration` | float | phase duration in seconds (rounded to 1e-4) |
| `wasxfail` | bool | the outcome was an expected failure / unexpected pass |
| `lineno` | int | 0-based source line; **omitted** when pytest reports none |
| `worker` | string | `gwN` — **pool runs only**; absent under `-n 0` |
| `longrepr` | string | failure traceback; **present only on `failed`** |

The stream closes with exactly one `sessionfinish`:

| Field | Type | Meaning |
|---|---|---|
| `event` | string | always `"sessionfinish"` |
| `exitstatus` | int | pytest-compatible exit code |
| `duration` | float | total wall time in seconds |
| `counts` | object | outcome tallies — the same keys and accounting as the snapshot's `meta.counts` |

Unlike the snapshot and discovery documents, the stream is **not versioned**
(no `schema` field) and should be treated as experimental: consume by
`event` kind and tolerate added fields. No banner, footer, or human summary
is interleaved, so every line parses on its own.

## Doctor JSON

```console
$ rstest --doctor-json doctor.json
```

writes the [`--doctor`](cli.md#-doctor) suite-health analysis as a single
versioned document — the machine-readable surface for CI trending (diff two
runs to catch new long-poles, fixture-cost growth, or wait-time
regressions; a ready-made recipe is in the
[CI quickstart](../guides/ci-quickstart.md#suite-health-trending-with-doctor)).
It is a **separate document** from the run snapshot above; combine with
`--doctor` to also print the human report.

```json
{
  "schema": 2,
  "rstest_version": "0.2.0",
  "workers": 8,
  "wall_seconds": 68.4,
  "tests": 2048,
  "test_time_seconds": 412.9,
  "cpu_time_seconds": 120.3,
  "wait_bound": {
    "wait_seconds": 292.6,
    "wait_pct": 70.8,
    "tests": [
      { "nodeid": "tests/test_api.py::test_slow_remote", "duration": 4.81, "wait": 4.72 }
    ]
  },
  "parallel_floor": {
    "longest_seconds": 84.1,
    "ideal_share_seconds": 51.6,
    "gate_tests": [
      { "nodeid": "tests/test_e2e.py::test_full_flow", "duration": 84.1 }
    ]
  },
  "parallel_efficiency": {
    "realized_speedup": 6.04,
    "ideal_speedup": 8,
    "efficiency_pct": 75.4,
    "workers_busy": [
      { "worker": "gw3", "busy_seconds": 68.0, "tests": 240 },
      { "worker": "gw1", "busy_seconds": 41.2, "tests": 268 }
    ],
    "imbalance_pct": 39.4,
    "long_pole_seconds": 84.1
  },
  "fixtures": [
    { "name": "pg_database", "scope": "session", "count": 8, "total_seconds": 31.2 }
  ],
  "slowest_files": [
    { "file": "tests/test_e2e.py", "total_seconds": 84.1, "pct": 20.4 }
  ]
}
```

Top-level fields:

| Field | Type | Meaning |
|---|---|---|
| `schema` | int | document version, currently `2` |
| `rstest_version` | string | the rstest version that wrote it |
| `workers` | int | worker count for this run (`-n`) |
| `wall_seconds` | float | total wall-clock time; **depends on worker count** — compare across runs only at equal `-n` |
| `tests` | int | number of tests with a recorded duration |
| `test_time_seconds` | float | summed per-test call durations (worker-count-independent — the stable trending metric) |
| `cpu_time_seconds` | float | summed call-phase CPU time, over tests where it was measured |
| `wait_bound` | object / `null` | wait-bound analysis; **`null`** unless CPU time was measured and waiting is significant (`wait_pct ≥ 20%` and `wait_seconds ≥ 1`) |
| `parallel_floor` | object / `null` | parallel-floor analysis; **`null`** unless the longest test exceeds the ideal per-worker share |
| `parallel_efficiency` | object / `null` | realized parallel speedup and per-worker load; **`null`** unless the run used more than one worker (`workers > 1`) |
| `fixtures` | array | fixture timings, slowest first (≤ 50) |
| `slowest_files` | array | per-file totals, slowest first (≤ 20) |

`wait_bound` (wall ≫ CPU — tests that wait rather than compute):

| Field | Type | Meaning |
|---|---|---|
| `wait_seconds` | float | total `test_time − cpu_time` |
| `wait_pct` | float | `wait_seconds` as a percent of `test_time_seconds` |
| `tests` | array | the worst offenders (`duration ≥ 0.2s` and ≥ 60% waiting), by wait descending (≤ 50): `{nodeid, duration, wait}` — all seconds |

`parallel_floor` (the tests that cap any `-n`):

| Field | Type | Meaning |
|---|---|---|
| `longest_seconds` | float | duration of the single longest test |
| `ideal_share_seconds` | float | `test_time_seconds / workers` — the per-worker floor if work split perfectly |
| `gate_tests` | array | up to 10 tests longer than that share: `{nodeid, duration}` (seconds) |

`parallel_efficiency` (realized speedup vs the worker budget, measured from
this run — `null` for single-worker runs):

| Field | Type | Meaning |
|---|---|---|
| `realized_speedup` | float | `test_time_seconds / wall_seconds`. May exceed `ideal_speedup` for wait-bound suites (overlapping sleeps/IO run more tests at once than there are cores) |
| `ideal_speedup` | int | worker count (`-n`) — the ceiling for a purely CPU-bound suite |
| `efficiency_pct` | float | `100 × realized_speedup / ideal_speedup`; over 100% signals wait-bound overlap |
| `workers_busy` | array | busy time per worker, busiest first (≤ 8 shown): `{worker, busy_seconds, tests}`. Tests with no recorded worker are bucketed as `"serial"` |
| `imbalance_pct` | float | `100 × (busiest − idlest) / busiest` — load spread across workers |
| `long_pole_seconds` | float | slowest single test — the hard floor no worker count beats |

`fixtures[]`: `{name, scope, count, total_seconds}` — fixture name, pytest
scope, setup count, summed setup time. `slowest_files[]`:
`{file, total_seconds, pct}` — `pct` is the file's share of
`test_time_seconds`.

`schema` history: `1` was the original (`wall_seconds`, `test_time_seconds`,
`cpu_time_seconds`, `wait_bound`, `parallel_floor`, `fixtures`,
`slowest_files`); `2` added the `parallel_efficiency` object.

`schema` aside, all times are raw seconds (no rounding) — round in your
consumer. Increment-only: incompatible changes bump `schema`.

## Migrate-check JSON

```console
$ rstest --migrate-check-json migrate.json
```

writes the [`--migrate-check`](cli.md#-migrate-check) parallel-readiness report
as a single versioned document — the machine-readable surface for CI gating
(fail the build when a new parallel-unsafe test appears) and for tooling that
renders the findings. It is a **separate document** from the run snapshot;
pass `--migrate-check` too to also print the human report. The flag implies
`--migrate-check`.

```json
{
  "meta": { "runner": "rstest", "kind": "migrate-check", "schema": 1 },
  "ready": false,
  "tests_collected": 4309,
  "will_bail_count": 64,
  "unstable_ids": [
    {
      "site": "tests/test_complex.py::test_complex_with_special_methods",
      "kinds": { "address": 12 },
      "will_bail": true,
      "allowed": false,
      "sample": "<ComplexWithIndex object at 0x10ae4e660>-(10+0j)",
      "fix": "give this parametrize a stable ids= (e.g. ids=[c.name for c in cases])"
    }
  ],
  "parallel": {
    "ran": true,
    "ready": false,
    "preexisting": 404,
    "findings": [
      {
        "nodeid": "tests/test_callback_warning.py::test_warns_when_unsupported",
        "verdict": "ISOLATION / CO-LOCATION",
        "why": "passes serial, fails under both load and loadfile — co-located state leak",
        "fix": "reset the leaked global state per test; stopgap @pytest.mark.serial",
        "allowed": false,
        "polluter": { "kind": "other_file", "file": "tests/test_other.py" }
      }
    ]
  }
}
```

Top-level fields:

| Field | Type | Meaning |
|---|---|---|
| `meta` | object | `{runner, kind, schema}`; `schema` is the document version, currently `1` |
| `ready` | bool | `true` only when the suite is parallel-ready: no unstable ids and no parallelism-specific failures |
| `tests_collected` | int | tests seen across the two collection passes (their union) |
| `will_bail_count` | int | count of unstable ids that are per-process (address / uuid) — these force `-n 0` |
| `unstable_ids` | array | the unstable-id findings, grouped by parametrize site (see below) |
| `parallel` | object / `null` | the parallel-classification phase; **`null`** when it was skipped (a WILL-bail id stopped the run before it) |

`unstable_ids[]` — one entry per parametrize site with run-to-run unstable ids:

| Field | Type | Meaning |
|---|---|---|
| `site` | string | the nodeid up to the `[…]` parametrize segment |
| `kinds` | object | count of each instability class at this site: `address`, `uuid`, `time`, `other` |
| `will_bail` | bool | `true` if any id here is per-process (`address`/`uuid`) — i.e. forces `-n 0` |
| `allowed` | bool | matched a `--migrate-allow` substring (excluded from the gate) |
| `sample` | string | a sample unstable param value from this site |
| `fix` | string | the upstream fix (give the parametrize a stable `ids=`) |

`parallel` (present only when the parallel phase ran):

| Field | Type | Meaning |
|---|---|---|
| `ran` | bool | whether the `-n auto` classification actually executed |
| `ready` | bool | `true` when the parallel run was green |
| `preexisting` | int | tests already failing at `-n 0` (a pre-existing bug, not a migration concern) |
| `findings` | array | the classified parallel-only failures (see below) |

`parallel.findings[]`:

| Field | Type | Meaning |
|---|---|---|
| `nodeid` | string | the failing test |
| `verdict` | string | `NOT PARALLEL-SPECIFIC` / `INTRINSIC FLAKE` / `ORDER DEPENDENCY` / `WALL-CLOCK / LOAD-SENSITIVE` / `ISOLATION / CO-LOCATION` |
| `why` | string | the evidence behind the verdict |
| `fix` | string | the recommended fix plus rstest stopgap |
| `allowed` | bool | matched a `--migrate-allow` substring (excluded from the gate) |
| `polluter` | object / `null` | for ORDER-DEPENDENCY / ISOLATION: `{kind: "other_file", file}`, `{kind: "same_file", file}`, or `{kind: "not_reproducible"}`; `null` otherwise |

The exit code is **not** in the document — read it from the process: non-zero
when any non-allow-listed WILL-bail id or parallel finding exists. Increment-
only: incompatible changes bump `meta.schema`.
