# Watch mode

```console
$ rstest --watch
```

runs the suite once, then watches the project and reruns on every save:

```text
2 passed in 0.13s

[watch] waiting for changes... (Ctrl+C to quit, last exit: 0)
[watch] test_w.py changed; rerunning changed files
2 passed in 0.13s
[watch] helper.py changed; rerunning full selection
```

## Rerun policy

- A change set consisting **only of test files** (per your project's
  `python_files` patterns) reruns exactly those files, with all your other
  flags intact.
- Any other `.py` change — source code — runs the tests **affected by the
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

## Combining with other flags

Flags compose; they apply to every rerun:

```console
$ rstest --watch -x            # stop each run at first failure
$ rstest --watch -k login      # only the login tests, on every change
$ rstest --watch -n 2          # bounded parallelism while editing
```

The duration cache and last-failed state update on every cycle, so `--lf`
and slow-test-first scheduling stay warm throughout the session.
