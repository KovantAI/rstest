# Architecture

rstest is two programs with a sharp boundary between them:

```
┌─ rstest (Rust) ────────────────────────────────┐
│ CLI · scheduling · progress/failure rendering  │
│ duration cache · crash recovery · merged       │
│ reports (summary, junitxml, lastfailed)        │
└──────────────┬─────────────────────────────────┘
               │ msgpack over dedicated pipes
┌──────────────▼─ worker pool (N processes) ─────┐
│ your project's Python interpreter              │
│ rstest_worker + vendored pytest core           │
│ collection · fixtures · plugins · test         │
│ execution — everything semantic                │
└────────────────────────────────────────────────┘
```

The Rust orchestrator owns everything *around* the tests; a vendored
pytest core owns everything *about* the tests.

## Why a vendored pytest, not a reimplementation

pytest compatibility is not an API — it's ten years of semantics: fixture
finalization order, conftest discovery rules, assertion rewriting, the
exact behavior of `importorskip` at collection time. Every prior attempt
at a pytest-compatible runner died reimplementing this surface.

There's a second, harder constraint: **plugins import pytest internals.**
Many of the most widely used pytest plugins import from `_pytest.*`, and
plugins check class identity against the real `pluggy` library. Only the
genuine code satisfies them.

So rstest vendors pytest verbatim (currently 9.0.3) inside
`rstest_worker._vendor`, shadows it onto `sys.path` inside worker
processes — never touching a pytest installed in your environment — and
depends on the real pluggy. Plugins load through normal `pytest11` entry
points and find exactly the classes they expect.

## How a parallel run works

1. **Spawn.** N workers start in your project's interpreter, each
   announced with an xdist-style identity (`gw0`, `gw1`, ...) that plugins
   like pytest-django key resources on.
2. **Collect.** Every worker runs identical pytest collection (same args,
   same ini, same conftest semantics — this is what keeps skip/marker
   behavior exact). Workers verify they collected the same test set by
   count and hash; one worker ships the full id list.
3. **Dispatch.** The orchestrator feeds item indices: cached slow tests
   first (individually, so they spread across workers), then contiguous
   chunks that preserve module-fixture locality. Workers run each test
   through pytest's own `runtest_protocol`, with correct `nextitem`
   teardown semantics.
4. **Stream.** Every test phase reports back over the pipe as it happens —
   progress, failures with captured output, warnings — and the
   orchestrator renders, merges, and accounts exactly as pytest would.
5. **Wind down.** When the queue empties, workers release their held
   items but stay connected — a failed test elsewhere may still be rerun
   on them (`--reruns`). Once every test's outcome is final, an explicit
   end-of-session signal lets each worker run its session-fixture
   finalizers. `@pytest.mark.serial` tests run only after every other
   worker has finished its session — exclusively, on a single worker — then
   exit codes merge.

The protocol deliberately never rides stdin/stdout — those belong to your
tests (and to pytest itself under `-s`/`--pdb`).

## Single-worker mode

`-n 0` skips the scheduling layer entirely: one worker, one session,
`pytest.main()` over your args. This mode is the compatibility anchor —
byte-exact pytest behavior — and the automatic fallback for flags that
need pytest's own terminal.
