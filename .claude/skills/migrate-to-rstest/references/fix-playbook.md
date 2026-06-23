# Fix playbook — one entry per `--migrate-check` finding

For each finding rstest reports, this is the root cause, the **upstream fix**
(removes the problem), and the **rstest stopgap** (caps parallelism but
unblocks). Always prefer the upstream fix; offer the stopgap when the user can't
touch the test now. The deeper background for each class is in the project's
`docs/reference/parity-divergences.md`.

## Table of contents
- Unstable parametrize ids (address / uuid / time)
- NOT PARALLEL-SPECIFIC
- INTRINSIC FLAKE
- ORDER DEPENDENCY
- WALL-CLOCK / LOAD-SENSITIVE
- ISOLATION / CO-LOCATION
- Fixed network port / OS resource

---

## Unstable parametrize ids (address / uuid / time)

**Symptom.** migrate-check: "UNSTABLE NODEIDS … per-process => WILL bail" (for
`address`/`uuid`) or "may bail (timing)" (for `time`).

**Cause.** A `@pytest.mark.parametrize` whose generated **id** isn't stable
across collections. Full-collect dispatch is index-based — every worker must
agree on the ordered id list — so an id embedding a memory address (`0x…`, the
`repr()` fallback for an object), a `uuid4()`, or a timestamp differs per worker
and the workers can't agree → rstest refuses to dispatch (forces `-n 0`).

**Upstream fix.** Give the parametrize explicit, stable `ids=`. The *values*
under test are fine; only the auto-generated id string is unstable.

```python
# before: id falls back to repr() -> embeds 0x… address
@pytest.mark.parametrize("case", CASES)
# after: stable label per case
@pytest.mark.parametrize("case", CASES, ids=[c.name for c in CASES])
```

For ids derived from `datetime.now()` / uuids, either freeze the clock for the
parametrize source or pass `ids=` with fixed labels. `time`-class ids are
usually stable *enough* within one run (all workers collect near-simultaneously)
so they often don't actually bail — fix them anyway to remove the fragility.

**Stopgap.** `-n 0` (the suite stays serial). Pointless to fix with
`--collect lazy` — its file-affine reorder can *break* positional id pairing.

---

## NOT PARALLEL-SPECIFIC

**Cause.** The test fails at `-n 0` too — it's a pre-existing bug or environment
gap, not a parallelism problem. migrate-check summarizes these and sets them
aside.

**Action.** Out of scope for the migration. Tell the user the count and point
them at `rstest -n 0` (≡ pytest) to reproduce. Don't "fix" these as part of
adopting rstest — they were already failing.

---

## INTRINSIC FLAKE

**Cause.** Two serial repeats disagree — the test is nondeterministic under *any*
runner (a real race, an unseeded random, a wall-clock dependency). Not caused by
parallelism; parallelism just made you run it more.

**Upstream fix.** Make it deterministic: seed the RNG, mock the clock, remove the
real race. `--reruns N` will hide it (green on retry) but that masks real
intermittent bugs — prefer fixing.

**Stopgap.** `--reruns 2` (visible, counted, not red).

---

## ORDER DEPENDENCY

**Cause.** Passes serial and under `--dist loadfile`, fails only under `--dist
load` — a **cross-file** coupling: some other file's tests leave state this test
depends on (or is broken by), and `load` can run them on the same worker in a
different order. migrate-check **bisects and names the polluting file**
("POLLUTED BY: …").

**Upstream fix.** Decouple: find what the named polluter leaves behind (module
global, env var, monkeypatch not undone, shared file) and make this test set up
its own state / the polluter clean up after itself (autouse fixture).

**Stopgap.** Run the suite with `--dist loadfile` (keeps each file's tests on one
worker, preserving in-file order) — set it in `[tool.rstest]`.

---

## WALL-CLOCK / LOAD-SENSITIVE

**Cause.** Passes serial, fails parallel, and is **wait-bound** (wall time ≫ cpu
time): the test asserts on real elapsed time / a timeout window, and that
deadline is missed when the machine is oversubscribed. Fails under any parallel
runner at high worker counts.

**Upstream fix.** Don't assert on real time. Mock the clock (`freezegun`, a fake
`time.monotonic`), assert behavior instead of duration, or relax a too-tight
upper bound (a `< 3s` assertion breaks under load).

**Stopgap.** `-n 4` (leave CPU headroom so the deadline holds), or
`@pytest.mark.serial` for the worst offenders.

---

## ISOLATION / CO-LOCATION

**Cause.** Passes serial, fails under both `load` and `loadfile`, and is *not*
wait-bound — a **same-process state leak**: a sibling test (often in the same
file; migrate-check tells you "SAME-FILE co-location" or names the polluting
file) mutates global state this test reads, and parallel co-location runs them
in an order that surfaces it. The classic case: leftover `warnings` filters,
a registry, `sys.modules`/`sys.path`, env vars, logging config.

**Upstream fix.** Reset the leaked state per test. If the report named the
polluter, look at what it mutates and add an autouse fixture (in the victim, or
better, make the *polluter* restore state):

```python
@pytest.fixture(autouse=True)
def _isolate_warnings():
    import warnings
    with warnings.catch_warnings():
        warnings.resetwarnings()
        yield
```

If migrate-check says "not reproducible serially — likely a concurrent-resource
race", it's not state pollution — treat it as a real concurrency bug or a fixed
resource (below).

**Stopgap.** `@pytest.mark.serial` (the marker is auto-registered) — the test
runs alone after the parallel phase.

---

## Fixed network port / OS resource

**Cause.** A session-scoped fixture binds a **fixed port** (or opens a fixed
socket/file). Session scope is per-worker, so every worker binds the same port →
clash/hang. Usually shows up as a hang (the `--worker-timeout` turns it into a
failure) or a bind error in the longrepr.

**Upstream fix.** Bind an **ephemeral** port (`port=0`, then read back the
assigned port) — the standard xdist-safe pattern. Then the fixture duplicates
safely per worker.

**Stopgap.** `@pytest.mark.serial` if the resource is truly single-instance, or
`--dist loadgroup` + `@pytest.mark.xdist_group("name")` to keep all tests using
that resource on one worker.
