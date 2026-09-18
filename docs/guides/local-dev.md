# Local dev / inner loop

You have a small, fast suite (it finishes in a few seconds) and you run it constantly while you code. This page is about the *loop*, not the clock: how to get from save to green in as few keystrokes as possible, and how to keep the suite trustworthy as it grows.

## Who this is for

Maintainers of a suite small enough that raw parallel throughput is beside the point. On a suite that already runs in under ~10 seconds, adding workers barely moves the wall clock. The wins that matter are *rerunning less* and *rerunning automatically*. So this page skips the parallel-speed pitch entirely and leads with the three flags that tighten the edit-save-test cycle: `--watch`, `--changed`, and `--lf`. Then it covers what [`--doctor`](doctor.md) still buys a fast suite (leaks, fixture hotspots, flaky candidates, not "where does the time go"), and how to keep flakes from eroding trust.

## The inner-loop trio

### 1. Watch mode: `--watch`

Run the suite once, then leave it running; it reruns on every save:

```console
$ rstest --watch
```

```text
2 passed in 0.13s

[watch] waiting for changes... (Ctrl+C to quit, last exit: 0)
[watch] test_w.py changed; rerunning changed files
2 passed in 0.13s
[watch] helper.py changed; rerunning full selection
```

The rerun-selection policy is import-graph based, so you don't rerun the whole suite on every keystroke:

- A change set of **only test files** (per your `python_files` patterns) reruns exactly those files, with all your other flags intact.
- Any other `.py` (source) change reruns the tests **affected by the change** per the project import graph (the same machinery as [`--changed`](changed.md)). A change affecting no tests skips the rerun; anything the graph can't reason about falls back to the full selection.
- Changes to pytest config (`pyproject.toml`, `pytest.ini`, `setup.cfg`, `tox.ini`) trigger a full rerun.
- VCS internals, `__pycache__`, virtualenvs, and rstest's own caches are ignored.

Editor save-bursts are debounced (300ms), and the screen clears between runs on a terminal. `Ctrl+C` exits.

**Every rerun is a clean run.** Each cycle spawns fresh Python worker processes and tears them down when it finishes, at every worker count, including `-n 0`/`-n 1`. Nothing is reused between cycles, so an edited module is always re-imported from scratch; watch mode cannot show a stale-import false green.

Flags compose, and they apply to every rerun:

```console
$ rstest --watch -x            # stop each run at first failure
$ rstest --watch -k login      # only the login tests, on every change
$ rstest --watch -n 2          # bounded parallelism while editing
```

The duration cache and last-failed state update on every cycle, so `--lf` (below) and slow-test-first scheduling stay warm throughout the session.

<!-- TODO(gap): whether saving a brand-new test file (not yet in any selection) is picked up by watch is not stated in watch-mode.md or cli.md. -->
<!-- TODO(gap): per-cycle watch overhead (fixed cost each rerun adds on top of test time) is not documented in the source files. -->

### 2. Run only what changed: `--changed`

When you'd rather drive the loop by hand than leave a watcher running, run only the tests your working-tree changes affect:

```console
$ rstest --changed
```

Changes come from git: working tree + untracked vs `HEAD`. Two selection engines back it, and rstest picks the tightest one available:

| Engine | When | Granularity |
|---|---|---|
| **Import graph** | always available, zero setup | whole test *files* that transitively import a changed module |
| **Coverage index** | when a line→test index is warm | individual *tests* whose recorded coverage hit the changed *lines* |

Out of the box you get the import graph, conservative by construction (over-selection is safe, under-selection is not): ambiguous module names select every match, function-local imports count as edges, a changed `conftest.py` selects its whole subtree, and any config or non-Python change falls back to a full run. The one documented gap is dynamic imports (`importlib.import_module`), which produce no edges. Use [`--changed-strict`](../reference/cli.md#-changed-strict) for correctness-critical runs.

If you want tighter selection locally, warm the coverage index once with a coverage run (`rstest --cov=src --cov-context=test`); from then on `--changed` maps *changed lines* to only the tests that executed them. Full mechanics, drift handling, and how to keep the index warm: [Selecting changed tests](changed.md).

### 3. Last-failed: `--lf`

After a red run, rerun just the failures until they're green:

```console
$ rstest --lf
```

`--lf`/`--ff` are forwarded to pytest, but the last-failed cache is written by rstest from **merged results** across workers, so a follow-up `--lf` behaves exactly as after a serial run (see [CLI reference](../reference/cli.md)). It composes with watch mode, where the last-failed state refreshes every cycle. Note that `--lf` still reruns [quarantined](flaky-tests.md) failures. Locally they behave like the failures they are.

## What `--doctor` gives a fast suite

`--doctor` is often pitched at slow suites answering "where does the time go?", but three of its findings matter regardless of how fast your suite is, and they're the reason to run it on a *small* suite too:

```console
$ rstest --doctor
```

### Resource leaks: the correctness one

A test that starts a thread it never joins, or opens an fd it never closes, leaves that resource live for the rest of the session. It rarely fails the test that caused it. Instead it becomes **shared state that flakes a later test**. On a small suite this is easy to miss precisely because everything's fast and green. Doctor prints a `RESOURCE LEAKS` section naming the culprit (net threads/fds still open after teardown):

```text
RESOURCE LEAKS (net threads/fds still open after teardown):
  +3 threads  tests/test_pool.py::test_executor
  +5 fds      tests/test_io.py::test_reader
```

The count is snapshotted before setup and after teardown, so correct cleanup nets zero. The first test each worker runs is skipped as a warm-up. Session/module-scoped fixtures can show a one-time "leak" that's actually the fixture behaving correctly, so the `--doctor` report is advisory. Full model, false-positive cases, and fixes: [Resource leaks](resource-leaks.md). To make it a gate once your suite is clean, use [`--fail-on-leak`](../reference/cli.md#-fail-on-leak).

### Fixture hotspots: keep setup cheap

```text
FIXTURE HOTSPOTS (setup time across all workers):
     0.79s   4442x  scope=function blockbuster
     0.54s    157x  scope=function transport
```

Total setup time per fixture. A *function-scoped* fixture that runs on every test and costs real time is a candidate for a wider scope: one real-world suite re-parsed the same RSA key 206 times in what could have been a session fixture. This is exactly the kind of drag that a fast suite accumulates silently and that punishes you on every single inner-loop rerun. (See [Suite diagnostics](doctor.md).)

### Flaky candidates

Doctor's leak section is the first place to look for order-dependent flakiness, because leaked state is a leading cause of it. For the flake *signal* itself, see the next section.

Doctor adds only a few cheap measurements and doesn't change outcomes, so it's fine to run on a whim, and it works at any worker count: a fast suite's usual `-n 0`/`-n 1` still runs a real worker process, so leak and fixture-cost instrumentation apply. (One nuance: the first test each worker runs is skipped from leak detection as a warm-up, so at `-n 0` the very first test isn't leak-checked.) There is also `--doctor-json`/`--doctor-md` for CI, but that's a CI concern, not an inner-loop one.

## Flaky-test handling

Even a fast suite gets the occasional intermittent failure, and on a tight loop a spurious red is maximally annoying. Three tools, one lifecycle:

- **Detect within a run**: [`--reruns N`](../reference/cli.md#-reruns-n) retries a failure; a test that then passes is reported `flaky` (the run stays green) and recorded. Works at any worker count, including `-n 0`/`-n 1`.
- **Remember across runs**: every run merges events into `.rstest_cache/flakes.json` automatically (no flag). A test with `flaky: 7` is your ranked candidate to fix, and history ages out (default 90 days) so a test you actually fixed goes quiet on its own.
- **Ring-fence**: [`--quarantine`](../reference/cli.md#-quarantine-file) tolerates a committed list of known offenders (failures on the list don't redden the run; failures off it still fail) while they get fixed.

The full workflow, file formats, and `--reruns-only-known-flaky` targeting: [Flaky tests](flaky-tests.md).

## Go deeper

- [Watch mode](watch-mode.md): rerun policy and flag composition in full.
- [Selecting changed tests](changed.md): import graph vs coverage index, drift, `--changed-strict`.
- [Suite diagnostics](doctor.md): every doctor section and the JSON/markdown surfaces.
- [Resource leaks](resource-leaks.md): what's measured, false positives, fixes, the `--fail-on-leak` gate.
- [Flaky tests](flaky-tests.md): reruns, flake history, and quarantine as one lifecycle.
- [CLI reference](../reference/cli.md): every rstest-owned flag; everything else forwards to pytest.
