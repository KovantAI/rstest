# Resource leaks

A test that starts a thread it never joins, or opens a file/socket it never
closes, leaves that resource live for the rest of the session. It rarely fails
the test that caused it: instead it becomes **shared state that flakes a later
test** (a stray thread races, an fd limit is hit, a background loop mutates
global state). rstest measures this per test and names the culprit, so you fix
the leak instead of chasing the symptom.

Two entry points, both riding the worker instrumentation you already pay for
under [`--doctor`](../reference/cli.md#-doctor):

- **See** leaks: `--doctor` prints a `RESOURCE LEAKS` section.
- **Gate** on them: [`--fail-on-leak`](../reference/cli.md#-fail-on-leak)
  fails the build if any test leaks (no `--doctor` needed).

## Detect: `--doctor`

```console
$ rstest -n auto --doctor
```

```text
RESOURCE LEAKS (threads/fds a test created, still open after its teardown):
  +1 thread  tests/test_pool.py::test_executor
  +5 fds  tests/test_io.py::test_reader
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

- **Threads**: the live `threading.Thread` objects (`threading.enumerate()`).
  Portable, but sees only Python threads; a native C-extension thread that
  bypasses the `threading` module is invisible.
- **File descriptors**: the open fds, from `/proc/self/fd` on Linux and
  `/dev/fd` on macOS/BSD, each identified by its number plus the device and
  inode it points at (so a reused fd number that now names a different file
  counts as a new fd). On platforms with neither, fd tracking is silently off
  (threads still work).

## How a leak is attributed

rstest tracks **which** threads and fds exist, not how many. The set is
snapshotted **before setup** and again **after teardown**; a test is charged
with what it created in that window (setup, call and teardown) and is still
alive at the end:

- A test that opens something and closes it (in the test or a fixture teardown)
  is clean.
- A test that opens something and never releases it is flagged.
- Another test's cleanup can't hide a leak. If a module fixture's teardown runs
  in the last test of the module and joins the fixture's thread, the last test
  is still charged with the thread *it* started. A thread one test starts and a
  later test happens to end is still charged to the test that started it.

Because the whole protocol (setup, call, teardown) is bracketed, correct
cleanup in a teardown fixture is credited; only what survives it counts.

**Fixtures wider than a test.** Threads and fds created while a class, module,
package or session fixture is being set up belong to that fixture, not to the
test that happened to trigger the setup, and are never charged to a test. A
session-scoped server that is shut down at session end is therefore not a leak,
and neither is the first test that used it. Such a fixture is set up once per
scope, so it can't pile up resources per test; the flip side is that a wide
fixture that never releases what it opened is not reported either. Function
fixtures are part of the test and are charged like the test body.

The worker also **skips its first test** as a warm-up: a library can lazily
spin up a persistent thread or open a cache fd *once*, on first use, which is
not a per-test leak. Measurement starts from the second test each worker runs.

## False positives to know about

- **Lazily started internals.** Some libraries start a shared background
  thread or open a cached fd on first use (loggers, async loops, a resolver
  socket). The warm-up skip absorbs the common case, but one first touched
  inside a later test's body is charged to that test.
- **Threads a wide fixture's resource starts later.** A session fixture that
  hands out a thread pool or a threaded server whose worker threads start only
  when a test uses it: the threads start in the test's window, so a worker
  thread still alive after the test is charged to the test.

Because of these, the report is **advisory under `--doctor`**. Reach for
`--fail-on-leak` once your suite is clean, so the gate flags *new* leaks.

Attribution follows what each test did, so it does not depend on which worker
ran it or in what order: the same leaking test is named whatever the scheduler
did. The one exception is the unchecked warm-up test, which is the first test
each worker runs and so can change between parallel runs. To gate every test
in a fixed order, use `rstest -n 0 --fail-on-leak` or
`rstest --dist loadfile --fail-on-leak`.

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
  control), know that `--fail-on-leak` has no allowlist: it fails the run on
  every test with a positive delta, and markers don't exempt a test. Keep that
  test out of the gated run instead: deselect it there (`-k "not test_name"`
  or `--deselect <nodeid>`) and run it in a separate invocation without
  `--fail-on-leak`.

## Go deeper

- [Suite diagnostics](doctor.md): the `--doctor` report this rides on.
- [`--fail-on-leak`](../reference/cli.md#-fail-on-leak): the CI gate.
- [Flaky tests](flaky-tests.md): leaked state is a leading cause of
  order-dependent flakiness.
