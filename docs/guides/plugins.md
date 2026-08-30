# Plugins

## How loading works

rstest workers run a vendored pytest core, and plugins load against it
through the standard `pytest11` entry points — the same mechanism pytest
uses. Class identity holds: rstest depends on the real
[pluggy](https://github.com/pytest-dev/pluggy), and the vendored core's
classes live at their usual `_pytest.*` import paths, so plugins that
`isinstance`-check or import internals find what they expect. No plugin
re-installation, configuration, or porting.

Plugin command-line flags forward like any pytest flag; plugin ini options
are read normally.

## Exercised continuously

These load and pass per-test outcome parity against pytest baselines in
rstest's compatibility battery, on real suites:

- pytest-django (incl. per-worker test databases under parallelism)
- pytest-asyncio
- pytest-aiohttp
- hypothesis
- pytest-mock
- pytest-cov (see [Coverage](coverage.md))

## Special-cased

- **pytest-xdist**: neutralized inside workers (rstest owns parallelism);
  its hookspecs stay importable for plugins that implement them. Of the
  master-side hooks, `pytest_configure_node`, `pytest_testnodeready`,
  and `pytest_testnodedown` are emulated (called); the rest —
  `pytest_xdist_make_scheduler`, `pytest_xdist_auto_num_workers`,
  `pytest_handlecrashitem`, `pytest_xdist_node_collection_finished` —
  are silent no-ops: scheduling and crash handling are the Rust
  orchestrator's job and are not extensible from Python.
- **pytest-rerunfailures**: inside pool workers rstest unregisters the
  plugin (before `pytest_configure`, so its xdist `sock_port` client branch
  never fires) and handles reruns itself — crash-aware, honoring
  `@mark.flaky`. rstest's `--reruns` fire at every worker count, including
  `-n 0/1`. At `-n 0` with no `--reruns` the plugin keeps its own behavior.

## Tested compatibility

Beyond the battery above, these common ecosystem plugins were run under both
`-n 0` and the parallel pool and tiered by result. **Works** = correct in
parallel; **caveat** = works with a stated limitation; **parallel-unsafe** =
run it at `-n 0` (or use rstest's native equivalent).

| Plugin | Tier | Note |
|---|---|---|
| pytest-timeout | Works | per-test timeout fires in both modes; complements rstest's own `--worker-timeout` (a hang backstop for C-extension deadlocks) |
| pytest-env | Works | env vars set on every worker |
| pytest-socket | Works | `--disable-socket` blocks identically in parallel |
| pytest-repeat | Works | `@mark.repeat(N)` items distribute across workers |
| freezegun / pytest-freezer | Works | in-process time freezing is per-worker; **but** don't put `now()` in parametrize IDs (see [known gaps](../concepts/compatibility.md#known-gaps)) |
| pytest-benchmark | Caveat | auto-disables at `-n ≥ 2` (sees the pool as xdist); run benchmarks at `-n 0` and read numbers from `--benchmark-json` (the stats table isn't painted — rstest owns the terminal) |
| pytest-order | Caveat | ordering only holds within a worker at `-n ≥ 2`; use `-n 0`, or `--dist loadfile`/`loadscope` to keep an ordered group on one worker |
| pytest-randomly | Works | rstest synthesizes the `randomly_seed` key xdist's master would inject, derived from the run uid so every worker agrees on one reproducible seed. An explicit `--randomly-seed=<n>` still wins. (rstest's native [`--shuffle`](../reference/cli.md#-shuffleseed) remains available and is also parallel-safe.) |
| pytest-rerunfailures | Works | inside pool workers rstest unregisters it *before* `pytest_configure`, so its xdist `sock_port` client branch never fires (the old `KeyError: 'sock_port'` at `-n ≥ 2` with pytest-xdist installed), and rstest owns reruns natively — crash-aware, honoring `@mark.flaky` and [`--reruns`](../reference/cli.md#-reruns-n) / `--only-rerun`. At `-n 0` the plugin keeps its own behavior. |
| pytest-html | Parallel-unsafe | at `-n ≥ 2` **no report is written** — a silent no-op, not a crash. pytest-html registers its report writer only on a node *without* `workerinput` (its xdist "am I the master?" check); every rstest pool worker has a `workerinput`, so nothing ever owns report generation. Merging all workers' results into one file needs a single master process, which rstest doesn't run (the Rust orchestrator owns the merge, and workers are isolated sessions). Generate the report at `-n 0`/`-n 1` (single session, no `workerinput`). |

The recurring fault line: plugins that read xdist-**master**-injected
`workerinput` keys used to crash under the pool when rstest had no master to
supply them. rstest now closes these per plugin: **derivable** keys are
synthesized worker-side (`randomly_seed` — one run-level value every worker
agrees on); **controller-service** keys are handled by each worker playing
master for itself — pytest-retry's branch self-provisions its own report
server per worker (so its `server_port` is set locally, no central
controller needed), and pytest-rerunfailures is unregistered before it can
read `sock_port` (rstest owns reruns instead). The one still-unsupported
case in this table is pytest-html (see its row). Where a plugin is risky,
rstest also ships a native equivalent: `--shuffle` (≈pytest-randomly),
`--reruns`/`--only-rerun` (≈pytest-rerunfailures), `--worker-timeout`
(complements pytest-timeout).

## Checking an unlisted plugin

The lists above aren't exhaustive — your suite likely runs plugins not named
here. Probe one yourself in a minute:

1. Run a slice of your suite at **`-n 0`** — establishes the plugin works at
   all under rstest's vendored core.
2. Run the same slice at **`-n 2`**. If it now crashes (commonly a
   `KeyError` on a `workerinput` key at `pytest_configure`), the plugin
   depends on an xdist-master-injected value rstest doesn't set — run it at
   `-n 0` or find a native equivalent.
3. If it runs but results look wrong (ordering, benchmark timings, a report
   file not written), it's likely order- or terminal-sensitive — see the
   caveat tiers above for the pattern.

`rstest --try` is a fast first pass: it runs your suite under pytest and
under `rstest -n auto` and flags outcome differences, plugins included.

## Known limits

- Plugins that *render the terminal* (pytest-sugar, pytest-rich and
  similar progress UIs) don't paint at `-n ≥ 2` — rstest owns the
  terminal. Their non-visual behavior is unaffected; use `-n 0` when you
  specifically want their rendering.
- Plugins registering custom `pytest_runtest_protocol` replacements run,
  but interplay with rstest's item dispatch is only exercised for the
  plugins listed above. Report surprises.
