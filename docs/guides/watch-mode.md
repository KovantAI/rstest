# Watch mode

`rstest --watch` runs the suite once, then watches the directory you started
it in (recursively) and reruns on every save of a Python or pytest config
file:

```console
$ rstest --watch
```

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
session. The `q` option is left out, and the prompt says only `Ctrl+C`, when
the run hands pytest the terminal (`-s`, `--capture=...`, `--pdb`, `--trace`,
`--co`/`--collect-only`, `--sw`/`--stepwise`, `--sw-skip`/`--stepwise-skip`,
`--sw-reset`/`--stepwise-reset`, rstest's `--debug`) or rstest runs as a
background job (`rstest --watch &`). **Unreleased:** `q` to quit is not in
rstest 0.7.0, whose prompt reads `(Ctrl+C to quit, last exit: 0)`.

Quitting with `q` (Unreleased) exits **0**, whatever the last cycle's result
was: the `last exit` in the prompt is informational, so don't use `--watch` as
a pass/fail gate. An rstest-level error (not a test failure), such as an
invalid `--dist` mode or no usable interpreter, ends the session with exit 1.

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

`rstest --watch --reruns N` retries failures on every cycle, with two
catches: at `-n 0`/`-n 1` it leaves byte-exact mode and runs a one-worker
pool (`RSTEST_WORKER_ID=gw0`), and it is inert under a passthrough flag
(`--pdb`, `-s`, `--co`, ...), which rstest warns about. See
[`--reruns`](../reference/cli.md#-reruns-n).

### Dispatch order and worker count

Watch reruns default to [`--order fail-fast`](../reference/cli.md#-order-throughputfail-fast): tests that recently failed or flaked (from `flakes.json`) run first, then the rest in slow-first throughput order, so a red surfaces as early as possible on each save. Pair it with `-x` to stop at that first red; pass `--order throughput` (or set `[tool.rstest] order`) to opt out. Ordering only applies with two or more workers.

Without `-n`, rstest uses `-n auto`, which caps the pool by test-file count and by cached suite time (about one worker per 2s of tests). A small, fast suite with a warm cache therefore often runs a single worker locally: [byte-exact mode](../concepts/glossary.md#byte-exact-mode), no worker identity, and fail-fast ordering has no effect. Parallel-only failures you see in CI won't reproduce that way; pass `-n 2` or more (`rstest --watch -n 2`) when you want local runs to parallelize like CI.
