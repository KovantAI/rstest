# Watch mode

```console
$ rstest --watch
```

runs the suite once, then watches the project and reruns on every save:

```text
2 passed in 0.13s

[watch] waiting for changes... (q + Enter or Ctrl+C to quit, last exit: 0)
[watch] test_w.py changed; rerunning changed files
2 passed in 0.13s

[watch] waiting for changes... (q + Enter or Ctrl+C to quit, last exit: 0)
[watch] helper.py changed; rerunning affected tests
1 passed in 0.11s
```

Type `q` (or `quit`) and Enter between runs, or press Ctrl+C, to end the
session. The `q` option is left out, and the prompt says only `Ctrl+C`, when a
test owns stdin (`-s`, `--pdb`, `--trace`, `--debug`) or rstest runs as a
background job (`rstest --watch &`). **Unreleased:** `q` to quit is not in
rstest 0.7.0, whose prompt reads `(Ctrl+C to quit, last exit: 0)`.

## Rerun policy

- A change set consisting **only of test files** (per your project's
  `python_files` patterns) reruns exactly those files, with all your other
  flags intact.
- Any other `.py` change (source code) runs the tests **affected by the
  change** per the project import graph (same machinery as
  [`--changed`](../reference/cli.md#-changedrev)); a change affecting no
  tests skips the rerun, and changes the graph can't reason about fall
  back to the full selection.
- Changes to pytest configuration files (`pyproject.toml`, `pytest.ini`,
  `setup.cfg`, `tox.ini`) also trigger a full rerun.
- VCS internals, `__pycache__`, virtualenvs, and rstest's own caches are
  ignored.

Save-bursts from editors are debounced (300ms), and the screen clears
between runs on a terminal.

### New test files

Creating a new test file is picked up like any other save: a new file
matching `python_files` is a test-file change, so the next cycle reruns
exactly that file. Reruns keep your flags but drop the positional paths
you started with, so a session started as `rstest --watch tests/unit`
still runs a new `tests/other/test_x.py` when you save it. Flags such as
`-k` still filter it.

## Per-cycle cost

Each cycle spawns fresh workers (nothing is reused between cycles), so a
rerun has a small fixed cost on top of your tests' own time. Measured from
save to result on a one-test project on a development laptop:

| Worker count | Save to result |
|---|---|
| `-n 0` | ~400ms |
| `-n 2` | ~405ms |

That is the 300ms debounce plus roughly 100ms for worker startup and
collection. Worker startup grows slightly with `-n`, and on a large tree
the incremental import-graph reselection adds tens of milliseconds per save
(see [`--watch`](../reference/cli.md#-watch)).

## Combining with other flags

Flags compose; they apply to every rerun:

```console
$ rstest --watch -x            # stop each run at first failure
$ rstest --watch -k login      # only the login tests, on every change
$ rstest --watch -n 2          # bounded parallelism while editing
```

The duration cache and last-failed state update on every cycle, so `--lf`
and slow-test-first scheduling stay warm throughout the session.
