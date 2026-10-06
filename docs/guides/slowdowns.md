# Catching slowdowns

A test that quietly goes from 0.1s to 1.2s never turns CI red on its own.
rstest has two [CI gates](../concepts/glossary.md#ci-gate) for that:

- [`--durations-regress RATIO`](../reference/cli.md#-durations-regress-ratio)
  is **per test**: it fails the run when a test got `RATIO` times slower than
  its [baseline](../concepts/glossary.md#baseline) in the
  [duration cache](../concepts/glossary.md#duration-cache).
- [`--doctor-fail-on COND`](../reference/cli.md#-doctor-fail-on-cond) is
  **suite level**: it fails the run when a [doctor](doctor.md) metric such as
  `wall_seconds` crosses a fixed threshold.

Both raise a passing run's exit code to `1` and leave the test outcomes alone.

## Gate on per-test regressions

```console
$ rstest -n auto --durations-regress 2
```

After the run, each test's time is compared with the time the cache recorded
for it, before this run's times are saved. The second run below made
`test_poll` sleep 1.2s instead of 0.1s:

```text
3 passed in 1.42s

=========== duration regressions (>= 2x baseline) ===========
     0.11s ->    1.20s  test_api.py::test_poll
rstest: 1 duration regression vs baseline (--durations-regress)
```

The run exits `1`. With nothing over the threshold it prints
`rstest: --durations-regress: no regressions (>= 2x baseline)` and the exit
code is the tests' own.

### What counts as a regression

A test is flagged when **all three** hold:

- its time this run is at least `RATIO` × its baseline;
- the baseline is at least **50 ms**;
- it grew by at least **0.5 s**.

The two floors absorb runner jitter: a 20 ms test that takes 600 ms, or a
0.3s test that takes 0.75s, is not flagged at `--durations-regress 2`. Pick
the ratio for your runners' noise; `2` is a safe start, `1.5` is tighter. It
must be greater than `1.0` (`--durations-regress 1` is an error, exit `1`).

What is timed is the **call phase** only: fixture setup and teardown are not
part of a test's duration. A function fixture that went from 0.1s to 2s does
not trip this gate; the suite-level gate below catches that. Tests with no
baseline entry (new or renamed) are skipped, never flagged.

## Where the baseline comes from

The baseline is `.rstest_cache/durations.json`, the same file
[duration-aware scheduling](../concepts/glossary.md#duration-aware-scheduling)
uses. Every run, gated or not, refreshes it (`--dist each` and
`rstest replay` runs write nothing), with two exceptions that keep the old
value:

- a **failed** test's time is not recorded, so a fail-fast `0.0002s` can't
  shrink its baseline;
- a test the gate **flagged** is not recorded either (nor pushed with
  `--cache-push`), so the next identical run fails again instead of adopting
  the slow time.

The comparison reads the cache as recorded, so a test in a file your change
edited is still compared with its old time. `rstest explain` shows the value
a test will be compared with:

```console
$ rstest explain test_api.py::test_poll
test: test_api.py::test_poll

  duration   0.1052s (last recorded)
  ...
```

Three consequences worth knowing:

- **The baseline is the last good run, not an average.** A test that grows
  1.8× on each of two runs passes both and ends up 3.3× slower. Keep the
  baseline pinned to your default branch (next section) so a pull request
  is always compared with main.
- **Accepting an intended slowdown** means one run without
  `--durations-regress`: it records the new time as the baseline.
- **An edited file drops its entries on save.** When a flagged test's file
  was edited in the same run, the save prunes that file's entries as stale
  instead of keeping the old time, and the next run on that cache has no
  baseline for the test (so it skips it). A pull request job that saves its
  own cache can therefore pass on its second push. Another reason to keep PR
  jobs read-only.

## Don't let a cold cache pass green

With no `durations.json` the gate has nothing to compare with. It warns and
skips:

```text
3 passed in 0.50s
rstest: --durations-regress: no duration baseline yet (.rstest_cache/durations.json); comparison skipped
```

That is right for the very first run, which seeds the baseline. In steady
state it means the cache restore or pull broke and the gate has gone dead.
Add [`--require-baseline`](../reference/cli.md#-require-baseline) to turn it
into an error before any test runs:

```console
$ rstest -n auto --durations-regress 2 --require-baseline
Error: --require-baseline: --durations-regress needs a duration baseline in .rstest_cache, but none is present (cold cache — nothing restored or pulled)
```

Exit `1`, and no report file is written
([exit codes](../reference/exit-codes.md#gating-ci-on-exit-code-and-report)).
With `--cache-pull` the check runs after the pull, so durations the pull
brought in count. A failed pull is already an error on its own
([Shared cache: reliability](ci-shared-cache.md#reliability)).

## Gate the whole suite

A per-test gate misses slowdowns spread thin (fifty tests each 30% slower)
and anything that lives in fixtures. `--doctor-fail-on` gates on suite totals:

```console
$ rstest -n auto --doctor-fail-on 'wall_seconds>1' --doctor-fail-on 'test_time_seconds>1'
```

```text
3 passed in 1.40s

=========== doctor gate failures ===========
  wall_seconds = 1.40 > 1.00 (wall_seconds>1)
  test_time_seconds = 1.53 > 1.00 (test_time_seconds>1)
rstest: --doctor-fail-on: threshold breach (see doctor gate failures above)
```

When every condition holds it prints
`rstest: --doctor-fail-on: all 2 condition(s) passed`. The thresholds are
absolute numbers you choose, not a comparison with a previous run, so set them
with headroom over a normal run:

- `test_time_seconds` sums every test's setup, call and teardown, so it
  changes little with the worker count. Prefer it when runner sizes vary.
- `wall_seconds` is what the job actually waits for; it depends on `-n` and
  the runner.
- `long_pole_seconds` caps the slowest single test, which is the floor no
  worker count can beat.

The full metric list and the parse-time checks are in
[Gating a PR on doctor metrics](doctor.md#gating-a-pr-on-doctor-metrics).

| | `--durations-regress` | `--doctor-fail-on` |
|---|---|---|
| Unit | one test | the whole run |
| Compared with | that test's cached time | a fixed threshold |
| Fixture time | not counted | counted (`test_time_seconds`, `wall_seconds`) |
| Needs a cache | yes | no |

## Wire both in CI

Both gates can share one step. Fail the PR on either, and let the default
branch refresh the baseline:

```console
$ rstest -n auto --durations-regress 2 --require-baseline \
                 --doctor-fail-on 'test_time_seconds>600'
```

- **Persist the cache** so the baseline exists at all: the `actions/cache`
  step in the [CI quickstart](ci-quickstart.md#github-actions) for one job, or
  the [shared cache](ci-shared-cache.md) for a shard matrix. The bundled
  action takes `durations-regress`, `require-baseline` and `doctor-fail-on`
  inputs.
- **Write the baseline from the default branch, read it on PRs.** Full runs
  on main publish it ([warm from full default-branch
  runs](ci-shared-cache.md#warm-from-full-default-branch-runs)); PR jobs
  should restore without saving (`actions/cache/restore`, or the action's
  `cache-push: false`). The action's `remote` backend already pushes only
  from the trusted branch by default.
- **Add `--require-baseline` once the cache is seeded.** Without it a broken
  restore only warns.
- **Expect exit `1` for a failed gate.** A `--report-json` from such a run
  has `meta.exitstatus` `0`; that is how to tell "tests passed, a gate
  fired" from "tests failed"
  ([exit codes](../reference/exit-codes.md#gating-ci-on-exit-code-and-report)).
- **Monorepos:** both gates are forwarded to every project and judged per
  project ([Monorepo](../concepts/monorepo.md)).

## Acting on a failure

1. **Read the table.** The regression block lists `baseline -> current` per
   test, biggest absolute growth first. It is on stdout; the one-line summary
   and the doctor gate lines are on stderr.
2. **Check it is real.** Re-run the test locally, before and after the
   change. A one-off spike on a shared runner is noise; raise the ratio if it
   keeps happening.
3. **Find where the time went.** `rstest --doctor` on the slow file splits
   waiting from computing and names fixture hotspots
   ([Suite diagnostics](doctor.md)). Waiting on a sleep or timeout is the
   usual cause ([Wait-bound suites](wait-bound.md)).
4. **Fix it, or accept it.** A fixed test passes the gate and its new time
   becomes the baseline. An intended slowdown is accepted by one run without
   `--durations-regress` on the branch that owns the baseline.
