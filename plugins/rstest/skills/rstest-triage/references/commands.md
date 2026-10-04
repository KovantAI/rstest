# Triage commands: exact behavior and gotchas

Everything here is checked against rstest's CLI. The full reference lives in
the project docs (`docs/reference/cli-commands.md`, `docs/reference/cli.md`,
`docs/guides/flaky-tests.md`); this file is the subset that matters while
triaging.

## Contents
- replay
- bisect
- audit
- explain
- --shuffle
- Hangs: --timeout and --worker-timeout
- Flaky controls: --reruns, --reruns-only-known-flaky, --quarantine
- Manual discriminator runs

---

## replay

```console
$ rstest replay                                  # most recent local run
$ rstest replay <run-uid>                        # a specific journaled run
$ rstest replay --journal ./latest.json          # a downloaded CI journal
$ rstest replay --journal ci.json --python .venv/bin/python
```

- **What gets journaled.** Every parallel run (`-n >= 2`) writes
  `.rstest_cache/replay/<run-uid>.json` (last 10 kept) plus `latest.json`.
  Lazy-collection runs journal too. **Not journaled:** `-n 0`/`-n 1`,
  `--dist each`, `--shard`. For a sharded CI failure there is no journal:
  re-run that shard's tests unsharded with `-n` set to the CI worker count,
  which records one.
- **CI side.** Upload `.rstest_cache/replay/latest.json` as an artifact on
  failure (`if: failure()`, `if-no-files-found: ignore`). The journal keys on
  nodeid, so it survives the hop to another machine.
- **`--python` goes after the subcommand.** Before it, `replay` is read as a
  test path.
- **Session args come from the journal**, not the command line (paths, `-k`,
  `-m`, plugins), and are printed as `rstest: replay: args: ...`. They go to
  pytest as-is, so only replay journals from runs you trust.
- **What's pinned:** worker count, each worker's exact nodeids and order.
  Work-stealing and reruns are off. `--lf`/`--sw` recorded in the args are
  dropped with a warning; `--ff`/`--nf` stay.
- **Determinism is per worker.** Worker-local order is exact, which is what
  state-pollution flakes depend on. Cross-worker interleaving is still timing
  dependent, so a true race between workers reproduces only best-effort.
- **Drift.** If the suite changed since the journal, replay prints a drift
  note, runs what still matches, and reports how many recorded tests no longer
  collect.
- Replay writes no journal and does not touch the duration or flake caches,
  so repeating it while debugging is harmless.
- **Exit:** the replayed run's own code; a missing/unreadable journal exits
  `1` with an `Error:` line.

## bisect

```console
$ rstest bisect "tests/test_report.py::test_totals"
$ rstest bisect "tests/test_report.py::test_totals" -- -o log_level=DEBUG
$ rstest bisect "tests/test_report.py::test_totals" --bisect-json out.json
```

- Runs entirely at `-n 0`: it isolates **ordering**, not concurrency.
- Steps: (1) run the victim alone; if it fails, it's a plain bug and bisect
  stops. (2) Run it after all preceding tests in collection order. (3) If
  that passes, run it after every other test, including the ones that collect
  after it, so a polluter that collects later (but ran first on a CI worker)
  is still found. If that passes too, bisect says the failure "does not
  reproduce from test order" (likely concurrency; use `audit`). (4) ddmin
  over the reproducing set to a 1-minimal set; handles single polluters and
  interacting pairs.
- With a replay journal, `scripts/journal_bisect.py` searches only the tests
  that ran before the victim on its CI worker, in their recorded order: a
  smaller set, closer to what CI did.
- Prints the culprit(s) and a minimal repro command
  (`rstest -n 0 <culprit...> <victim>`), relative to the current directory.
- Bounded to about 80 child runs; at the ceiling it reports the smallest set
  found, which may not be fully minimal.
- Disables pytest-randomly, lifts `-x`/`--maxfail`, uses a private empty
  pytest cache (so `--lf`/`--ff` do nothing), and turns rstest's reruns off.
  To bisect a failure that appears only in one shuffled order, reorder
  explicitly instead.
- **Refused (exit 2):** `--nf` or `--sw` in the options; any test path or
  nodeid after `--`; test paths inside ini `addopts` or `PYTEST_ADDOPTS`
  (clear with `-- -o addopts="<options only>"`).
- **Exit:** `0` culprit found, `1` not order-dependent, `2` nodeid not in the
  suite, a selection after `--`, or an error.

## audit

```console
$ rstest audit
$ rstest audit --audit-repeat 5 --audit-json audit.json
```

- Runs the suite at `-n auto` (N times with `--audit-repeat`), diffs against
  the `-n 0` oracle, and classifies each parallel-only failure: ISOLATION /
  CO-LOCATION, WALL-CLOCK / LOAD-SENSITIVE, ORDER DEPENDENT, INTRINSIC FLAKE,
  or pre-existing.
- Emits a paste-ready `conftest.py` block marking the ISOLATION /
  CO-LOCATION and WALL-CLOCK / LOAD-SENSITIVE tests `@pytest.mark.serial`. If
  `conftest.py` already defines `pytest_collection_modifyitems`, paste only
  the `_RSTEST_SERIAL` set and add the loop to the existing hook; a second
  definition silently replaces the first.
- ORDER DEPENDENT tests are listed separately with a `--dist loadfile`
  recommendation, because serial would run them apart from the tests they
  depend on. Intrinsic flakes and inconclusive tests are also separate.
- A test flaky in every mode can still pass all serial runs by chance and be
  listed as a serial candidate. Raise `--audit-repeat` when in doubt.
- **Exit:** non-zero on any parallel-only failure (gates CI); pre-existing
  failures don't fail it; `2` for a refused dispatch or an audit error.

`audit` vs `migrate-check`: `migrate-check` is the onboarding preflight
(unstable ids first, verbose classification, bisects polluting files for the
first 3 victims). `audit` is the recurring fix loop for a suite already on
rstest.

## explain

```console
$ rstest explain "tests/test_api.py::test_login"
$ rstest explain "tests/test_api.py::test_login" --json
```

- Reads only `.rstest_cache/`, runs nothing, needs no interpreter.
- Shows last duration, flake/fail counts with last-event age, last-green
  outcome, and the coverage footprint (needs a prior `--cov-context=test` run).
- Caches keep only the latest duration, so there is no variance or
  last-N-outcomes history.
- Unknown nodeid: human mode exits `1` with substring suggestions; `--json`
  exits `0` with `"found": false`.
- On a fresh CI runner the caches are empty unless `.rstest_cache` is
  persisted; run `explain` where the history lives.

## --shuffle

```console
$ rstest --shuffle                   # random seed, printed
$ rstest --shuffle=1234 -n 2 --dist loadfile   # reproduce a failing order
```

- The seed must be attached with `=`: `--shuffle 42` is a bare `--shuffle`
  plus a test path `42`.
- Needs the parallel pool with full collection: `-n 0`/`-n 1`,
  `--collect lazy` and `--dist each` are refused. Auto collection never picks
  lazy under `--shuffle`, so the default is fine.
- In `load` mode the shuffle replaces duration ordering; affinity modes
  shuffle group order and keep in-group order.

## Hangs: --timeout, --worker-timeout, --stream-json

- **Without any of these, a hung run tells you nothing.** Dots only print
  when a test finishes, and when CI's job timeout kills rstest it writes no
  report and no replay journal (the journal is written at the end of a run),
  so there is nothing to replay.
- `--timeout SECS` fails a test whose **call** phase runs longer than SECS,
  interrupted in-process so the traceback points at the stuck line.
  `@pytest.mark.timeout(N)` overrides per test. It also arms a watchdog at
  3 × timeout + 10 s that kills the worker for code that never returns to
  Python **and for hangs in fixture setup or teardown**, which the in-process
  timer does not cover.
- **`--timeout` is command line (or marker) only.** It is not a
  `[tool.rstest]` key: `timeout = N` there is warned about as unknown and
  ignored. `addopts` is not read for rstest's own flags either.
- `--worker-timeout SECS` (off by default; also `[tool.rstest] worker-timeout`)
  kills a worker stuck on one test in any phase; the test is reported failed,
  and a replacement worker joins. It replaces the per-test watchdog limits, so
  set it above the longest `@pytest.mark.timeout`. No traceback, but the
  failure names the test.
- `--stream-json FILE` writes each test phase report as it happens, flushed
  line by line, so it survives the job being killed. Upload it with
  `if: always()`. A test with a `setup` report and no `teardown` report hung
  in its call or teardown. A test that hangs in fixture **setup** writes no
  record at all. The stream still shows which worker went quiet (its last
  event stops early); the `--worker-timeout` failure line or the
  `faulthandler_timeout` dump below names the test itself. pytest's `faulthandler_timeout = N` (ini) also
  dumps every thread's stack while the test is still stuck.
- Once a run with `--timeout` finishes, its journal includes the test that
  hung, so `replay` works as usual. `replay` does not accept `--timeout`; set
  `[tool.rstest] worker-timeout` so replays are bounded too.
- Put a CI step timeout (`timeout-minutes`) well under the job limit as the
  last resort.
- A fixed-port fixture that deadlocks across workers is the classic CI-only
  hang: fix with an ephemeral port or a per-worker name from `worker_id`.

## Flaky controls

- `--reruns N`: retries a failure up to N times; fail-then-pass is `flaky`
  (green run, counted, listed, flagged in JUnit and report-json).
  `@pytest.mark.flaky(reruns=3)` (or positional `flaky(3)`) sets a per-test
  budget, honored at every `-n`, `-n 0` included; `condition=False` turns it
  off. `--only-rerun REGEX` (repeatable) limits retries to failures whose
  error text matches.
- The marker's `only_rerun` and `reruns_delay` keywords are ignored: a
  `flaky(only_rerun=["ConnectionError"])` test still retries an
  `AssertionError`. Use the `--only-rerun` flag instead.
- `-x` / `--maxfail` count a test only once its reruns are used up, and never
  count a `--quarantine` match.
- Pass `--reruns` whenever the suite relies on `@pytest.mark.flaky`: with the
  marker alone, a parallel run retries and reports `1 flaky`, yet exits 1.
- History: every run (except `--dist each` and `replay`) merges into
  `.rstest_cache/flakes.json` (`flaky`, `failed`, `last_epoch`). Entries age
  out after 90 days without events (`RSTEST_FLAKE_RETENTION_DAYS`, `0` keeps
  forever). Local edit loops count as `failed`, so read `flaky` as the flake
  signal.
- `--reruns-only-known-flaky`: reruns only tests with `flaky > 0` history
  (or an explicit `@pytest.mark.flaky`). A hard-failure-only record doesn't
  qualify. It never learns new flakes; pair it with a learning run.
- `--quarantine FILE`: one nodeid or `*` glob per line, `#` comments on
  their own line only (a trailing `# ...` after a nodeid makes the entry
  silently never match). It is a CLI flag only, not a `[tool.rstest]` key. A
  matching **failure** becomes `quarantined` (traceback still printed, JUnit
  has no `<failure>`); a run whose only failures are quarantined exits `0`;
  failures outside the list still exit `1`.

## Manual discriminator runs

When a quick classification is enough, or `audit` is too slow for the suite:

```console
$ rstest -n 0 "path/to/test.py::test_x"   # passes? the test itself is fine
$ rstest --dist loadfile                  # passes? order dependency
$ rstest -n 2                             # passes? load sensitivity
```

Order dependencies want `loadfile` or a refactor; load sensitivity wants
`serial` or a clock mock; failing at `-n 0` too is a plain bug.
