# Running on Windows

rstest runs on Windows: the same wheel-installed `rstest` command, the same
flags, the same parallel pool and the same reports. A few things differ
because Windows has no `fork()`, no Unix signals and no `/proc`. This page
is the one place those differences live; other pages link here instead of
repeating them.

## Install { #install }

Wheels are published for Windows on x86_64 and arm64. Install into the
virtualenv whose tests you run, exactly as on other platforms:

```console
> python -m venv .venv
> .venv\Scripts\activate          # PowerShell: .venv\Scripts\Activate.ps1
> pip install rstest
```

The wheel installs `rstest.exe` into the environment's `Scripts\` folder, so
`rstest` is on `PATH` once the venv is active. Without activating, call it by
path (`.venv\Scripts\rstest.exe`); rstest still finds the project `.venv` by
walking up from the working directory. A `pip install --user` puts it in your
per-user `Scripts\` folder, which is often not on `PATH` (pip prints a warning
naming it); a [global tool install](../getting-started/installation.md#tool-install)
with pipx or `uv tool` avoids that. On Windows rstest also installs
`colorama` as a runtime dependency.

Interpreter discovery knows the Windows layout: a venv's
`Scripts\python.exe`, `python.exe` / `python3.exe` on `PATH`, python.org
installs behind the `py` launcher (`py --list-paths`), and uv-managed
interpreters under `%APPDATA%\uv\data\python`. So `--python .venv` and
`--python 3.12` resolve as they do elsewhere
([`--python`](../reference/cli.md#-python-path-or-version)).

## What works the same { #same }

Collection, scheduling, fixtures, markers, `-n auto`, `--dist`,
`@pytest.mark.serial`, crash recovery and the restart budget, `--reruns`,
`--quarantine`, JUnit and JSON reports, coverage, `--changed`, `--watch`,
`rstest replay` and `rstest bisect` all behave the same. Workers talk to the
orchestrator over anonymous pipes instead of POSIX pipes; nothing about that
is visible to your tests.

## What differs { #differences }

### Worker startup: no `--fork-pool` { #worker-startup }

Every worker is a freshly spawned `python -m rstest_worker` process.
[`--fork-pool`](../reference/cli.md#-fork-pool) is accepted but is a no-op
(there is no `fork()`), and `--doctor` never suggests it. Process and
interpreter startup also costs more on Windows than on Linux or macOS, so the
fixed per-run spawn tax is larger; it matters on short suites and disappears
on long ones. The `startup:` line of [`--doctor`](doctor.md) shows it.

### Timeouts { #timeouts }

`--timeout` and `@pytest.mark.timeout` interrupt a test **in-process** with
SIGALRM on Unix. Windows has no SIGALRM, so a slow test is **not** failed at
its timeout. Only the per-test
[hang watchdog](../concepts/crash-handling.md#hung-tests-worker-timeout)
applies: 3 × the test's timeout + 10 s, after which it kills the worker and
reports the test failed, without a traceback at the stuck line. With
`--timeout 30`, a 60 s test passes and a hung one is killed at 100 s. rstest
says so once per run:

```text
rstest: warning: --timeout can't interrupt a blocked test in-process on Windows (no SIGALRM). ...
```

For a tighter, fixed cap set
[`--worker-timeout SECS`](../reference/cli.md#-worker-timeout-secs), which
also silences the warning. Plugins that rely on SIGALRM, such as
pytest-timeouts, are Unix-only for the same reason.

### Ctrl+C and cancelled jobs { #interrupts }

On Unix, the first SIGINT or SIGTERM during a run stops the workers, names
the test each was running, reports it failed, and still writes the summary,
the [replay journal](replay.md) and any `--junitxml` / `--report-json`.
rstest installs no such handler on Windows: Ctrl+C or a CI job cancellation
ends a parallel run at once, with no summary, journal or reports. In `--watch`,
`q` + Enter between runs is the clean way to quit (exit 0). In CI, keep your
`--timeout` / `--worker-timeout` well under the job limit so a hang ends the
run on its own terms rather than by cancellation.

### Crash messages { #crashes }

A crashed worker is reported the same way, but its detail line reads
`exited with code N` rather than `killed by signal N`. Windows reports a
native crash as an NTSTATUS exit code, printed as a signed number: an
access violation (the Windows segfault, `0xC0000005`) shows as
`exited with code -1073741819`.

### Wait-bound and leak diagnostics { #diagnostics }

- **Child CPU time.** Windows reports no CPU time for child processes, so a
  test that runs a CPU-heavy tool through `subprocess.run` reads as
  **waiting** in [`--doctor`](doctor.md) and the
  [wait-bound](wait-bound.md) analysis, where on Linux and macOS it reads as
  computing.
- **File descriptors.** fd leak tracking reads `/proc/self/fd` or `/dev/fd`,
  neither of which exists on Windows, so it is off there; thread leaks are
  still tracked ([Resource leaks](resource-leaks.md)).

### Terminal output { #terminal }

Colors follow the same rules (`--color`, `PY_COLORS`, `NO_COLOR`,
`FORCE_COLOR`, tty detection). rstest writes ANSI escape sequences and does
not switch the console into virtual-terminal mode itself. Windows Terminal
renders them; if a legacy console window shows raw `←[32m`-style codes, use
`--color=no` or `NO_COLOR=1`. The live per-worker footer is
cut to the terminal width, but on Windows rstest does not query the console
size: it uses `COLUMNS`, else 80. On a narrower window set `COLUMNS` (or
`--color=no`, which also turns the footer off).

### Paths and node ids { #paths }

Node ids always use `/` (`tests/test_api.py::test_login`), on Windows too, so
`--deselect`, JUnit, `--report-json`, the duration cache and replay journals
name tests identically on every platform. Prefer forward slashes in the paths
and node ids you pass too: pytest accepts them everywhere, so one CI script
serves every OS. A replay journal recorded on Windows replays on Linux or macOS
([Keeping a journal portable](replay.md#keeping-a-journal-portable)). Content
hashes are newline-normalized, so a `core.autocrlf` CRLF checkout doesn't
invalidate the duration cache or make `--changed` think every file drifted.

### The cache directory { #cache }

`.rstest_cache` is the same layout. Concurrent writers are serialized with
`LockFileEx` instead of `flock`. Key CI caches by OS (the recipes and the
bundled action already do) rather than sharing one across Windows and Linux
legs. The interpreter-probe cache defaults to `%LOCALAPPDATA%\rstest`
([`RSTEST_CACHE_DIR`](../reference/environment.md#interpreter-probe-cache)).

## How well Windows is validated { #validation }

From the CI configuration, not from code:

- The full test gate (`cargo test`, then `python e2e/gate.py` including the
  Gherkin persona specs) runs on `windows-latest` with Python 3.13 on every
  commit, alongside Linux and macOS.
- The x86_64 wheel is built and smoke-tested (`rstest.exe ... -n 2`) on
  `windows-latest`; the arm64 wheel is built at release on `windows-11-arm`
  but not smoke-tested in CI.
- Not on Windows: the Python 3.10 to 3.14 compatibility matrix, the
  service-backed plugin gates (postgres, playwright, Home Assistant) and the
  bundled GitHub Action's smoke tests run on Linux only, and the 33-suite
  public corpus runs on macOS and Linux only. Gate checks that need Unix
  (fork pool, in-process timeout, SIGINT handling, pty rendering,
  pytest-memray, bash-based CI snippets) are skipped on Windows or replaced
  by a check of the Windows behavior.

Net: supported and gated on every commit, with less real-world-suite
coverage than Linux and macOS ([Known gaps](../concepts/compatibility.md#gap-windows)).

## CI tips { #ci }

**GitHub Actions.** Add `windows-latest` to your OS matrix. In the
[CI quickstart](ci-quickstart.md) steps, the `actions/cache` key's
`${{ runner.os }}` keeps Windows and Linux caches apart. Your own `run:`
steps default to PowerShell on Windows, so a step written in bash syntax
(`$(...)`, `>> "$GITHUB_OUTPUT"`, `\` line continuations, such as the
warm-run lookup in [Shared cache across CI jobs](ci-shared-cache.md)) needs
`shell: bash`, which GitHub's Windows images provide. The bundled action
already runs its own steps under `shell: bash`, though its smoke tests run
on Linux only.

**Azure Pipelines.** The [Azure recipe](ci-recipes.md#azure-pipelines) uses
`vmImage: ubuntu-latest`; for `windows-latest`, its `Cache@2` key already
includes `$(Agent.OS)`. Note that `script:` steps run in `cmd.exe` on Windows
agents.

**Shell commands.** The recipes remove `.rstest_cache/replay` with
`python -c "import shutil; shutil.rmtree('.rstest_cache/replay', True)"`,
which works in bash, PowerShell and cmd.exe alike. In your own steps,
`source .venv/bin/activate` becomes `.venv\Scripts\activate`, and
`--python .venv/bin/python` becomes `--python .venv\Scripts\python.exe`
(or just `--python .venv`, which works on both).

**Hangs.** Because a cancelled run writes nothing on Windows
([above](#interrupts)), set `--worker-timeout` comfortably below the job's
`timeout-minutes:` so a hung test fails the run and the journal and JUnit
still get written.

**Shared files in tests.** Appends from several processes to one file are
not atomic on Windows and can drop lines; rstest's own gate writes
per-worker files for this reason. Give each worker its own file
(`PYTEST_XDIST_WORKER` or the `worker_id` fixture), as
[parallel safety](parallel-safety.md) recommends for any shared state.
