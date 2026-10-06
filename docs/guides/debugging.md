# Debugging a test

A parallel run spreads tests over [workers](../concepts/glossary.md#worker)
whose stdin is `/dev/null`, so neither a `(Pdb)` prompt nor an editor
debugger has anywhere to attach. Every debugging path below therefore runs
the session in [single-worker mode](../concepts/glossary.md#single-worker-mode):
one Python process running pytest itself. Pick the row that matches your
tool:

| You want to... | Run |
|---|---|
| stop at a `breakpoint()` in the terminal | `rstest -n 0 tests/test_x.py::test_y` (or `-s`) |
| drop into pdb when a test fails | `rstest --pdb tests/test_x.py` |
| step through in VS Code or another DAP client | `rstest --debug tests/test_x.py`, then attach |
| step through in PyCharm | `rstest -n 0` with a Python Debug Server |
| debug a failure that only happens in parallel | [Replay](replay.md) or [bisect](parallel-safety.md#diagnosing-a-parallel-only-failure) first |

## `breakpoint()` and pdb

In a parallel run, a `breakpoint()` (or `pdb.set_trace()`) fails that one
test with a hint instead of hanging the worker, and the rest of the run
carries on:

```text
--- FAILED [gw0] tests/test_cart.py::test_total ---
    def test_total():
        items = [(3, 2), (5, 1)]
>       breakpoint()
E       Failed: breakpoint() / pdb.set_trace() needs a terminal, and parallel workers have none: rerun with -n 0 (or -s) to get the (Pdb) prompt

tests/test_cart.py:7: Failed

1 failed, 3 passed in 0.23s
```

Rerun with `-n 0`, `-n 1` or `-s` and you get pytest's own session with a
real prompt on your terminal:

```text
$ rstest -n 0 tests/test_cart.py
...
>>>>>>>>>>>>>>>>>>> PDB set_trace (IO-capturing turned off) >>>>>>>>>>>>>>>>>>>>
> .../tests/test_cart.py(7)test_total()
-> breakpoint()
(Pdb) p items
[(3, 2), (5, 1)]
(Pdb) c
>>>>>>>>>>>>>>>>>>>>> PDB continue (IO-capturing resumed) >>>>>>>>>>>>>>>>>>>>>>
..                                                    [100%]
```

`-n 0` keeps output capture on (pdb suspends it while the prompt is open);
`-s` turns it off for the whole session, so the test's `print` output shows
live. Either works for the prompt.

On a small suite the default `-n auto` often picks single-worker mode by
itself, so a plain `rstest` may already stop at the prompt. Pass `-n 0`
explicitly when you need to be sure.

!!! warning "`--reruns` turns `-n 0` back into a pool"
    With a nonzero `--reruns` (or `reruns` set in `[tool.rstest]`), `-n 0`
    and `-n 1` run a one-worker pool so reruns still apply, and a
    `breakpoint()` fails with a hint saying so. Add `-s`, or pass
    `--reruns 0`.

`--pdb` (post-mortem on failure) and `--trace` (stop at the start of every
test) need the terminal too, so they switch the run to single-worker mode by
themselves. An explicit worker count is ignored, with a warning:

```text
rstest: --pdb runs the session in a single process with pytest's own output, so -n 2 is ignored (no parallel workers); drop --pdb to run in parallel
```

These are [passthrough](../concepts/glossary.md#passthrough) flags; the full
list: [Passthrough-IO flags](../reference/cli.md#passthrough-io-flags).

## Editor debugging with `--debug`

`--debug` starts [debugpy](https://github.com/microsoft/debugpy) inside the
worker and waits for your editor before collecting, so breakpoints in
`conftest.py`, collection code and tests all hit. It forces single-worker
mode like `--pdb`. Install debugpy in the interpreter that runs the tests
(the one `--python` resolves to):

```console
$ pip install debugpy
$ rstest --debug tests/test_cart.py
{"event": "debugpy", "host": "127.0.0.1", "port": 5678}
rstest: debugpy listening on 127.0.0.1:5678; waiting for client...
```

The run now blocks until a client attaches. Bare `--debug` listens on
`127.0.0.1:5678`; pick another port with `--debug=5679` (the `=` is required,
since a bare `--debug 5679` would read `5679` as a test path). The JSON line
on stderr is a ready signal for tools that start rstest and attach for you.

Once attached, editor breakpoints work, and so does a `breakpoint()` in the
test: it pauses in the editor rather than at a `(Pdb)` prompt.

Without debugpy the run is not aborted; it prints a note and runs the session
with no debugger:

```text
rstest --debug: the target interpreter has no `debugpy` installed; run `pip install debugpy` in the test environment. Continuing without a debugger.
```

A port that is already taken degrades the same way:

```text
rstest --debug: could not start debugpy on 5690: Can't listen for client connections: [Errno 48] Address already in use. Continuing without a debugger.
```

`--debug` is refused at a [monorepo](monorepo.md) root, which runs several
sessions; run it inside one project.

### VS Code

With the Python Debugger extension (`ms-python.debugpy`), add an attach
configuration to `.vscode/launch.json`:

```json
{
  "version": "0.2.0",
  "configurations": [
    {
      "name": "Attach to rstest --debug",
      "type": "debugpy",
      "request": "attach",
      "connect": { "host": "127.0.0.1", "port": 5678 },
      "justMyCode": false
    }
  ]
}
```

Start `rstest --debug ...` in the integrated terminal, wait for `waiting for
client...`, then run the configuration (F5). The test output stays in the
terminal; the call stack, variables and stepping are in the editor.
`justMyCode: false` lets you step into fixtures and libraries in
site-packages; drop it to stay in your own code. Change `port` when you use
`--debug=PORT`.

### PyCharm

PyCharm's **Python Debug Server** works in the other direction from
`--debug`: the IDE listens and the test process connects to it. So skip
`--debug` and connect from `conftest.py`:

1. **Run > Edit Configurations > + > Python Debug Server**. Set the host to
   `localhost` and a port, say `12345`. The dialog shows the
   `pydevd-pycharm` version that matches your IDE build.
2. Install that version in the test environment:
   `pip install pydevd-pycharm~=<version from the dialog>`.
3. Connect from `conftest.py`, behind an environment variable so normal
   runs are unaffected:

    ```python
    import os

    if os.environ.get("PYCHARM_DEBUG_PORT"):
        import pydevd_pycharm

        pydevd_pycharm.settrace(
            "localhost",
            port=int(os.environ["PYCHARM_DEBUG_PORT"]),
            suspend=False,
            stdout_to_server=True,
            stderr_to_server=True,
        )
    ```

4. Start the Debug Server configuration, then run the test in one process:

    ```console
    $ PYCHARM_DEBUG_PORT=12345 rstest -n 0 tests/test_cart.py
    ```

Keep `-n 0` (or `-s`): in a parallel run every pool worker would import
`conftest.py` and connect separately.

## A failure that only happens in parallel

Single-worker mode changes the schedule, so a test that fails only under
`-n 4` usually passes the moment you add `-n 0` or `--debug`. Find the cause
first, then debug the one test:

- To re-run CI's exact per-worker schedule, replay the job's journal:
  [Replaying a CI failure locally](replay.md).
- To classify the failure and find the polluting test:
  [Diagnosing a parallel-only failure](parallel-safety.md#diagnosing-a-parallel-only-failure).

Once you know the [polluter](../concepts/glossary.md#polluter), run it and
the victim together in one process (for example `rstest -n 0
tests/test_a.py::test_polluter tests/test_b.py::test_victim`) and set your
breakpoint there.

## With watch mode

`--debug`, `-s`, `--pdb` and `--trace` all compose with
[`--watch`](watch-mode.md). Every cycle starts a fresh worker, so under
`rstest --watch --debug` each rerun prints `waiting for client...` again and
you reattach the editor once per cycle. In these modes the test process owns
stdin, so the between-runs prompt offers only `Ctrl+C` to quit, not `q`.
