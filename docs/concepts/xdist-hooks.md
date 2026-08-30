# xdist hook emulation

pytest-xdist exposes controller-side ("master") hooks that fire in the
controller process around each worker: `pytest_configure_node(node)`,
`pytest_testnodeready(node)`, and `pytest_testnodedown(node, error)`. rstest
has no separate controller process — workers run under the Rust orchestrator —
so these hooks are *emulated*. This page is the reference for how that
emulation behaves and where it diverges from a single xdist controller. For
the migration overview, see
[Migrating from pytest-xdist](../guides/migrate-from-xdist.md).

## How the emulation works

Each worker plays controller for itself, calling your implementations against
a node shim with the worker's own `workerinput`, `gateway.id`, and `config`.
Implementations registered mid-`pytest_configure` are caught (the call fires
synchronously at plugin registration).

A `configure_node` call that fails because the state it reads is not set yet —
e.g. a plugin registers its hook, then on later lines stashes the value the
hook reads — is deferred, not fatal: the registration-time call is lenient,
and a strict retry runs at `pytest_sessionstart`, once every plugin's own
`pytest_configure` has completed and the state is populated. This covers both
patterns: SQLAlchemy (self-contained, succeeds at registration) and pytest-retry
(reads a `config.stash` server port set after it registers its hook — succeeds
on the sessionstart retry).

## `numprocesses` visibility

xdist's own distributed session is kept inert by forcing `dist = "no"` (its
`DSession` only registers when `dist != "no"`), **not** by zeroing
`numprocesses`. The pool width is left visible on `config.option.numprocesses`
on purpose: third-party plugins gate their parallel-master setup on it. The
load-bearing example is pytest-retry, whose
`has_plugin("xdist") and getoption("numprocesses")` branch starts a
`ReportServer` and stashes its (ephemeral) port; if `numprocesses` were nulled
the plugin would instead take its *worker* branch and read a
`workerinput["server_port"]` no master had set, raising `KeyError`. Surfacing
the width lets each worker self-provision its own server, the same way the
follower-DB pattern self-provisions.

## When self-provisioning can't apply: pytest-rerunfailures

Not every controller-service plugin can be steered to a master branch.
pytest-rerunfailures gates its master/client split on **`workerinput`
presence** (`is_master = not hasattr(config, "workerinput")`), not on
`numprocesses` — and every rstest pool worker has a `workerinput`, so the
plugin always takes its *client* branch and reads
`workerinput["sock_port"]`, a key only an xdist master sets. There is no
knob (as there is for pytest-retry) to flip it to the self-provisioning
branch. rstest wants the plugin inert under the pool anyway — it owns reruns
natively (crash-aware, `@mark.flaky`), and leaving the plugin active would
double-rerun — so it simply **unregisters** it. The timing matters:
`pytest_configure` is dispatched via `call_historic`, which snapshots the
impl list, so unregistering during configure is too late (the already-queued
`pytest_configure` still runs and `KeyError`s). rstest therefore drops the
plugin in `pytest_cmdline_main`, after entry-point plugins load but before
`_do_configure`. At `-n 0` (no worker id) the plugin is left untouched and
keeps its native behavior.

For hooks that are **pure functions of the node** — read `node.gateway.id`,
fill `node.workerinput`, provision a resource derived from them — the
emulation produces the same observable result as xdist. The call *timing*
differs: xdist fires in the controller before the worker exists; rstest fires
inside the worker, before other plugins' `pytest_configure` read
`workerinput`. SQLAlchemy's uuid-based `follower_ident` is this pattern, and
runs measured — see
[compatibility](compatibility.md#measured-at-scale).

## Divergences from a single controller

- **Your hook runs N times concurrently, in N processes.** xdist's one master
  serializes `pytest_configure_node` calls and can keep shared bookkeeping
  across them; per-process emulation cannot. Hooks that allocate from
  controller-side shared state (counters, registries, pools) need rework —
  derive everything from `gateway.id` or a uuid.
- **`pytest_testnodedown` for a CRASHED worker runs on a surviving worker**
  that never saw the dead node's `configure_node`. The shim carries the dead
  worker's `workerinput` snapshot, so workerinput-keyed cleanup works;
  conftest-side registries keyed at configure time will miss. Make teardown a
  function of `node.workerinput` alone.

On the normal path, a worker's own `pytest_testnodedown` fires at session
finish — after the run-test loop has torn down all fixtures (session scope
included), the same ordering xdist's controller observes. Database drops in
that hook run after your session fixtures have finalized.

## conftest hooks run per-worker

The master-side node hooks above are the special case. **Every other
conftest hook** — `pytest_configure`, `pytest_collection_modifyitems`,
`pytest_sessionstart`/`pytest_sessionfinish`, `pytest_runtest_*`, and your
own — runs inside each worker, because each worker is a full pytest session.
So at `-n 8` a `pytest_configure` fires eight times, once per process; this
is the same model as xdist, where each worker also runs its own
`pytest_configure`. Consequences:

- A hook that mutates shared external state (writes a file, seeds a DB,
  increments a counter) runs N times concurrently — make it idempotent or
  key it on `RSTEST_WORKER_ID` / `workerinput["workerid"]`.
- In the default (`--collect full`) mode each worker performs a **full**
  collection — `pytest_collection_modifyitems` sees the *entire* suite on
  every worker, exactly as under xdist; the orchestrator then dispatches only
  that worker's share to run. Under [`--collect lazy`](../reference/cli.md#-collect-fulllazy)
  the orchestrator assigns files and each worker collects only its assigned
  files on demand, so the hook sees a partial item set — run at `-n 0` (or
  `--collect full`) if a hook must see the whole suite.
- **`pytest_collection_modifyitems` reordering does not control parallel run
  order.** Deselection is honored (a deselected item won't run), but the
  *order* you impose is ignored at `-n ≥ 2`: the orchestrator dispatches by
  index into the verified collection, duration-first. Run order is governed
  only by `--dist` mode, `@pytest.mark.serial`, and `xdist_group` — not by a
  reordering hook. A suite relying on such a hook for ordering needs `-n 0`
  or an affinity `--dist` mode.
- Hooks assuming single-process semantics (a module-level global that
  accumulates across the run) will not see other workers' contributions.

## Crash cleanup

Crash cleanup is best-effort and **weaker than xdist's**: xdist's master is a
separate always-alive process; rstest needs a surviving worker (if the last
worker crashes, cleanup is skipped with a loud warning). One ordering hazard
to know: the crashed worker's REPLACEMENT starts while the survivor runs the
dead node's `pytest_testnodedown` — with idents derived deterministically
from `gateway.id`, the drop can race the replacement's re-provisioning of the
same ident. Use uuid-based idents (as SQLAlchemy does) and the race
disappears: the replacement provisions a fresh ident, the survivor drops the
old one. See [Crash handling](crash-handling.md) for the surrounding model.
