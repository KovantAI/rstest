# Resource leaks

A test that starts a thread it never joins, or opens a file/socket it never
closes, leaves that resource live for the rest of the session. It rarely fails
the test that caused it — instead it becomes **shared state that flakes a later
test** (a stray thread races, an fd limit is hit, a background loop mutates
global state). rstest measures this per test and names the culprit, so you fix
the leak instead of chasing the symptom.

Two entry points, both riding the worker instrumentation you already pay for
under [`--doctor`](../reference/cli.md#-doctor):

- **See** leaks — `--doctor` prints a `RESOURCE LEAKS` section.
- **Gate** on them — [`--fail-on-leak`](../reference/cli.md#-fail-on-leak)
  fails the build if any test leaks (no `--doctor` needed).

## Detect: `--doctor`

```console
$ rstest -n auto --doctor
```

```text
RESOURCE LEAKS (net threads/fds still open after teardown):
  +1 thread  tests/test_pool.py::test_executor
  +5 fds     tests/test_io.py::test_reader
  a test opened a thread/fd it never released; leaked state can flake later
  tests (reset it, or close in teardown).
```

The section only appears when something leaked, and rides the same
[doctor JSON / markdown](doctor.md#json-output-for-ci) surfaces as every other
doctor finding (a `leaks` array in `--doctor-json`).

## Gate: `--fail-on-leak`

```console
$ rstest -n auto --fail-on-leak
```

Exits `1` if any test leaks (offenders listed on stderr), `0` otherwise. It
turns on the leak instrumentation by itself, so you can gate without the full
doctor report. Ideal as a CI step that keeps new leaks out.

## What is measured

Per test, in the worker that ran it:

- **Threads** — `threading.active_count()`. Portable, but counts only Python
  threads; a native C-extension thread that bypasses the `threading` module is
  invisible.
- **File descriptors** — the open-fd count, from `/proc/self/fd` on Linux and
  `/dev/fd` on macOS/BSD. On platforms with neither, fd tracking is silently
  off (threads still work).

## How the delta is computed

The count is snapshotted **before setup** and again **after teardown**, and the
report carries the *net* difference:

- A test that opens something and closes it (in the test or a fixture teardown)
  nets **zero** — not a leak.
- A test that opens something and never releases it nets **positive** — flagged.

Because the whole protocol (setup → call → teardown) is bracketed, correct
cleanup in a teardown fixture is credited; only what survives it counts.

The worker also **skips its first test** as a warm-up: importing a test module
can lazily spin up a persistent thread or open a cache fd *once*, which is not a
per-test leak. Measurement starts from the second test each worker runs.

## False positives to know about

- **Session / module-scoped fixtures.** A fixture that opens a connection pool
  is set up on the *first test that uses it* and torn down at the end of its
  scope — not per test. That first test therefore shows the fixture's threads/
  fds as a "leak", even though the fixture is behaving correctly. Treat a
  fixture-shaped leak as informational; move the resource into a properly
  teardown-scoped fixture if you want it to net zero.
- **Interpreter internals.** Some libraries start a shared background thread on
  first use (loggers, async loops). The warm-up skip absorbs the common case,
  but a lazily-imported one can still attribute to whichever test first touched
  it.

Because of these, the report is **advisory under `--doctor`**. Reach for
`--fail-on-leak` once your suite is clean, so the gate flags *new* leaks rather
than a pre-existing fixture pattern.

## Fixing a leak

- **Close what you open**, ideally in a fixture teardown so it runs even when
  the test fails:

  ```python
  @pytest.fixture
  def reader():
      f = open("data.bin")
      yield f
      f.close()  # or: with open(...) as f: inside the test
  ```

- **Join threads / shut down executors** the test starts:

  ```python
  with ThreadPoolExecutor() as pool:  # __exit__ shuts it down
      ...
  ```

- If a leak is genuinely unavoidable for one test (a C extension you don't
  control), isolate it with [`@pytest.mark.serial`](../reference/markers.md) so
  it can't race the parallel phase, and exclude it from the gate.

## See also

- [Suite diagnostics](doctor.md) — the `--doctor` report this rides on.
- [`--fail-on-leak`](../reference/cli.md#-fail-on-leak) — the CI gate.
- [Flaky tests](flaky-tests.md) — leaked state is a leading cause of
  order-dependent flakiness.
