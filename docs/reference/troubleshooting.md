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
that has it. Each rejected candidate is listed with its reason (older than
3.10, not runnable, missing the shim, or not matching a `--python` version
request).

## `found .venv/bin/python but rstest is not installed in it`

```text
Error: found /path/to/project/.venv/bin/python but rstest is not installed in it (it cannot import the rstest worker shim).
That environment holds your project's dependencies, so rstest will not silently run your tests with /usr/bin/python3 instead.
```

rstest found your project's virtualenv (a `.venv` up from the working
directory, or the active `$VIRTUAL_ENV`) and it runs, but rstest isn't
installed in it, while some other interpreter (on `PATH`, or uv-managed) has
rstest. rstest stops here instead of quietly using that other interpreter,
because your project's dependencies live in the venv: a run anywhere else
would fail every test that imports them with a `ModuleNotFoundError` that
never mentions the venv. Install rstest into the venv:

```console
$ uv pip install --python .venv/bin/python rstest
$ .venv/bin/python -m pip install rstest     # or with pip
```

If you really do want a different interpreter (say, a global rstest for a
project whose tests need no dependencies), pass it with `--python`, for
example `--python python3`. A venv that is broken (its interpreter no longer
runs) or too old is skipped with a `rstest: warning:` line naming it, and so
is an active `$VIRTUAL_ENV` without rstest when the project's own `.venv`
has it.

## `No interpreter satisfied '3.13' (pinned by .../.python-version)`

A `.python-version` file sets the Python version rstest looks for, but only
as a soft pin: a usable virtualenv (the active `$VIRTUAL_ENV` or the project's
`.venv`) wins over it, with a `rstest: warning:` line when the versions
differ. This error therefore means no venv was usable and no other
interpreter with rstest matches the pin. Update or delete the file the error
names, install a matching Python, or pass `--python` to override it.

## `ImportError: cannot import name 'TypeAlias'` (or similar) at startup

Your project's interpreter is older than Python 3.10. The vendored pytest
core requires 3.10+, which matches the supported CPython line (3.9 is
end-of-life as of October 2025). Upgrade the environment's Python.
rstest rejects a 3.9 interpreter during discovery
(`Python 3.9.x is older than the required 3.10`, listed with the other
rejected candidates), so this error means a 3.9 interpreter was forced in
some other way.

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
Workers must collect the same ids in the same order. Common causes:
pytest-randomly without a fixed seed, parametrize IDs derived from
time/randomness, or parametrize over a set (set/dict iteration order of
strings changes with PYTHONHASHSEED, random per process). Workarounds:
-p no:randomly, stable parametrize ids, a list or sorted(...) instead of a
set, a fixed PYTHONHASHSEED for the run, or -n 0. `rstest migrate-check`
names the unstable sites
```

Your collection is nondeterministic, usually a `@pytest.mark.parametrize`
whose ids differ between processes (a memory address, a uuid, a timestamp),
a `parametrize` over a set, or an unseeded randomizing plugin. rstest refuses
to dispatch rather than misassign tests. Fix the nondeterminism or run
`-n 0`; [Unstable parametrize ids](../concepts/compatibility.md#unstable-parametrize-ids)
lists the causes and fixes, and
[`rstest migrate-check`](cli-commands.md#migrate-check) names each unstable
site before a parallel run.

## `workers collected different test sets (N vs N items)`: same count, intermittent

Same error, but both counts match and it comes and goes between runs. The
workers collected the same ids in a different order, almost always a
`parametrize` over a `set` of strings: string hashing follows
`PYTHONHASHSEED`, which is random per process, so each worker iterates the
set differently. Iterate a list or `sorted(...)` instead, or pin one seed for
the whole run (`PYTHONHASHSEED=0 rstest`). `rstest migrate-check` reports
these sites as `UNSTABLE ORDER`. See
[Unstable parametrize ids](../concepts/compatibility.md#unstable-parametrize-ids).

## Tests fail once the cache is warm: `auto-selected lazy collection`

A large suite that passed on its first runs starts failing after rstest
prints `rstest: auto-selected lazy collection (...)`. Under lazy collection
each worker imports only the test files it runs, so a test that relied on
another test file's import breaks: a `skipif` that reads `sys.modules` stops
skipping, or a registration done by a sibling module never happens. Pass
`--collect full` (or set `[tool.rstest] collect = "full"`) to restore eager
collection, and fix the hidden dependency when you can. See
[The compatibility trade](../concepts/lazy-collection.md#the-compatibility-trade).

## My plugin's terminal output doesn't appear

At `-n ≥ 2` rstest renders the terminal; plugin-drawn UIs (progress bars,
custom reporters) don't paint. The plugin still *runs*: hooks fire, data
flows. Use `-n 0` when you specifically want a plugin's own rendering.

## A plugin crashes at `-n ≥ 2` with `KeyError` on a `workerinput` key

The plugin reads a `workerinput` key that pytest-xdist's *controller* process
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
controller?" check), and every rstest pool worker has a `workerinput`, so nothing
owns report generation. Producing one file from all workers needs a single
controller process, which rstest doesn't run.

This only bites when `--html` reaches pytest-html itself: from `addopts` or
after `--`. A `--html` on the rstest command line is rstest's native merged
report and is written at every worker count, so the simplest fix is to move
`--html` out of `addopts` and onto the command line. If you need pytest-html's
own layout, generate it in single-worker mode with
`rstest -n 0 -- --html=report.html` (no `workerinput` is set there); the rest of your suite
can still run parallel in a separate step.

## Where did my `tmp_path` go?

Each worker uses a disjoint temp root (`$TMPDIR/rstest-<pid>/gwN/...`),
like pytest-xdist. With a user-provided `--basetemp`, each worker uses
`<basetemp>/gwN` (xdist's layout); with `-n 0` it is used as given.

## `rstest` runs the wrong Python / can't find my venv

Worker interpreter discovery order: `--python` flag, `$VIRTUAL_ENV`, a
`.venv` walking up from the working directory, versioned `python`/`pythonX.Y`
on PATH, then uv-managed interpreters (full list:
[Which Python does rstest use?](../getting-started/installation.md#which-python-does-rstest-use)).
Activate your environment or pass `--python` explicitly. `--python` takes an
interpreter path, a venv directory (`--python .venv`), a command on `PATH`
(`python3.12`), or a version request (`3.12`). Under pre-commit, see
[Point the hook at your project's interpreter](../guides/ci-recipes.md#point-the-hook-at-your-projects-interpreter).

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
extension, or any test on Windows), the per-test
[hang watchdog](../concepts/crash-handling.md#hung-tests-worker-timeout)
kills the worker and reports the test failed;
[`--worker-timeout 300`](cli.md#-worker-timeout-secs) sets one fixed limit
for every test instead. Hangs during collection or session config are outside
both, so wrap the invocation in an external timeout if your environment can
hang before tests start.

## A worker crashed: what happened to its tests?

The test that killed it is reported FAILED with a "crashed while running"
message. By default it is *not* retried: segfault loops are worse. With
[`--reruns`](cli.md#-reruns-n) (or `@pytest.mark.flaky`) it does get
another attempt, on whichever worker takes it next, while budget remains,
bounded by both the rerun and restart budgets so a repeatable crash can't
loop. Its remaining tests are redistributed to other workers automatically.

If a run has already had crashes and you then see a `<worker gwN>` error
reading `worker terminated unexpectedly`, the restart budget was exhausted:
something is killing workers repeatedly, and the longrepr of the first crash
is the lead. The surviving workers still run the dead worker's remaining
tests; any test no worker was left to run is reported as an error that
starts with `not run:` (see
[Budgets](../concepts/crash-handling.md#budgets)). Each crash failure and
`<worker gwN>` error carries the worker's exit code (or the signal that
killed it) and the last lines it wrote to stderr. The same `worker terminated unexpectedly` text also appears
when a worker dies before it collects anything, with no crash before it; that
case is covered next.

## `worker terminated unexpectedly: exited during startup`

The worker process died before it sent anything back, so it never collected
a test and isn't restarted. The failure shows its exit code and the last
lines of its stderr, which is usually the actual error, for example:

```text
--- FAILED <worker gw0> ---
worker terminated unexpectedly: exited during startup, before sending any event
  exited with code 1
  last lines of its stderr:
    ...
    ModuleNotFoundError: No module named 'exceptiongroup'
```

A missing module here means the interpreter rstest picked can't import
rstest's worker or the vendored pytest core: reinstall rstest into the
environment you run tests in (`pip install --force-reinstall rstest`), or
check that rstest is using the interpreter you expect (see
[`rstest` runs the wrong Python](#rstest-runs-the-wrong-python-cant-find-my-venv)).
Anything else in the tail, such as an error from a `sitecustomize` or a
`.pth` file, comes from the environment itself.

## `Error: pulling shared cache from <remote>` (exit 1)

`--cache-pull` could not reach or read the remote (an unreachable endpoint,
expired credentials, a listing or read error), and rstest stopped with exit
1 before running any test. An empty or missing remote is not an error, so
this is a transport or permission problem: check the credential against
[the grants each backend needs](../concepts/caching.md#cache-permissions). To
keep a remote outage from turning the job red, retry the step or rerun
without `--cache-pull` when the pull was the failure; see
[Shared cache: reliability](../guides/ci-shared-cache.md#reliability).

## `--shard needs the parallel pool (-n >= 2)`

`--shard` refuses single-worker mode, and the default `-n auto` can resolve
to one worker (one selected test file, or a warm cache with only a couple of
seconds of test time), so the same job can pass on one machine and exit 1 on
another. Pass an explicit `-n 2` or higher with `--shard`. See
[`--shard`](cli.md#-shard-kn).

## `--reruns`, `--junitxml` or `--timeout` in `addopts` is ignored or rejected

rstest reads its own flags only from the command line and `[tool.rstest]`.
The same flag in pytest's `addopts` or in `PYTEST_ADDOPTS` goes to the pytest
session inside each worker instead. There it either fails the run with
`unrecognized arguments: --timeout --reruns` (exit 4) when no plugin defines
the flag, or reaches the plugin rather than rstest: with pytest-rerunfailures
installed `addopts = --reruns 2` silently gives no reruns under the pool (the
plugin is neutralized inside workers), and `addopts = --junitxml=x.xml`
writes no file at `-n 2` or more. Move these flags to the command line or
`[tool.rstest]`. The rstest-owned flags that share a name with a pytest or
plugin flag are listed under
[Owned flags that shadow plugin or pytest flags](cli.md#shadowed-flags).

## `--changed` selected nothing and the artifact upload step failed

When nothing is affected, `--changed` prints
`rstest: no tests affected by N changed file(s)` and exits before running
anything (0, or 5 under `--changed-strict`), so no `--junitxml` or
`--report-json` file is written. A later step that requires those files,
such as `actions/upload-artifact` or a JUnit publisher, then fails. Make the
step tolerate a missing file (`if-no-files-found: ignore`). See
[Exit codes: special cases](exit-codes.md#special-cases) and
[Selecting changed tests: CI usage](../guides/changed.md#ci-usage).
