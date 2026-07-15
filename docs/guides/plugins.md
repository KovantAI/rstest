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
- **pytest-rerunfailures**: rstest intercepts `--reruns` and handles
  reruns itself (crash-aware), so the plugin stays inert rather than
  double-rerunning. rstest's `--reruns` needs `-n ≥ 2` (ignored at
  `-n 0/1`).

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
| pytest-randomly | Parallel-unsafe | crashes at `-n ≥ 2` (`KeyError: 'randomly_seed'` — reads a key only xdist's master injects). Pin `--randomly-seed=<n>` to work around, or prefer rstest's native [`--shuffle`](../reference/cli.md#-shuffleseed) (parallel-safe, prints a reproducible seed) |
| pytest-rerunfailures | Parallel-unsafe | with pytest-xdist also installed it raises `KeyError: 'sock_port'` at `-n ≥ 2` (the crash is at plugin *configure*, before rstest's runtime neutralization of its reruns); use rstest's native [`--reruns`](../reference/cli.md#-reruns-n) / `--only-rerun` instead (crash-aware, handles `@mark.flaky`) |
| pytest-html | Parallel-unsafe | its pytest-metadata dependency defines a one-arg `pytest_testnodedown(node)` that rstest's emulation calls with an `error=` kwarg → `TypeError` at `-n ≥ 2`, and no HTML is written. Generate the report at `-n 0` |

The recurring fault line: plugins that read xdist-**master**-injected
`workerinput` keys (`randomly_seed`, `sock_port`, pytest-retry's
`server_port`) crash under the pool, because rstest fills a worker-shaped
`workerinput` but runs no central controller process to set those keys —
the same root as the [master-side-hook / controller-service gaps](../concepts/compatibility.md#known-gaps).
Where a plugin is risky, rstest ships a native equivalent: `--shuffle`
(≈pytest-randomly), `--reruns`/`--only-rerun` (≈pytest-rerunfailures),
`--worker-timeout` (complements pytest-timeout).

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
