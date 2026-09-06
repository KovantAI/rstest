# Serve protocol

**Experimental.** The wire protocol a client speaks to an
[`rstest --serve <SOCK>`](cli.md#-serve-sock) daemon — a warm-pool server that
collects a suite once and runs on-demand nodeid subsets, each in a forked child
so a source overlay (a mutation) can't leak into the next run. Built for
mutation-testing tools that would otherwise pay a full interpreter + plugin
startup per mutant.

Unix only; a single client, sequential runs.

## Transport & framing

A Unix-domain stream socket. Every message is a **msgpack map**
`{"kind": <str>, "payload": <map>}`, written back-to-back (no length prefix) —
the same framing rstest uses over its worker pipes. Both directions stream these
envelopes; a decode error or EOF ends the session.

## Message flow

```
client                          daemon
  │ ── hello ──────────────────► │
  │ ◄──────────────── welcome ── │
  │ ── open_session ───────────► │   (spawns a warm worker, collects once)
  │ ◄─────────── session_ready ─ │
  │ ── run ────────────────────► │   (forks a child, applies overlay)
  │ ◄──────────────── report ─── │   (one per test phase, streamed)
  │ ◄──────────────── report ─── │
  │ ◄──────────────── run_done ─ │
  │            … more run …      │
  │ ── shutdown ───────────────► │
  │ ◄────────────────────  bye ─ │
```

## Client → daemon

| kind | payload | meaning |
|---|---|---|
| `hello` | `{proto: 1}` | handshake |
| `open_session` | `{args: [str]}` | collect the suite once and warm a worker. `args` are pytest session args (paths, `-k`, …); empty falls back to the daemon's own CLI args |
| `run` | `{id: int, node_ids: [str], patch?, stop_on_first_fail?: bool}` | run a nodeid subset; `id` correlates the reply. See **Patch** below |
| `close_session` | `{}` | tear down the warm worker (daemon stays up) |
| `shutdown` | `{}` | tear down and exit |

### Patch (the overlay / mutation carrier)

`run.patch` is optional:

```json
{"mode": "overlay", "files": {"pkg/mod.py": "<full replacement contents>"}}
```

The named files are written over on disk before the forked run and restored
after it, so the child imports the mutated source fresh. Absent `patch` (or
`{"mode": "none"}`) runs the current working tree.

## Daemon → client

| kind | payload | meaning |
|---|---|---|
| `welcome` | `{proto: 1, server: str}` | handshake reply |
| `session_ready` | `{collected: int}` | collection done, worker warm |
| `report` | `{id: int, report: {...}}` | one per-phase test report for run `id` (see below) |
| `run_done` | `{id: int, killed: bool, ran: int}` | run finished. `killed` = any test failed/errored; `ran` = tests executed |
| `error` | `{code: str, message: str}` | `collect_failed` / `bad_session` / `bad_request` (includes an empty `node_ids`) / `bad_frame` |
| `bye` | `{}` | reply to `close_session` / `shutdown` |

### Report body

`report.report` is rstest's per-phase report (the same shape as
[`--report-json`](report-json.md) test phases):

```json
{"nodeid": "pkg/test_x.py::test_a", "when": "call",
 "outcome": "passed", "duration": 0.01, "longrepr": null}
```

`when` is `setup` / `call` / `teardown`; `outcome` is `passed` / `failed` /
`skipped`; `longrepr` carries the traceback on failure. The report may also
carry the extra fields rstest attaches elsewhere (`wasxfail`, `skip_reason`,
`sections`, `cpu`, `thread_delta`, `fd_delta`, `lineno`); a client should read
the fields it needs and ignore the rest.

## Isolation contract

Each `run` executes in a `fork()` off the warm template. The child resets its
imported SUT/test modules to the post-collection framework baseline, so every
run sees pristine module state and the overlay's mutation — a mutant that kills
a test in one `run` cannot affect the next. A file imported *during* config or
collection (a `conftest.py`, or any module the root conftest pulls in) lives in
that baseline; when a `run` overlays such a file, its already-loaded module is
dropped too, so the child re-imports the mutation rather than reusing the stale
source.

## Not yet implemented

`cancel`, `stop_on_first_fail` early-bail at the socket boundary, concurrent
runs (`max_parallel`), multiple sessions per daemon, and a Windows fallback are
future work. `stop_on_first_fail` is accepted and passed through, but the run is
not cancelled from the client side mid-flight.

## Caveats

The per-run `fork()` is a bare fork with no `exec`, so it inherits the classic
fork-in-a-possibly-threaded-process hazard: if the collected suite's plugins or
conftests start a background thread (or hold an internal lock) at import time,
the forked child keeps only the forking thread and can deadlock the first time
it touches such a lock. Suites whose import side effects are single-threaded —
the common case for test code — are unaffected; a suite that spins up threads at
collection time should not be served until an `exec`-based worker lands.

A `run` must name a non-empty `node_ids`; an empty subset is rejected with
`bad_request` (an empty pytest argument list would otherwise collect and run the
*whole* suite). Malformed msgpack frames are answered with a `bad_frame` error
before the session ends, rather than being treated as a clean disconnect. The
daemon socket is created `0600` (owner-only), since a `run` overlay writes files
as the serving user.
