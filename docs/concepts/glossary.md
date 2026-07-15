# Glossary

**Worker** — a Python process (`gw0`, `gw1`, ...) running your project's
interpreter with the vendored pytest core; executes tests and streams
reports to the orchestrator.

**Orchestrator** — the `rstest` binary: spawns workers, dispatches tests,
merges results, renders output.

**Master / controller** — pytest-xdist's term for its central coordinating
process. rstest has no such process — the Rust **orchestrator** plays that
role — so "master-side" xdist hooks are *emulated* per worker. See
[xdist hook emulation](xdist-hooks.md).

**Vendored core** — the unmodified copy of pytest shipped inside
`rstest_worker._vendor`; provides all test semantics. Never conflicts with
an installed pytest.

**Item dispatch** — distributing individual tests (not files) to workers
by index into the verified collection.

**Long-pole** — a test whose duration exceeds the ideal per-worker share;
it caps the wall time of any parallel run. Dispatched first, individually.

**Chunk** — a contiguous run of collection order dispatched as one unit,
preserving module-fixture locality.

**nextitem invariant** — a worker never runs its final pending test until
it knows the successor (teardown scoping requires it); queues must always
end explicitly.

**Designate** — the worker chosen to host the serial phase and to ship the
full collection id list.

**Serial phase** — `@pytest.mark.serial` tests running exclusively on the
designate after all other workers finish.

**Flaky** — a test that failed and then passed within the
[`--reruns`](../reference/cli.md#-reruns-n) budget; reported green but
counted and listed.

**Byte-exact mode**{#byte-exact-mode} — `-n 0` and `-n 1` are identical: one in-process
pytest session, no worker, no `[gwN]` attribution, byte-exact pytest
behavior — the compatibility anchor. There is no worker identity below
`-n 2` (unlike pytest-xdist, whose `-n 1` spawns a `gw0` worker — see
[xdist migration](../guides/migrate-from-xdist.md)). The flags that need
pytest's own terminal (`--co`, `-s`, `--capture`, `--pdb`, `--trace`)
switch to this mode automatically. See [Compatibility](compatibility.md)
for the guarantee and [Architecture](architecture.md) for how it falls
back.

**Selection** — the set of tests chosen to run; under
[`--changed`](../reference/cli.md#-changedrev), derived from the import
graph.
