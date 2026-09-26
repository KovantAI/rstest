# Troubleshooting

Common errors and surprises on a first rstest run, each with its cause and fix.

## `no usable Python interpreter found` / `cannot import the rstest worker shim`

```text
Error: no usable Python interpreter found. Tried:
  /path/to/.venv/bin/python: cannot import the rstest worker shim (is rstest installed in it?)
```

The workers run in *your project's* interpreter, and that interpreter must be
able to import the `rstest_worker` package, which ships in the rstest wheel.
This usually means rstest was installed somewhere else: with `pipx` or
`uv tool`, globally, or into a different venv than the one rstest picked.
Install rstest into the project environment itself (`uv add --dev rstest`,
`pip install rstest` inside the venv), or point `--python` at an interpreter
that has it. Each rejected candidate is listed with its reason (too old, not
runnable, missing the shim, or not matching a `--python` version request).

## `ImportError: cannot import name 'TypeAlias'` (or similar) at startup

Your project's interpreter is older than Python 3.10. The vendored pytest
core requires 3.10+, which matches the supported CPython line (3.9 is
end-of-life as of October 2025). Upgrade the environment's Python.

## Tests pass under pytest, fail under `rstest`, only in parallel

Work through the three-run diagnosis in
[Parallel safety](../guides/parallel-safety.md#diagnosing-a-parallel-only-failure):
`-n 0` (is it the test?), `--dist loadfile` (is it ordering?), `-n 2`
(is it load?). The fix is usually a `@pytest.mark.serial` mark, `--dist
loadfile`, or a clock mock.

## `workers collected different test sets; cannot dispatch safely`

The full error reads:

```text
workers collected different test sets (N vs M items); cannot dispatch safely.
Common causes: pytest-randomly without a fixed seed, or parametrize IDs
derived from time/randomness. Workarounds: -p no:randomly, stable
parametrize ids, or -n 0
```

Your collection is nondeterministic: typically a randomizing plugin
(pytest-randomly without a fixed seed) or test parametrization built from
an unordered source (set iteration, directory listing). rstest refuses to
dispatch rather than misassign tests. Fix the nondeterminism (seed it, sort
it) or run `-n 0`.

## My plugin's terminal output doesn't appear

At `-n ≥ 2` rstest renders the terminal; plugin-drawn UIs (progress bars,
custom reporters) don't paint. The plugin still *runs*: hooks fire, data
flows. Use `-n 0` when you specifically want a plugin's own rendering.

## A plugin crashes at `-n ≥ 2` with `KeyError` on a `workerinput` key

The plugin reads a `workerinput` key that pytest-xdist's *master* process
injects, which rstest has no central controller to set (it runs a
worker-shaped `workerinput` only). The three common cases are now handled, so
you should not hit them on current rstest:

- **pytest-randomly** (`randomly_seed`): rstest synthesizes one run-level
  seed every worker agrees on.
- **pytest-rerunfailures** with pytest-xdist installed (`sock_port`): rstest
  unregisters it inside pool workers (before its configure reads the key) and
  owns reruns natively.
- **pytest-retry** (`server_port`): each worker self-provisions its own
  report server, so the key is set locally.

If a *different* plugin hits this, run it at `-n 0`, or use rstest's native
equivalent (`--shuffle`, `--reruns`): full per-plugin table in
[Plugins](../guides/plugins.md#tested-compatibility). Please also file it.

## My `--html` (pytest-html) report is missing at `-n ≥ 2`

On the rstest command line, `--html report.html` is rstest's own merged
report and works at every worker count (see [`--html`](cli.md#-html-path)).
This section is about pytest-html's report, when the flag reaches the plugin.

No crash, no error: the file just isn't written. pytest-html registers its
report writer only on a node *without* `workerinput` (its xdist "am I the
master?" check), and every rstest pool worker has a `workerinput`, so nothing
owns report generation. Producing one file from all workers needs a single
master process, which rstest doesn't run.

This only bites when `--html` reaches pytest-html itself: from `addopts` or
after `--`. A `--html` on the rstest command line is rstest's native merged
report and is written at every worker count, so the simplest fix is to move
`--html` out of `addopts` and onto the command line. If you need pytest-html's
own layout, generate it in a single session with `rstest -n 0 --
--html=report.html` (no `workerinput` is set there); the rest of your suite
can still run parallel in a separate step.

## Where did my `tmp_path` go?

Each worker uses a disjoint temp root (`$TMPDIR/rstest-<pid>/gwN/...`),
like pytest-xdist. A user-provided `--basetemp` wins and is left alone.

## `rstest` runs the wrong Python / can't find my venv

Worker interpreter discovery order: `--python` flag, `$VIRTUAL_ENV`, a
`.venv` walking up from the working directory, versioned `python`/`pythonX.Y`
on PATH, then uv-managed interpreters (full list:
[Which Python does rstest use?](../getting-started/installation.md#which-python-does-rstest-use)).
Activate your environment or pass `--python` explicitly.

## `rstest: command not found` after `pip install rstest`

The install landed in an environment that isn't on your PATH, usually a
non-activated venv or a `--user` install. Activate the venv you installed
into (its `bin/`/`Scripts/` holds the `rstest` binary), or run it through
your env manager (`uv run rstest`).

## A test hangs forever and the run never finishes

Add [`--timeout 60`](cli.md#-timeout-secs) (or a limit suiting your slowest
test): rstest interrupts a test whose call phase runs past the limit, reports
it failed with a traceback at the line it was stuck on, and the run
completes. `@pytest.mark.timeout(N)` sets a per-test limit. This is built in;
you don't need pytest-timeout (and rstest consumes `--timeout`, so the plugin
never sees it).

For a hang the in-process interrupt can't break (a test blocked inside a C
extension), [`--worker-timeout 300`](cli.md#-worker-timeout-secs) is the
backstop: a worker stuck on one test past the limit is killed, the test
reported failed, and the run completes. `--timeout` arms it automatically at
a generous multiple. Caveat: the watchdog covers
hangs on a TEST (any phase); a hang during collection or session config
is outside it, wrap the invocation in an external timeout if your
environment can hang before tests start.

## A worker crashed: what happened to its tests?

The test that killed it is reported FAILED with a "crashed while running"
message. By default it is *not* retried: segfault loops are worse. With
[`--reruns`](cli.md#-reruns-n) (or `@pytest.mark.flaky`) it does get
another attempt on the replacement worker while budget remains, bounded by
both the rerun and restart budgets so a repeatable crash can't loop. Its
remaining tests redistributed to other workers automatically. If you see
`worker terminated unexpectedly` instead, the restart budget was
exhausted: something is killing workers repeatedly, and the longrepr of
the first crash is the lead.
