# Suite diagnostics

`rstest --doctor` runs your suite normally, then answers the question every
slow suite raises: *where does the time actually go?* The diagnosis comes
from data the runner already owns (per-test wall time, per-test CPU time,
and per-fixture setup time), so it adds almost nothing to the run.

```console
$ rstest --doctor          # a suite that "feels slow"
$ rstest -n 4 --doctor     # diagnosing parallel scaling
```

## A real report

```text
================== rstest doctor ==================
4442 tests, 185.8s test time (wall 67.7s, 8 workers)

WAIT-BOUND: 95% of test time (176.5s) is waiting, not computing (sleeps / IO / timeouts).
    54.20s waiting of   54.25s  tests/test_proxy_functional.py::test_proxy_https_multi_conn_limit
    10.97s waiting of   10.97s  tests/test_proxy_functional.py::test_proxy_https_connect
  ... and 27 more

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

(That's [aiohttp]'s real suite, trimmed for the page: WAIT-BOUND lists up
to 8 tests before its `... and N more` line, and the `startup:` line and the
[PARALLEL EFFICIENCY](#parallel-efficiency) section, which appears in any
`-n ≥ 2` run, are cut. One file is 81% of total test time, almost all of it
waiting on 10-second proxy timeouts.)

The `4442 tests` count is tests with a **recorded call duration**, which is
what doctor analyzes: slightly fewer than the 4,469 the suite *collects*
([benchmarks](../reference/benchmarks.md)), because skips and zero-duration
tests contribute no timing.

[aiohttp]: https://github.com/aio-libs/aiohttp

## What "test time" means

Every number in the report counts a test's **whole protocol**: fixture setup,
the test call, and fixture teardown. That is the time the test held its
worker, so a suite whose cost lives in function fixtures reads correctly:
its test time, worker load, long pole, slowest files and realized speedup
all include the fixture work. CPU time is measured over the same span. On
Linux and macOS it includes CPU used by child processes the test waited for (a
CLI run with `subprocess.run`), so a test that shells out to a CPU-heavy tool
reads as computing, not waiting. On Windows such a test reads as waiting
([no child CPU time](windows.md#diagnostics)).

A single-worker run (`-n 0` or `-n 1`) reports `1 worker`.

## Reading each section

### WAIT-BOUND

Compares each test's wall time with its CPU time. A test whose wall time
vastly exceeds its CPU time isn't computing: it's sleeping, waiting on a
socket, or waiting out a timeout. These tests waste wall-clock no matter
how fast the runner is; fixing them (mock the clock, shrink the timeout,
use event-driven waits) is usually the single biggest speedup available in
a suite.

The section appears when waiting is at least 20% of test time and at least
1s, and lists tests that spent 60% or more of at least 0.2s waiting. Waiting
in a fixture (a `time.sleep` before `yield`, a server that takes a while to
come up) counts like waiting in the test body.

In profiling of popular open-source suites, this is the dominant pattern:
rich spends 74% of its test time in three `sleep()`-based tests; aiohttp
spends 95% waiting on proxy timeouts.

### PARALLEL FLOOR

No worker count can finish faster than the longest single test (the
[parallel floor](../concepts/glossary.md#parallel-floor)). The section appears
when that test clearly exceeds the ideal per-worker share: test time ÷
workers, or 1 second if that is larger, plus 10% slack. It names up to 10
gate tests above that threshold; splitting or shrinking them lowers the
floor.

### PARALLEL EFFICIENCY

Where PARALLEL FLOOR is a static lower bound on wall time, this is the *realized* speedup
measured from the run just finished: `test time / wall`, compared against
the worker count. "1.5× realized of 4× possible (38%)" means the run
converted only 38% of its worker budget into wall-clock savings: the
direct answer to "why isn't `-n auto` faster?".

Two things cap it, both named in the section:

- **long pole**: the slowest single test. When it clearly exceeds the ideal
  per-worker share, it is also the PARALLEL FLOOR above.
- **worker load**: busy time summed per worker, plus the imbalance
  between the busiest and idlest worker. A high imbalance means the
  scheduler couldn't spread the work evenly (usually a few long tests
  pinned to one worker); split them, or switch to the default `--dist load`
  if you're on `loadfile` or `loadscope`.

Efficiency is measured against the worker count, so it stays at or below
100%. On a wait-bound suite, set `-n` above the core count: overlapping
sleeps/IO run more tests at once than there are cores, and the realized
speedup climbs past the core count (see
[Wait-bound suites](wait-bound.md)). Only emitted for multi-worker runs.

### FIXTURE HOTSPOTS

Total setup time per fixture, summed over every worker (the heading reads
`setup time across all workers` on a pool run and `setup time` on a
single-worker run), with two pieces of advice:

- A *function-scoped* fixture that ran 20 or more times and cost at least
  1s in total is a candidate for a wider scope: one real-world suite re-parsed
  the same RSA key 206 times (≈20% of its total runtime) in what could
  have been a session fixture.
- A *session-scoped* fixture that ran more than once ran **once per
  worker**: the report reminds you it must be safe to duplicate.

### SCOPE-PROMOTION CANDIDATES

The advisor upgrade to the hotspot heuristic. Under `--doctor`, rstest
fingerprints each function-scoped fixture's **produced value** on every
call. A fixture that returned the *same immutable value every time* (in
every worker), with no per-test teardown and only session-scoped inputs,
is a candidate for `scope="session"`, and the report attaches a concrete
number:

```text
SCOPE-PROMOTION CANDIDATES (same value every call; promote to session scope):
  ~  0.52s saved     206x  feature_flags  <- @pytest.fixture(scope="session")
  (check the fixture body for side effects before promoting)
```

A session-scoped fixture runs once per worker session, so every call
after a worker's first is a redundant re-setup. Each worker session
totals its own `(calls − 1) × mean setup time`. The projected saving is
the largest of those totals: the wall time saved on the worker that
benefits most, whether the calls were spread across the pool or pinned to
one worker by `--dist loadfile`.

A fixture is flagged only when some worker session ran it at least twice.
If every worker (including a respawned one) called it once, that is not
evidence. Candidates are listed even below the hotspot threshold (from
0.01s up), biggest saving first.

The check is deliberately narrow. Only **immutable builtin values**
qualify: `str`, `bytes`, `int`, `float`, `bool`, `complex`, and tuples or
frozensets built from them (up to 10,000 items in total). Looking at any
other object can't prove it is safe to share, so these are **never**
flagged:

- mutable values, even when they look the same each call: a fresh `[]`
  or `{}` is usually exactly what each test must get its own copy of;
- any other object: user classes, settings models, mocks, numpy arrays,
  DataFrames, lazy objects. rstest never calls their `repr`, so `--doctor`
  runs no extra user code;
- `None`, since side-effect fixtures (reset a global, truncate tables)
  return it every call;
- a fixture with per-test teardown: a `yield` fixture, or one that calls
  `request.addfinalizer`;
- a fixture that depends on anything narrower than session scope
  (`monkeypatch`, `tmp_path`, a per-test database), whether as an argument
  or fetched with `request.getfixturevalue(...)` in its body: promoting it
  would raise `ScopeMismatch`, and its per-test effects are the point;
- a fixture that returned a different value in different workers (say,
  one built from `request.module` under `--dist loadfile`): the value
  depends on which tests a worker got;
- a fixture whose setup failed or skipped;
- a `@pytest.mark.parametrize` argument, which pytest serves through an
  internal fixture there is nothing to promote.

So a fixture returning a settings object or a key is not suggested even
when sharing it would be fine; the advisor only speaks when it is sure
about the value. It still can't see side effects that leave no trace
(writing a file, setting a global), so treat it as *advice* and check the
fixture body before promoting.

### SLOWEST FILES

Test time aggregated by file: where to look first, and the input for
deciding what to split under `--dist load`.

### COVERAGE WASTE

Slow tests that add **no unique coverage**: every line each one executes is
also executed by some other test that is kept, so the flagged tests can all be
deleted or merged together without dropping a single covered line. Tests that
duplicate each other are picked slowest first, so of two tests with identical
coverage only the slower one is flagged. This is the "which time is *wasted*"
counterpart to SLOWEST FILES.

```text
COVERAGE WASTE: 18.4s across 3 slow test(s) that cover no line another test doesn't also cover (delete/merge candidates):
    12.10s  240 line(s), all shared with 4 other test(s)  tests/test_api.py::test_end_to_end_slow
     4.30s   88 line(s), all shared with 2 other test(s)  tests/test_api.py::test_variant_b
```

It needs per-test coverage from the **same run**, so run the doctor with
coverage and per-test contexts:

```console
$ rstest --doctor --cov=. --cov-context=test
```

An index left by an earlier run (or restored from a cache) is never used: the
tests or code may have changed since, and a stale "fully shared" verdict could
recommend deleting a test that is now the only one covering some line. Without
this run's index the section is simply omitted. Only tests that passed count,
either as candidates or as the other coverers. Test files are identified by
your `python_files` patterns, so a product module holding doctests
(`--doctest-modules`) still counts as product code. Only tests slow enough to
matter are flagged (a fast redundant test frees no meaningful time when
deleted).

### RESOURCE LEAKS

Tests that ended with more live threads or open file descriptors than they
started: a resource opened and never released, its own teardown included.

```text
RESOURCE LEAKS (threads/fds a test created, still open after its teardown):
  +3 threads  tests/test_pool.py::test_executor
  +5 fds  tests/test_io.py::test_reader
  a test opened a thread/fd it never released; leaked state can flake later
  tests (reset it, or close in teardown).
```

Only appears when something leaked. A leaked thread/fd is shared state that can
flake a *later* test, so this is the first place to look for order-dependent
flakiness. Full model, false-positive cases, and fixes:
[Resource leaks](resource-leaks.md). To make it a CI gate, use
[`--fail-on-leak`](../reference/cli.md#-fail-on-leak).

### startup

A one-line summary under the header reports how long spawning the worker pool
took: wall from spawn to every worker's first event (import + collection start):

```text
startup: 0.22s spawning 16 workers (54% of wall) — try --fork-pool to prewarm the pool
```

When that startup is a real fraction of a short multi-worker run on Unix, the
line suggests [`--fork-pool`](../reference/cli.md#-fork-pool), which imports the
vendored pytest core once in a zygote and forks the workers off it instead of
re-importing per worker. The hint is dropped once the run already uses
`--fork-pool`, on Windows ([no fork pool](windows.md#worker-startup)), and on
single-worker runs. It's a fixed per-run tax,
so it matters most on short / cold suites and is negligible on long ones.

## JSON output for CI

`rstest --doctor-json doctor.json` writes the same analysis as a versioned
JSON document ([field reference and schema version](../reference/report-json.md#doctor-json)):

```console
$ rstest --doctor-json doctor.json
```

The document holds totals (tests, test time, CPU time, wall, workers, pool
`startup_seconds`), the wait-bound test list, parallel-floor gate tests,
parallel-efficiency (realized speedup and per-worker load), fixture timings
(each with `constant` and `projected_saving_seconds` for the scope-promotion
advisor), slowest files, and the coverage-waste list (`coverage_waste`, `null`
unless the same run collected per-test coverage with `--cov --cov-context=test`).
Combine with `--doctor` to also print the human report. See
[Doctor JSON](../reference/report-json.md#doctor-json) for the full field
schema.

### Suite-health trending in CI

Archive `doctor.json` per run and compare a PR's report against the main
branch's to see what the PR added: new wait-bound tests, new parallel-floor
gate tests, fixture cost growth. No extra tooling is needed; the document
already holds totals, wait-bound tests, gate tests, and fixture costs by name.

On GitHub Actions the baseline can travel through the actions cache. Pushes to
main save it; PR jobs only restore it (GitHub lets a PR read its base branch's
cache entries):

```yaml
      - name: test (with doctor)
        run: rstest -n auto --junitxml junit.xml --doctor-json doctor.json

      # PRs only restore, so a PR never reads back its own earlier save:
      # the latest baseline always comes from main.
      - uses: actions/cache/restore@v6
        if: github.event_name == 'pull_request'
        with:
          path: doctor-baseline.json
          key: doctor-baseline-${{ github.sha }}
          restore-keys: doctor-baseline-

      - name: compare against main
        if: github.event_name == 'pull_request'
        run: |
          [ -f doctor-baseline.json ] || { echo "no baseline yet"; exit 0; }
          {
            echo "## Suite health vs main"
            jq -rn --slurpfile a doctor-baseline.json --slurpfile b doctor.json '
              def d(f): ($b[0][f] - $a[0][f]);
              "tests: \($a[0].tests) -> \($b[0].tests)",
              "test time: \($a[0].test_time_seconds|round)s -> \($b[0].test_time_seconds|round)s (\(d("test_time_seconds")|round)s)",
              "wait-bound: \($a[0].wait_bound.wait_pct // 0|round)% -> \($b[0].wait_bound.wait_pct // 0|round)%"
            '
            echo "new wait-bound tests:"
            comm -13 \
              <(jq -r '.wait_bound.tests[]?.nodeid' doctor-baseline.json | sort) \
              <(jq -r '.wait_bound.tests[]?.nodeid' doctor.json | sort) \
              | sed 's/^/- /' || true
          } >> "$GITHUB_STEP_SUMMARY"

      - name: refresh baseline
        if: github.ref == 'refs/heads/main'
        run: cp doctor.json doctor-baseline.json

      - uses: actions/cache/save@v6
        if: github.ref == 'refs/heads/main'
        with:
          path: doctor-baseline.json
          key: doctor-baseline-${{ github.sha }}
```

Two practical notes:

- **Don't fail the job on timing deltas.** CI runners are noisy;
  single-digit-percent changes in `test_time_seconds` are jitter. Treat
  the summary as a review aid; alert only on structural signals (new
  wait-bound tests, a fixture's `count` doubling, a new parallel-floor
  gate test) or on large sustained moves. To enforce a threshold, use
  [`--doctor-fail-on`](#gating-a-pr-on-doctor-metrics).
- **Compare like with like.** `wall_seconds` depends on the worker
  count; if runner sizes vary, compare `test_time_seconds` (summed test
  time) and per-test signals instead.

## Markdown output and GitHub job summaries

Under GitHub Actions, any doctor run appends the report to
`$GITHUB_STEP_SUMMARY` automatically: `rstest --doctor-json doctor.json`
in a workflow puts the analysis on the run page with no extra step. On
Buildkite, the same markdown is piped to `buildkite-agent annotate` as an
info annotation. Both are best-effort: an unwritable summary path or a
missing agent prints a warning on stderr, and the exit code and every
requested report file stay as they would be without it.

To also write the markdown to a file of your own (for an artifact, or on a
CI with no native summary):

```console
$ rstest --doctor-md doctor.md
```

`--doctor-md` is additive: under GitHub Actions or Buildkite the automatic
summary is still published. GitLab and TeamCity have no native markdown
summary, so write the file with `--doctor-md` and publish it as an artifact.

## Gating a PR on doctor metrics

JSON trending is advisory: someone has to look. To make the signal
*enforce* itself, gate the run on a threshold with `--doctor-fail-on`:

```console
$ rstest -n auto --doctor-fail-on 'parallel_efficiency<30' \
                 --doctor-fail-on 'wait_pct>50'
```

The run exits non-zero if any condition fires (here: efficiency below 30%,
or more than half of test time spent waiting). Repeatable; the gate is the
union of all conditions. `wait_pct` and `wait_seconds` are gated on the
measured values even when the WAIT-BOUND section is below its display
threshold, and `long_pole_seconds` works at any worker count. A pool-only
metric (`parallel_efficiency`, `efficiency_pct`, `realized_speedup`,
`imbalance_pct`) at `-n 0` / `-n 1` is skipped, never failed, and the
closing line then says how many conditions passed and how many were
skipped (`1 condition(s) passed, 1 skipped (not measured for this run)`)
instead of `all N condition(s) passed`. A typo'd metric or a threshold that
is not a finite number (`NaN`, `inf`) aborts before the run rather than
silently passing. Full metric and
operator list: [`--doctor-fail-on`](../reference/cli.md#-doctor-fail-on-cond).
