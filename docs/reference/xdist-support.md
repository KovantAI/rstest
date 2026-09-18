# xdist support matrix

## Who this is for

You are moving a suite off pytest-xdist and need one lookup: for each xdist flag, hook, and worker-identity fixture, does rstest support it, emulate it, or drop it? This page answers that and links to the deeper treatment of each item.

The baseline guarantee: at `-n 0` (or `-n 1`) rstest is a single vendored-pytest session and outcomes are **byte-exact** to pytest: any difference there is a bug (see [Compatibility](../concepts/compatibility.md)). rstest replaces xdist rather than wrapping it; the worker environment is xdist-shaped on purpose so plugins keep working. The migration risk is concentrated in the two areas below: flags that silently no-op, and hooks that run per-worker instead of once. The worker-identity fixtures (`worker_id`, `testrun_uid`) are provided natively, so they are one thing you do *not* have to worry about.

For the narrative version see [Migrating from pytest-xdist](../guides/migrate-from-xdist.md).

## Flag matrix

| pytest-xdist flag | rstest equivalent | Status / notes |
|---|---|---|
| `-n <N>` / `-n auto` | `-n <N>` / `-n auto` | Same. `auto` is logical cores, capped for small suites; it is the default. |
| `-n 1` | `-n 1` (**differs**) | xdist's `-n 1` runs one `gw0` worker WITH `workerinput`; rstest's `-n 1` (like `-n 0`) is plain byte-exact mode with **no worker identity**. |
| `--dist load` | `--dist load` (default) | Same, plus duration-aware long-pole-first scheduling. |
| `--dist loadfile` | `--dist loadfile` | Same. File affinity, in-file order. |
| `--dist loadscope` / `loadgroup` | same names | Supported, incl. `@pytest.mark.xdist_group`; rejected under `--collect lazy` (needs full collection). See [`--dist`](cli.md). |
| `--dist each` | `--dist each` (**partial**) | Full suite per worker, but every worker uses the SAME interpreter. Heterogeneous `--tx` gateways have no equivalent. `--reruns` rejected in this mode. |
| `--dist no` / `--dist=no` | none | **rstest error, exit 2** (`no` is not a valid `--dist` mode); single-worker is `-n 0`. |
| `-d` | `--dist load` | `-d` is xdist's load-balancing shorthand, which is rstest's default. Forwarded verbatim (see below), no effect. |
| `--maxprocesses` | none | Use `-n` (no separate cap). Forwarded verbatim, no effect. |
| `--max-worker-restart` | none | No equivalent: rstest auto-respawns crashed workers on a fixed, non-tunable budget (see [Crash handling](../concepts/crash-handling.md)). Forwarded verbatim, no effect. |
| `--tx` (gateways) | none | No equivalent: one local interpreter. `--dist each` covers same-env broadcast, not heterogeneous environments. |
| `--rsyncdir` / `--rsync` | none | No equivalent: rstest runs local workers, no remote sync. |
| `-p xdist.looponfail` / `--looponfail` | `--watch` | With import-graph selection. See [`--watch`](cli.md). |

**What happens to an unsupported xdist flag?** `--dist no`/`--dist=no` is consumed by rstest's own `--dist` and rejected as an invalid mode (exit 2). The rest (`--tx`, `--rsync*`, `-d`, `--max-worker-restart`, `--maxprocesses`) are **forwarded to the vendored pytest session verbatim**, so the outcome depends on whether pytest-xdist is installed:

- **pytest-xdist installed** (usual mid-migration): the flag *parses* but has **no effect**: rstest keeps xdist's session inert (`dist = no`), so nothing acts on it. Silently ignored, no error, no warning.
- **pytest-xdist not installed**: pytest doesn't recognize the option: usage error from the vendored core (exit 4).

Either way these flags don't *do* anything under rstest; remove them from `addopts` once the switch is done. If `addopts = -n 4` with pytest-xdist installed is still in your ini, it is neutralized inside rstest workers automatically (options parse, the xdist session never engages, no nested workers). Remove it at your convenience and pass `-n` to rstest.

## Hook matrix

xdist's controller-side ("master") hooks fire in the controller process around each worker. rstest has no separate controller process, so the supported ones are **emulated**: each worker plays controller for itself, calling your implementation against a node shim with its own `workerinput`, `gateway.id`, and `config`. Full semantics: [xdist hook emulation](../concepts/xdist-hooks.md).

| xdist hook | Status |
|---|---|
| `pytest_configure_node(node)` | **Emulated.** Runs per worker; hooks that are pure functions of the node (read `gateway.id`, fill `workerinput`, provision from them, such as SQLAlchemy's `follower_ident`) produce the same observable result as xdist. |
| `pytest_testnodeready(node)` | **Emulated.** Runs per worker against the node shim. |
| `pytest_testnodedown(node, error)` | **Emulated.** Fires at session finish on the normal path; a **crashed** worker's `pytest_testnodedown` runs on a *surviving* worker, so teardown must be a function of `node.workerinput` alone. |
| `pytest_xdist_auto_num_workers` | Not documented / not emulated. |
| `pytest_xdist_make_scheduler` | Not documented / not emulated. There is **no custom-scheduler plug point**: `--dist` is a fixed set of modes with no override. |
| `pytest_xdist_newgateway` | Not documented / not emulated. |
| `pytest_xdist_setupnodes` | Not documented / not emulated. |
| `pytest_handlecrashitem` | Not documented / not emulated. |

Two structural caveats on the three emulated hooks: they run **N times concurrently in N processes** (controller-side shared state such as counters, registries, and pools needs rework; derive everything from `gateway.id` or a uuid), and crashed-node teardown runs on a survivor that never saw the dead node's `configure_node`. Every **other** conftest hook (`pytest_configure`, `pytest_collection_modifyitems`, `pytest_sessionstart`/`finish`, `pytest_runtest_*`) also runs inside each worker (the same model as xdist), so make shared-state hooks idempotent or key them on `RSTEST_WORKER_ID` / `workerinput["workerid"]`. Note also that `pytest_collection_modifyitems` **reordering does not control parallel run order** at `-n ≥ 2` (deselection is honored; ordering is governed by `--dist` mode, `@pytest.mark.serial`, and `xdist_group`).

## Fixtures & worker identity

**rstest provides native `worker_id` and `testrun_uid` fixtures**, with semantics identical to pytest-xdist's, so `def test(worker_id): ...` resolves whether or not pytest-xdist is installed. Removing pytest-xdist from your config does not lose them.

- `worker_id`: the worker the test runs on, `gw0`, `gw1`, ..., or `"master"` below `-n 2` (single-worker mode, no worker identity).
- `testrun_uid`: one uid shared by every worker in a run. Below `-n 2` a fresh uid is generated per session, matching xdist's standalone behavior.

When pytest-xdist is also installed its own same-named fixtures are present too. Because rstest's values match xdist's exactly, whichever definition wins the resolution returns the same result, so the duplicate is harmless.

Under the fixtures, rstest sets the full xdist-compatible surface on every pool worker, for plugins and conftests that read it directly:

- `RSTEST_WORKER_ID` env var, the `gwN` value, rstest-specific.
- `PYTEST_XDIST_WORKER` and `PYTEST_XDIST_WORKER_COUNT` env vars, set so environment-grepping plugins/conftests keep working.
- `config.workerinput`, which carries `workerid` (`gwN`), `workercount`, `testrun_uid` (one uid per run, shared by all workers), `mainargv`, and the `cov_master_*` keys pytest-cov expects.

Reading the surface directly, instead of via the fixtures, also works:

```python
worker = getattr(request.config, "workerinput", {}).get("workerid", "master")
```

**Values at `-n 0` / `-n 1`.** There is **no `workerinput`** below `-n 2`: `config.workerinput` does not exist and `RSTEST_WORKER_ID`, `PYTEST_XDIST_WORKER`, and `PYTEST_XDIST_WORKER_COUNT` are all unset. The `worker_id` fixture returns `"master"` there and `testrun_uid` a fresh per-session uid. This differs from xdist's `-n 1`, which *does* create a `gw0` worker with `workerinput`. Code that reads `config.workerinput` directly (rather than via the fixtures) must still guard for the single-worker case.

Plugins keyed on worker identity work unchanged: pytest-django's per-worker test databases are the canonical case. And `hypothesis`'s shared example DB under many workers can be split per worker with `DirectoryBasedExampleDatabase(f".hypothesis/{os.environ.get('RSTEST_WORKER_ID', 'master')}")` (see [Compatibility](../concepts/compatibility.md)).

## Known divergences

These are not rstest bugs: they are catalogued differences and design choices. Full catalogue: [Parity divergences](parity-divergences.md).

- **Scheduling / run order.** `pytest_collection_modifyitems` reordering is ignored at `-n ≥ 2`; the orchestrator dispatches by index into the verified collection, duration-first. Order is controlled only by `--dist` mode, `@pytest.mark.serial`, and `@pytest.mark.xdist_group`. Suites relying on a reordering hook need `-n 0` or an affinity `--dist` mode. See [Markers](markers.md).
- **Crash cleanup is weaker than xdist's.** xdist's master is a separate always-alive process; rstest needs a *surviving* worker to run a dead node's `pytest_testnodedown`: if the last worker crashes, cleanup is skipped with a loud warning. The replacement worker can also race the survivor's drop; use uuid-based idents (as SQLAlchemy does) and the race disappears. See [xdist hook emulation](../concepts/xdist-hooks.md).
- **Controller-side shared services aren't emulated.** A plugin needing one service shared across the whole pool has no central controller to host it; known cases (pytest-retry, pytest-rerunfailures) are handled per worker or neutralized in favor of native `--reruns`. See [Parity divergences](parity-divergences.md).
- **Unstable parametrize IDs force `-n 0`.** IDs built from a memory address, uuid, or `now()` make per-worker collections disagree and rstest refuses to dispatch (same constraint as xdist). `rstest migrate-check` names the exact site before your first run.

## Go deeper

- [Migrating from pytest-xdist](../guides/migrate-from-xdist.md): the full flag map, what your plugins see, and what improves.
- [xdist hook emulation](../concepts/xdist-hooks.md): how the three node hooks are emulated, `numprocesses` visibility, and per-worker conftest semantics.
- [Compatibility](../concepts/compatibility.md): the `-n 0` byte-exact contract, the measured battery, and the known-gaps table (incl. xdist master-side hooks).
- [Parity divergences](parity-divergences.md): every catalogued reason a suite diverges and the upstream fix.
- [Markers](markers.md): `@pytest.mark.serial`, `@pytest.mark.flaky`, `@pytest.mark.xdist_group`.
- [CLI](cli.md): `-n`, `--dist`, and the full forwarded-flag surface.
