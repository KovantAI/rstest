# Changelog

All notable changes to rstest. Pre-1.0: minor behavior changes may occur
between 0.0.x releases and are listed here.

## 0.2.0 — 2026-07-15

- `--doctor` PARALLEL EFFICIENCY section: the realized parallel speedup
  measured from the run just finished (`test time / wall` vs worker
  count), the per-worker busy-time load balance, and the long pole that
  caps it — the after-the-fact answer to "why isn't `-n auto` faster?".
  Emitted for multi-worker runs in the terminal report, `--doctor-md`,
  and `--doctor-json`; the doctor JSON schema is bumped `1` → `2`
  (adds `parallel_efficiency`).
- `--shard <K/N>`: split one suite across `N` independent CI jobs and run
  only shard `K` (1-based). Buckets are balanced by the duration cache
  (longest-processing-time-first bin-packing; even count split on a cold
  cache), disjoint, and cover the whole suite, so merging the per-job
  JUnit reconstructs the full run. Orthogonal to `-n`; shards at file
  granularity under `--collect lazy`; composes with `--changed`. Under an
  affinity `--dist` mode (`loadfile`/`loadscope`/`loadgroup`) it partitions
  at whole-group granularity, so a file/scope/xdist_group never splits
  across shards (the run-together / in-order contract those modes provide).
  Requires `-n >= 2`; refused with `--shuffle` and `--dist each`. See the
  Sharding guide.

- `--output azure`: Azure Pipelines style — the normal `dots` log plus a
  `##vso[task.logissue type=error;sourcepath=;linenumber=]` command per
  failure (inline issue on the PR file), and `type=warning` for
  flaky-passed tests.
- Flaky-passed tests (`--reruns`) now surface in every CI `--output`
  style, not just `github`: `azure` emits a `type=warning` logissue,
  `teamcity` a `WARNING`-status build message, `buildkite` a `warning`
  annotation on the build page (via `buildkite-agent`), and `gitlab`
  folds the flaky block into its own collapsed section (GitLab has no
  per-line warning command).

- Four new CI `--output` styles beyond `github`: `gitlab` (failures
  folded in collapsible job-log sections), `buildkite` (failures under
  auto-expanded `+++` groups), `teamcity` (service messages per test,
  grouped so parallel results never interleave), and `tap` (a pure TAP
  version 13 stream with a trailing plan — no human chrome). Like
  `--output json`, `tap` is refused at a monorepo root (concatenated
  child streams would not be one valid TAP document).

- `--durations-regress <RATIO>`: gate CI on per-test duration
  regressions vs the duration cache the scheduler already maintains.
  Tests grown past RATIO x baseline are listed and the run exits 1;
  jitter floors (50ms baseline, 0.5s absolute growth) keep CI noise
  from flagging. Cold cache skips the comparison.

- `--shuffle[=SEED]`: run tests in a seeded random order to flush out
  order dependencies on demand (pytest-randomly for the dispatch
  queue). The seed is printed for reproduction; affinity modes shuffle
  group order and keep in-group order intact. Single-worker mode,
  `--collect lazy`, and `--dist each` are refused rather than silently
  ignored.

- `--output github`: tests that passed only after reruns now emit a
  `::warning` annotation (`flaky: passed only after N reruns`) — the
  run stays green, but the flake shows up inline on the PR.

- Flake history + `--quarantine <file>`: every run records per-test
  flaky/failed counts to `.rstest_cache/flakes.json` (sparse — only
  tests with events). `--quarantine` demotes failures matching a list
  of nodeids/globs to a non-fatal "quarantined" outcome: own summary
  count and section (with history annotations), junit/report-json
  property (report-json schema 5 adds the flag and counts key), exit 0
  when every failure is quarantined. New failures outside the list
  still fail the run.
- `--doctor-md <path>`: the doctor analysis as GitHub-flavored markdown
  (job-summary tables). Under GitHub Actions any doctor run now appends
  this markdown to `$GITHUB_STEP_SUMMARY` automatically, so the report
  shows up on the run page without a post-processing step.
- `--changed` is PR-aware in CI: on a GitHub Actions pull_request job
  (`GITHUB_BASE_REF` set), bare `--changed` diffs against the merge-base
  with the PR base branch instead of `HEAD`, so a clean checkout of the
  PR commit selects exactly the PR's files. An unfetched base ref is an
  error (fetch-depth: 0), never a silent full skip; an explicit rev
  disables the auto-targeting.

## 0.1.0 — 2026-06-23

- Vendored pytest upgraded 9.0.3 → 9.1.1 (re-extracted verbatim from the
  PyPI wheel; no local modifications). rstest's runner hooks are unaffected;
  the full e2e gate passes.

## 0.0.5 — 2026-06-13

- Windows promoted from experimental to supported: the full test gate
  runs on `windows-latest` in CI every commit (not just a wheel smoke
  test) and passes. Large-suite corpus validation remains macOS/Linux.


- `--report-json` schema 3: the envelope now carries `counts`
  (pytest-accounting outcome totals — the same numbers as the terminal
  summary line, so consumers never re-derive them by walking `tests`),
  `duration_seconds`, `started_at_epoch`, `workers`, and `argv`. The
  monorepo merged report aggregates grand totals and adds per-project
  `counts` under `meta.projects`.

- Monorepo `--report-json` now writes ONE merged document: root-relative
  nodeid keys, merged `meta.exitstatus`, and per-project status (incl.
  `--changed` skips) under `meta.projects` — no more globbing slugged
  files and client-side merging. junit stays per-project (one testsuite
  file per package).

- `--changed-strict`: `--changed` hardened for gating CI. A changed
  source file unreachable from any test via the import graph forces a
  full run instead of a silent skip; in monorepos, undeclared
  cross-project imports are detected by scanning and counted as
  dependency edges (the shared-venv trap); "nothing affected" exits 5
  instead of 0. Implies `--changed` when given alone.

- `--report-json` schema 2: `meta.schema` version field, `longrepr`
  (failure text, capped 20k) on failed tests, and `crashed: true` on
  outcomes fabricated by the orchestrator (worker crash /
  `--worker-timeout` kill) — machine consumers no longer re-parse
  terminal output or mistake a crash for an assertion failure.

- xdist environment parity: workers now set `PYTEST_XDIST_WORKER` and
  `PYTEST_XDIST_WORKER_COUNT`, and `workerinput` carries `testrun_uid`
  (one uid per run, shared across workers; monorepo children inherit
  the root's) — plugins and conftests that grep the environment work
  without edits.

- Round-four documentation review fixes: report-json field table
  repaired and version history completed; the CI duration-cache recipe
  no longer freezes (actions/cache keys are immutable — unique key +
  restore-keys); watch-mode rerun policy corrected in two stale pages
  (source changes select via the import graph, not the full
  selection); crash-handling now distinguishes passive worker-id-keyed
  resources (reuse is the point) from hook-provisioned ones (use uuid
  idents); the SQLAlchemy at-scale claim names its backend and worker
  count; exit-code special cases (`--changed` nothing-affected,
  monorepo merging) documented; which xdist master hooks are CALLED vs
  silent no-ops enumerated; monorepo slug derivation, skipped-project
  file absence, small-runner oversubscription, and tox/nox guidance
  written down.

- Documentation hardening from the third persona review: master-side
  hook emulation scoped precisely (per-node-stateless contract, the
  N-concurrent-hooks divergence from xdist's serialized master, the
  crash-cleanup ordering hazard and the uuid-ident remedy), `-n 1`
  semantics vs xdist, `--dist each` scope, monorepo `--changed`
  false-skip warning (declared-metadata edges only — keep merge-queue
  gating on full runs), per-project coverage verified by the gate, and
  the worker-runtime vs tool-install mechanism spelled out.

- Monorepo mode: at a repo root with per-package pytest configs, rstest
  discovers the subprojects and runs each as its own session group in
  one command — own rootdir/ini/conftest semantics per project (cwd
  switched), merged exit codes, per-project `--junitxml`/`--report-json`
  files, and a summary table. Auto-engages when the cwd has no pytest
  config but subdirectories do; `[tool.rstest] projects = [globs]`
  pins the set; an explicit path argument opts out. Projects run
  CONCURRENTLY under one worker budget, split by each project's
  last-known suite time (duration caches; even split on first run),
  output printed whole per project in completion order. Validated
  against langchain-ai/langgraph: 8 libs auto-discovered, per-lib
  outcomes matched to the digit, and one command replaces six serial
  pytest invocations at 126s vs 853s (6.8x). `--changed` is
  monorepo-aware: directly-changed projects narrow via their own import
  graph, dependents (through pyproject dependency names incl.
  dependency-groups, transitively) run full, unaffected projects are
  skipped, and out-of-project changes run everything. A project-local
  `.venv` is used automatically. Per-project `[tool.rstest]` settings
  apply per project — a `numprocesses` pin survives the worker planner
  (`numprocesses = 0` = that project runs pytest-exact while siblings
  split the rest). (`git diff --relative` fix rides along: `--changed`
  from any repo subdirectory now sees its own files.)

- `--collect lazy` (D5 single-point collection): each test file is
  collected exactly once, on one worker, on demand — instead of every
  worker collecting the whole suite. One distributed collection pass;
  the collection-mismatch failure class cannot occur by construction.
  3x faster narrow `-k` selections on big suites (aiohttp). Strict
  file affinity by default; an explicit `--dist load` enables
  work-stealing for suites with giant files. Session fixtures persist
  across per-file collection; module fixtures tear down exactly at
  file boundaries. Suites that depend on whole-suite import side
  effects (sys.modules-reading skipifs, cross-file registries,
  run-order pollution) should stay on `--collect full` — every
  divergence found in the corpus reproduces under plain pytest with
  the same isolation or ordering. `[tool.rstest] collect` configures
  it per project.

- `--dist loadscope` and `--dist loadgroup` (with
  `@pytest.mark.xdist_group`) — xdist's remaining affinity modes.

- `--dist each` (xdist's last mode): every worker runs the full suite
  (multi-environment validation). Outcomes are keyed `nodeid [gwN]`,
  counts are per-worker totals, a crash replacement runs only the dead
  worker's remaining items (xdist semantics), and the duration cache is
  left untouched. `--reruns` is rejected in this mode.

- xdist MASTER-side hooks emulated in workers: `pytest_configure_node`,
  `pytest_testnodeready`, `pytest_testnodedown`. Suites whose conftest
  fills `node.workerinput` from the controller now run in parallel —
  SQLAlchemy's follower-database provisioning (`follower_ident`) was
  the canonical blocker: its suite now runs at `-n 4` in 76s vs 519s
  under sequential pytest, outcome-identical. The configure_node call
  fires synchronously at plugin registration (sqlalchemy registers its
  hooks mid-configure and reads the result on the next line — a plain
  trylast hook call misses that window). pytest-cov's and xdist's own
  master hooks are excluded: rstest already emulates those handshakes
  directly. Crash cleanup included: workers ship a
  `workerinput` snapshot after configure, and when one crashes the
  orchestrator hands it to a surviving worker, whose
  `pytest_testnodedown` then runs with the dead worker's idents
  (best-effort, like xdist's own master).

- Live status footer (terminal only): per-worker current test with elapsed
  time, plus overall progress and ETA, rendered below the streaming dots.
  Piped/CI output is unchanged.
- Worker attribution: `-v` lines and failure headers carry `[gwN]`
  (xdist's convention); `--report-json` gains a per-test `worker` field.
  Single-worker runs stay unprefixed.

- `--durations=N` / `--durations-min=X`: pytest's slowest-durations
  block, rendered by the orchestrator (it was silently swallowed before
  — worker terminals are captured). Merged across workers, pytest's
  phase granularity, hidden-note wording, and `-vv` behavior.

- `--doctest-modules` verified working in pool and single-worker modes
  (vendored core collects, items dispatch normally, failures render);
  now covered by the gate.

- Rust unit tests (`cargo test`, wired into CI alongside the gate):
  dispatch scheduling for every `--dist` mode, exit-code merging,
  wire-protocol kind strings and Python-shaped event decoding, summary
  accounting, junit rendering, `[tool.rstest]`/ini parsing, argv
  splitting, and import-graph scanning for `--changed`.

- Fixed: parallel runs now abort on collection errors like pytest does
  (exit 2, no tests run). The guard lives inside pytest's default
  `pytest_runtestloop`, which rstest's item dispatch replaces, so pool
  mode previously ran the collectable remainder of the suite past the
  errors. `--continue-on-collection-errors` is honored.

- Better collection-mismatch refusal: when workers collect the same
  number of tests but different IDs (pytest-randomly, time-derived
  parametrize IDs), the error now names the common causes and the
  workarounds (`-p no:randomly`, stable IDs, or `-n 0`), and workers
  exit quietly instead of printing broken-pipe tracebacks.

- Fixed: tests that spawn subprocesses via `multiprocessing` spawn mode
  or `anyio.to_process` now work under workers. Both re-import the
  parent's `__main__` file without package context; the worker entry
  point used a relative import (ImportError in the child), ran `main()`
  unguarded, and re-prepended the vendored-pytest path (making child
  `sys.path` differ from the parent's). Found via anyio's own suite —
  28 tests, including `test_identical_sys_path`.

- Public-suite corpus: `corpus/run.py` reproduces parity + timing runs
  against 31 well-known pytest suites (pandas, fastapi, aiohttp, …)
  with SHA-pinned checkouts and a strict network-then-offline phase
  split. Baseline pytest is pinned to the vendored version.

## 0.0.4 — 2026-06-11

- `@pytest.mark.flaky(reruns=N)` per-test rerun budgets and
  `--only-rerun REGEX` (pytest-rerunfailures semantics); the plugin is
  neutralized inside workers to prevent double reruns.

- `[tool.rstest]` in pyproject.toml: project-level defaults for
  `numprocesses`, `dist`, `reruns`, `worker-timeout` (CLI wins).
- Rerun reliability: workers now stay connected after draining, so
  failures in a worker's final batch (including single-test runs) are
  retried like any other — previously tail-of-queue failures could not
  rerun. Sessions close via an explicit end-of-run signal.

- Release workflow: tag-triggered wheel builds for linux/macos/windows,
  signed with GitHub artifact attestations (Sigstore provenance,
  `gh attestation verify`), staged as a draft GitHub release with
  SHA256SUMS.

- `--worker-timeout SECS`: hang backstop — kills and replaces a worker
  stuck on one test, reporting that test failed (off by default).

- Experimental Windows support: anonymous-pipe worker transport
  (CreatePipe + inheritable handles), `Scripts/python.exe` venv discovery,
  platform-correct `PYTHONPATH` joining. CI builds and smoke-tests a
  Windows wheel; the full compatibility battery has not yet run on
  Windows.
- JUnit XML: flaky tests (passed after `--reruns`) carry a
  `<property name="flaky" value="true"/>`.

## 0.0.3 — 2026-06-11

- Fixed: `-n auto`'s suite-size heuristic undercounted suites using the
  `*_test.py` naming convention (pytest's default matches both `test_*.py`
  and `*_test.py`), capping parallelism to one worker.
- Added: gate checks for both test-file naming conventions.

## 0.0.2 — 2026-06-11

- Parallel by default: `-n auto`, suite-aware (capped by test-file count
  and cached suite duration); header line announces the worker count.
- Parallel wind-down (worker teardowns overlap; removes the post-summary
  pause on small suites).
- `@pytest.mark.serial` (exclusive post-parallel phase) and
  `--dist loadfile`.
- Crash recovery: exact attribution, fail-don't-retry, requeue, same-`gwN`
  respawn with a capped restart budget.
- `--watch` with import-graph-targeted reruns.
- `--doctor` and `--doctor-json` (schema 1).
- pytest-cov support under parallelism (combine + reports +
  `--cov-fail-under`).
- Global `-x`/`--maxfail`, merged `--lf`/`--ff`, orchestrator-rendered
  `--junitxml`, warnings summary, captured-output sections, ANSI colors,
  `-v` mode.
- Nested pytest-xdist neutralized inside workers.

## 0.0.1 — 2026-06-10

- Initial wheel: Rust orchestrator + vendored pytest 9.0.3 core
  (`rstest_worker._vendor`), item-level dispatch across workers,
  duration-aware scheduling, live progress, pytest-style summaries.
- Verified: 100% per-test outcome parity vs pytest baselines on pandas
  (193,627 tests), aiohttp, django-allauth, and rich, with real plugins.
