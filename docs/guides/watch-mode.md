# Watch mode

`rstest --watch` runs the suite once, then watches the directory you started
it in (recursively) and reruns on every save of a Python or pytest config
file:

```console
$ rstest --watch
```

```text
rstest 0.9.0 — 2 workers (parallel by default; -n 0 for single-worker mode)
...                                                                      [100%]

3 passed in 0.16s

[watch] waiting for changes... (q + Enter or Ctrl+C to quit, last exit: 0)
[watch] test_w.py changed; rerunning changed files
============================= test session starts ==============================
platform darwin -- Python 3.13.13, pytest-9.1.1, pluggy-1.6.0
rootdir: /Users/me/proj
collected 2 items

test_w.py ..                                                             [100%]

============================== 2 passed in 0.00s ===============================

[watch] waiting for changes... (q + Enter or Ctrl+C to quit, last exit: 0)
[watch] helper.py changed; rerunning affected tests
============================= test session starts ==============================
platform darwin -- Python 3.13.13, pytest-9.1.1, pluggy-1.6.0
rootdir: /Users/me/proj
collected 1 item

test_h.py .                                                              [100%]

============================== 1 passed in 0.00s ===============================
```

Under the default `-n auto` rstest starts about one worker per 2 seconds of
cached test time, so a quick suite like this one reruns on a single worker and
prints pytest's own session output; a slower suite reruns in parallel and
prints rstest's summary.

Type `q` (or `quit`) and Enter between runs, or press Ctrl+C, to end the
session. The `q` option is left out, and the prompt says only `Ctrl+C`, when
the run hands pytest the terminal (`-s`, `--capture=...`, `--pdb`, `--trace`,
`--co`/`--collect-only`, `--sw`/`--stepwise`, `--sw-skip`/`--stepwise-skip`,
`--sw-reset`/`--stepwise-reset`, rstest's `--debug`): stdin belongs to the
test process there, so rstest does not read it.

Closing stdin (`nohup`, `< /dev/null`) does not end the session. Started as a
background job (`rstest --watch &`), rstest leaves stdin alone so the shell
doesn't suspend it, and `q` is not offered; stop it with `kill` or `fg`. A
session backgrounded later (Ctrl+Z, then `bg`) may be suspended for tty input;
`fg` resumes it.

Quitting with `q` exits **0**, whatever the last cycle's result was: the
`last exit` in the prompt is informational, so don't use `--watch` as a
pass/fail gate. Ctrl+C ends the process by signal. An rstest-level error (not
a test failure), such as a bad flag combination, an invalid `--dist` mode, a
failed `--cache-pull` or no usable interpreter, ends the session with exit 1.

## Rerun policy

--8<-- "docs/_snippets/watch-rerun-policy.md"

Save-bursts from editors are debounced (300ms), and the screen clears
between runs on a terminal.

### New test files

Creating a new test file is picked up like any other save: a new file
matching `python_files` is a test-file change, so the next cycle reruns
exactly that file. Reruns keep your flags but drop the positional paths
you started with, so a session started as `rstest --watch tests/unit`
still runs a new `tests/other/test_x.py` when you save it. Flags such as
`-k` still filter it, and an option's value is never treated as a path to
drop (`-k api` keeps `api` even when an `api/` directory exists; see
[forwarded pytest flags](../reference/cli.md#forwarded-pytest-flags)).

## Per-cycle cost

Each cycle spawns fresh workers at every worker count (nothing is reused
between cycles), so an edited module is always re-imported from scratch and a
rerun cannot show a stale-import false green. The price is a small fixed cost
on top of your tests' own time. Measured from save to result on a one-test
project (Apple M4 Max, rstest 0.8.0, CPython 3.13; median of 10 saves, min-max
in parentheses):

| Worker count | Save to rerun start | Rerun (spawn, collect, test, report) | Save to result |
|---|---|---|---|
| `-n 0` | 314ms (312-317) | 162ms (159-168) | 476ms (472-482) |
| `-n 2` | 314ms (311-317) | 202ms (181-211) | 517ms (497-523) |

The first column is the 300ms debounce plus file-event delivery; the rerun
itself costs 160-200ms here, growing with `-n`. Reproduce with
`python3 corpus/watch_cycle.py --out <file>`; the measured run is
[`corpus/bench-results/2026-10-06-watch-cycle.json`](https://github.com/KovantAI/rstest/tree/main/corpus/bench-results).

Selection is **incremental** across the session: the import graph stays warm,
and each save re-parses only files whose mtime or size changed (adding or
deleting a `.py` file rebuilds the graph from cached parses). On a large tree
that keeps reselection to tens of milliseconds per save, roughly 4x faster
per save than a full rebuild on a 1,600-file tree.

## Combining with other flags

Flags compose; they apply to every rerun:

```console
$ rstest --watch -x            # stop each run at first failure
$ rstest --watch -k login      # only the login tests, on every change
$ rstest --watch -n 2          # bounded parallelism while editing
```

The duration cache and last-failed state update on every cycle, so `--lf`
and slow-test-first scheduling stay warm throughout the session.

`rstest --watch --reruns N` retries failures on every cycle, with two
catches: at `-n 0`/`-n 1` it leaves single-worker mode and runs a one-worker
pool (`RSTEST_WORKER_ID=gw0`), and it is inert under a passthrough flag
(`--pdb`, `-s`, `--co`, ...), which rstest warns about. See
[`--reruns`](../reference/cli.md#-reruns-n).

### Dispatch order and worker count

Watch reruns default to
[`--order fail-fast`](../reference/cli.md#-order-throughputfail-fast): tests
that recently failed or flaked (from `flakes.json`) run first, then the rest
in slow-first throughput order, so a red surfaces as early as possible on each
save. Pair it with `-x`/`--maxfail=1` to stop at that first red; pass
`--order throughput` (or set `[tool.rstest] order`) to opt back into packing.
Ordering only applies with two or more workers.

Without `-n`, rstest uses `-n auto`, which caps the pool by test-file count
and by cached suite time (about one worker per 2s of tests), and sizes it
again on every cycle. A small, fast suite with a warm cache therefore often
runs a single worker locally:
[single-worker mode](../concepts/glossary.md#single-worker-mode), no worker
identity, and fail-fast ordering has no effect. Parallel-only failures you see
in CI won't reproduce that way; pass `-n 2` or more (`rstest --watch -n 2`)
when you want local runs to parallelize like CI.
