# Troubleshooting

## `rstest worker requires the 'msgpack' package`

The worker runs in *your project's* environment and needs its runtime
deps there. Installing the rstest wheel into that environment brings them
automatically; if you're running a source checkout, add `msgpack pluggy
iniconfig packaging pygments` to the environment.

## `ImportError: cannot import name 'TypeAlias'` (or similar) at startup

Your project's interpreter is older than Python 3.10. The vendored pytest
core requires 3.10+, which matches the supported CPython line (3.9 is
end-of-life as of October 2025). Upgrade the environment's Python.

## Tests pass under pytest, fail under `rstest` — only in parallel

Work through the three-run diagnosis in
[Parallel safety](../guides/parallel-safety.md#diagnosing-a-parallel-only-failure):
`-n 0` (is it the test?), `--dist loadfile` (is it ordering?), `-n 2`
(is it load?). The fix is usually a `@pytest.mark.serial` mark, `--dist
loadfile`, or a clock mock.

## `workers collected different test sets; cannot dispatch safely`

Your collection is nondeterministic — typically a randomizing plugin
(pytest-randomly without a fixed seed) or test parametrization built from
an unordered source (set iteration, directory listing). rstest refuses to
dispatch rather than misassign tests. Fix the nondeterminism (seed it, sort
it) or run `-n 0`.

## My plugin's terminal output doesn't appear

At `-n ≥ 2` rstest renders the terminal; plugin-drawn UIs (progress bars,
custom reporters) don't paint. The plugin still *runs* — hooks fire, data
flows. Use `-n 0` when you specifically want a plugin's own rendering.

## A plugin crashes at `-n ≥ 2` with `KeyError` on a `workerinput` key

The plugin reads a `workerinput` key that pytest-xdist's *master* process
injects, which rstest has no central controller to set (it runs a
worker-shaped `workerinput` only). The three common cases are now handled, so
you should not hit them on current rstest:

- **pytest-randomly** (`randomly_seed`) — rstest synthesizes one run-level
  seed every worker agrees on.
- **pytest-rerunfailures** with pytest-xdist installed (`sock_port`) — rstest
  unregisters it inside pool workers (before its configure reads the key) and
  owns reruns natively.
- **pytest-retry** (`server_port`) — each worker self-provisions its own
  report server, so the key is set locally.

If a *different* plugin hits this, run it at `-n 0`, or use rstest's native
equivalent (`--shuffle`, `--reruns`) — full per-plugin table in
[Plugins](../guides/plugins.md#tested-compatibility). Please also file it.

## My `--html` (pytest-html) report is missing at `-n ≥ 2`

No crash, no error — the file just isn't written. pytest-html registers its
report writer only on a node *without* `workerinput` (its xdist "am I the
master?" check), and every rstest pool worker has a `workerinput`, so nothing
owns report generation. Producing one file from all workers needs a single
master process, which rstest doesn't run. Generate the report at `-n 0`/`-n 1`
(a single session, where no `workerinput` is set) — the rest of your suite can
still run parallel in a separate step.

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

The install landed in an environment that isn't on your PATH — usually a
non-activated venv or a `--user` install. Activate the venv you installed
into (its `bin/`/`Scripts/` holds the `rstest` binary), or run it through
your env manager (`uv run rstest`).

## A test hangs forever and the run never finishes

Add `--worker-timeout 300` (or a limit suiting your slowest test): a
worker stuck on one test past the limit is killed, the test reported
failed with a timeout message, and the run completes. For per-test limits
with in-test tracebacks, use pytest-timeout; the watchdog is the backstop
for hangs pytest-timeout can't interrupt. Caveat: the watchdog covers
hangs on a TEST (any phase); a hang during collection or session config
is outside it — wrap the invocation in an external timeout if your
environment can hang before tests start.

## A worker crashed — what happened to its tests?

The test that killed it is reported FAILED with a "crashed while running"
message. By default it is *not* retried — segfault loops are worse. With
[`--reruns`](cli.md#-reruns-n) (or `@pytest.mark.flaky`) it does get
another attempt on the replacement worker while budget remains, bounded by
both the rerun and restart budgets so a repeatable crash can't loop. Its
remaining tests redistributed to other workers automatically. If you see
`worker terminated unexpectedly` instead, the restart budget was
exhausted: something is killing workers repeatedly, and the longrepr of
the first crash is the lead.
