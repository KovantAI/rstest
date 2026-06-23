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
  double-rerunning. rstest's `--reruns` needs `-n >= 2` (ignored at
  `-n 0/1`).

## Known limits

- Plugins that *render the terminal* (pytest-sugar, pytest-rich and
  similar progress UIs) don't paint at `-n ≥ 2` — rstest owns the
  terminal. Their non-visual behavior is unaffected; use `-n 0` when you
  specifically want their rendering.
- Plugins registering custom `pytest_runtest_protocol` replacements run,
  but interplay with rstest's item dispatch is only exercised for the
  plugins listed above. Report surprises.
