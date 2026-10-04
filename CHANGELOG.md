# Changelog

All notable changes to rstest. Pre-1.0: minor behavior changes may occur
between 0.x releases and are listed here.

## Unreleased

### Behavior changes

- **Behavior change: a project `.venv` without rstest stops the run.**
  rstest used to skip it silently and run the workers under another
  interpreter on `PATH`, so every test failed on the project's own imports.
  It now exits with an error naming the venv and the install command
  (`uv pip install --python .venv/bin/python rstest`). Pass `--python` to use
  another interpreter on purpose. A stale `.python-version` no longer rejects
  the project's own `.venv`, and a pin that can't be met names its file.
- **Behavior change: `.rstest_cache` lives at the pytest rootdir.** It used to
  be created in whatever directory you ran from, so a run from `tests/unit/`
  kept its own durations, flake history, replay journals and `explain` data.
  It now sits next to `.pytest_cache`, resolved the way pytest finds its
  rootdir. Both caches get a `.gitignore` and `CACHEDIR.TAG`, so a run leaves
  the git tree clean. `--incremental` and the coverage index key their paths
  by rootdir too.
- **Behavior change: `--output VALUE` with a value that is not an rstest
  style goes to pytest.** pytest-playwright's `--output <dir>` used to be
  swallowed (with a fallback to `dots`), at every `-n`. A typo now fails with
  pytest's usage error plus a hint naming rstest's styles.
- **Behavior change: `-p no:cacheprovider` writes no cache.** rstest no longer
  creates `.rstest_cache` or `.pytest_cache` when pytest's cache plugin is
  disabled; the run behaves like a cold one. `--collect lazy` with
  `-p no:cacheprovider` no longer crashes.
- **Behavior change: terminal output follows pytest's color rules.**
  `--color`, `PY_COLORS`, `NO_COLOR`, `FORCE_COLOR` and `TERM=dumb` are
  honored in that order. Without color (or under `CI`) there is no live
  footer and no `bar` view, so `NO_COLOR` and `--color=no` now mean zero
  escape codes. `FORCE_COLOR` colors piped output. The live footer fits the
  terminal width.
- **Behavior change: `rstest try` refuses to compare nothing.** A collection
  error or zero tests on either side now prints "could not compare" and exits
  2 instead of "drop-in ready". The speed line names the worker count
  (`at -n auto = -n 4`).
- **Behavior change: a session-scoped fixture's resources are never charged
  to a test by the leak check.** See the leak check entry below.

### Fixes

- **unittest `subTest` and `subtests` failures count as failures.** In
  parallel runs a failing subtest was reported passed, and report-json said
  `failed: 0` even at `-n 0`, so a CI gate reading it went green. Counts now
  match pytest, in the summary, report-json, JUnit and `rstest try`.
- **A user `--basetemp` is split per worker.** Every worker shared it and wiped
  it at startup, so `tmp_path` tests failed at random. Each worker now uses
  `<basetemp>/gwN`, as with pytest-xdist.
- **`--changed` and `--watch` select tests through `conftest.py`.** A changed
  module imported by a conftest now selects every test under that conftest;
  before, those tests were skipped and a broken test passed unnoticed.
- **`--cov-fail-under` matches pytest-cov.** It is checked exactly once in
  every report mode (it was skipped with `--cov-report=` or `annotate` in
  parallel runs), honors `.coveragerc` `fail_under` and precision, and uses
  pytest-cov's rounding. `--cov-config`, `show_missing` and `skip-covered`
  are honored, and `-n 0` no longer prints the table twice.
- **`-x` and `--maxfail` with reruns, `@flaky` or a quarantine.** A test
  waiting for its rerun could stop a worker and drop the rest of its queue,
  so a run with a failing test passed, or hung. A quarantined failure counted
  toward `-x` and the run exited 0. The orchestrator now owns maxfail for
  retryable and quarantined failures, and a run that maxfail stopped never
  exits 0.
- **`-x` stops at once.** Workers drop their queued tests on the stop, and the
  run prints `stopping after N failures`.
- **`@pytest.mark.flaky` reruns at `-n 0`.** The mark was ignored in
  single-worker mode. A positional reruns count and `condition=` are now read
  as pytest-rerunfailures does.
- **`@pytest.mark.flaky` alone no longer fails a green run.** A marked test
  that recovered on retry without a global `--reruns` printed
  `1 flaky, ... passed` but exited 1. It now exits 0, like `--reruns`.
- **A flaky test is counted once.** It used to count as both `flaky` and
  `passed` (two tests read `1 flaky, 2 passed`). The summary and the
  report-json `counts` now put it in `flaky` only, so the counts add up to
  the tests run.
- **`--durations-regress` keeps firing until the test is fast again.** Each run
  used to overwrite the baseline with its own times, so the gate fired once
  and a re-run passed; a failing run also stored its near-zero time. Failed
  and regressed tests no longer update `durations.json` or the shared remote
  cache.
- **Doctor numbers include fixture time.** Efficiency, worker load, the long
  pole, slowest files and WAIT-BOUND now count setup and teardown, so a
  fixture-heavy suite no longer reads as 7% efficient. CPU spent in child
  processes counts as computing, not waiting. `-n 0` reports 1 worker.
- **`--doctor-fail-on` gates what it measures.** `wait_seconds`, `wait_pct`
  and `long_pole_seconds` are evaluated whenever measured, at any worker
  count; they were skipped below the report's display threshold. A skipped
  condition is reported as skipped, not passed, and NaN or infinite
  thresholds are rejected.
- **The leak check blames the test that leaked.** It tracks thread and file
  descriptor identities instead of net counts, so one test's leak is no
  longer cancelled by another test's cleanup. Resources a wider-scoped
  fixture creates (and releases at scope end) are not charged to any test.
- **SIGTERM/SIGINT stop a parallel run cleanly.** rstest used to die on the
  spot: no summary, no replay journal, no reports, and its workers kept
  running, reparented to init. Now it stops and reaps every worker, names the
  test each was running and records it failed ("interrupted by SIGTERM after
  12.3s"), writes the journal and any `--junitxml`/`--report-json`, and exits
  2. A second signal exits at once. A CI job killed by its timeout now leaves
  a `rstest replay` journal and the name of the hung test.
- **Ctrl-C keeps the history honest.** Tests running at the interrupt are no
  longer recorded as failures in `lastfailed` or the flake history or given
  a zero duration, and the summary says how many tests did not run. At `-n 0`
  rstest no longer dies on Ctrl-C: it exits 2 and writes its reports, and it
  passes SIGTERM on to the test session.
- **`lastfailed` is merged like pytest's.** A passing subset run no longer
  wipes earlier failures.
- **`breakpoint()` in a parallel run fails with a hint.** It used to end in a
  bare `BdbQuit` and could lose the next test on that worker. It now fails
  the test with "rerun with -n 0 (or -s)" and every test is still reported.
- **Startup errors print once.** A bad nodeid, missing path, unknown flag or
  broken conftest printed its error once per worker. The usage line now says
  `rstest`, not `pytest.main()`.
- **The parallel summary reads like pytest's.** `1 error` and `1 warning`
  (not `1 errors`), collection errors counted once and as errors, an
  `N deselected` count, and pytest's warnings footer.
- **Tracebacks keep their indentation and `--tb` works.** The first source
  line of a parallel failure kept losing its indent, and `--tb=no` /
  `--tb=line` printed full blocks.
- **Collection errors reach CI.** `--output tap` emits a failed point with the
  traceback instead of a passing `1..0`, and `github`, `azure` and `teamcity`
  annotate each broken module. Azure issues show the exception line, and
  GitHub `file=` paths are relative to the repository, so annotations land on
  the PR diff under `working-directory`.
- **An unwritable `GITHUB_STEP_SUMMARY` is a warning.** It used to fail a green
  run and skip `--report-json`.
- **`--shard` spreads zero-duration tests.** A warm cache with many 0.0s
  timings put almost every test in one shard. Shard assignment is also the
  same on every machine for near ties in lazy mode.
- **The GitHub Action checks its inputs.** `shard` without `shard-total` (or
  the reverse) fails the step instead of running the whole suite, and
  `rerun-on` / `doctor-fail-on` values reach rstest unchanged (quotes and
  backslashes were mangled).
- **`-n auto` sizes the pool by what you selected.** One selected test runs in
  single-worker mode, and a one-file suite of slow tests runs in parallel once
  its timings are cached.
- **The pool spreads long tests and groups.** Startup gave each early worker
  two items, so cached long tests and `xdist_group` / `loadfile` groups
  stacked on one worker.
- **`bisect` finds a polluter that collects after the victim.** When the
  preceding tests don't reproduce the failure, bisect now also runs the victim
  after every other test before concluding it's not order-dependent. Before,
  a polluter later in collection order (one that ran first on another
  worker's CI schedule) was reported as a likely concurrency bug.
- **`migrate-check` and `audit` really run in parallel.** Their parallel pass
  could resolve to one worker and report "ready". `migrate-check` also flags
  collections whose order changes between runs (for example parametrizing
  over a `set`).
- **Trailing `# comments` work in `--quarantine` files.** A line such as
  `tests/test_q.py::test_broken  # JIRA-1` was read as one pattern, comment
  included, and silently matched nothing. A `#` after whitespace now starts a
  comment; a `#` inside a nodeid (`test_x[#1]`) is still part of it.
- **Lazy collection finds what pytest finds.** It honors `norecursedirs`,
  expands glob `testpaths`, and ignores `.ignore` files.
- **`@serial` tests share the designated worker's session.** Session fixtures
  were set up again for every serial test.
- **Smaller fixes.**
  - `rstest -q try` runs `try`; a subcommand name in a later position gets a
    clear error.
  - `testrun_uid` is a 32-character uuid hex, as with pytest-xdist.
  - `xdist-removal-check` gives the right advice for `import xdist.plugin`.
  - A wrong interpreter under pre-commit gets a hint naming the project's
    `.venv` when imports fail.
  - The doctor's PARALLEL FLOOR no longer fires on a perfectly balanced pool.
- **Docs.** The Azure shared-cache recipe now fails the step when tests fail.
  The `--shuffle` docs point to `rstest replay` for an exact reproduction.

## 0.8.0 (2026-09-28)

- **`rstest xdist-removal-check`: a readiness check for uninstalling
  pytest-xdist.** It scans the pytest config and `PYTEST_ADDOPTS` for xdist
  flags that become a usage error once the package is gone (`--tx`,
  `--rsyncdir`, `-d`, `--maxprocesses`, `--looponfail`, `-p xdist`, ...) and
  for `-n` / `--dist`, which rstest never read from `addopts` (so a
  `--dist loadgroup` there already lost its grouping); `pytest-xdist` in
  `required_plugins`; unguarded `import xdist` sites in the project and in
  installed plugins; xdist hook implementations not marked
  `optionalhook=True`; and `hasplugin("xdist")` gates in the project and in
  installed plugins. Each
  finding prints its fix. Exits non-zero on any blocking finding;
  `--migrate-allow` accepts known ones and `--xdist-removal-json` writes a
  versioned document for CI. `--xdist-trial` also runs the suite with xdist
  hidden (`-p no:xdist`) and names the tests that only pass with it.
- **`pip install rstest` works on Python 3.10 without pytest.** The vendored
  pytest core imports `exceptiongroup` and `tomli` below Python 3.11 (and
  `colorama` on Windows), but rstest didn't declare them, so a fresh 3.10
  environment without pytest failed to start any worker. They are now
  declared with pytest's own markers, and CI checks a clean install.
- **A worker that dies says why.** Its failure now carries the exit code (or
  the signal) and the last lines it wrote to stderr, so the report, junit and
  report-json name the actual error (for example a `ModuleNotFoundError` at
  startup). A worker that dies before sending anything reads "exited during
  startup" instead of msgpack's "failed to fill whole buffer". Worker stderr is
  still shown live; outside `-s`/`--pdb` it now reaches the terminal through
  rstest, so it is no longer a TTY for the worker.
- **`--junit-xml` is intercepted like `--junitxml`.** pytest's alias used to
  reach every worker session, which then all wrote the same file.
- **Worker messages are size-capped.** One worker-to-orchestrator message is
  capped at 256 MiB, so a crashed or wedged worker can't drive an unbounded
  read. Raise it with `RSTEST_MAX_MESSAGE_BYTES` for the rare suite that
  genuinely exceeds it.
- **JSON Schemas for `audit`, `bisect` and `explain`.** `--audit-json`,
  `--bisect-json` and `explain --json` are now built from typed structs and
  get generated field references and full draft-07 schemas on the
  [Output schemas](docs/reference/output-schemas.md) page, checked by the same
  golden test as the others. The emitted JSON is unchanged. All schemas now
  describe what rstest writes: a field that is omitted when empty is optional
  and never `null`, and a field always written is required.
- **`--collect` now defaults to auto.** With neither the `--collect` flag nor
  `[tool.rstest] collect` set, rstest picks `lazy` collection for a big-enough
  parallel run: at least 2000 known tests (from `.rstest_cache/durations.json`)
  and a `tests × workers` product of at least 16 000, on a file-affine dist
  (`--dist load`/`loadfile`). Otherwise it picks `full`. This drops the
  `(workers − 1)` redundant full collections large parallel runs paid before,
  without touching small suites (which keep full collection's locality). A cold
  cache counts as zero tests, so the first run of a suite stays `full`. Auto
  also stays `full` for path or `--changed` selections, when doctests are
  enabled, when the cache has tests from files the lazy walk can't see, or
  when one file would hold up the run, and under `--shard`, `--shuffle`,
  `--incremental` or a fail-fast `--order` (so `--watch` stays `full`). Without
  an explicit `--dist load`, lazy runs each file whole on one worker, and it
  has no cross-worker collection comparison. Pin `--collect full` for xdist's
  exact collection model. Lazy workers (auto or explicit) now
  honor `norecursedirs`, `collect_ignore` and `--ignore` for the files they are
  handed, as eager recursion does. A banner
  reports the choice when auto picks `lazy`. Force either with `--collect full`
  / `--collect lazy`. See
  [Lazy collection](docs/concepts/lazy-collection.md#auto-default).
- **`--fork-pool` prewarms the worker pool (Unix).** A zygote imports the
  vendored pytest core once and forks the initial workers off it, so the core
  import is no longer paid once per worker. Off by default; a no-op on
  Windows, single-worker runs and `-s`/`--pdb`/`--co`. Crash-respawned workers
  use the normal spawn path. `--doctor` now prints a `startup:` line with the
  pool spawn time and suggests `--fork-pool` when startup is a real share of a
  short run; doctor JSON gains `startup_seconds` and `fork_prewarm`. At
  a monorepo root the flag is forwarded to every project. See
  [`--fork-pool`](docs/reference/cli.md#-fork-pool).
- **`--incremental` no longer caches a test on a stale pass when coverage
  measures only part of the project.** First-party files that coverage never
  measures used to be invisible to `--incremental`, so editing one left
  dependent tests cached. Those files are now folded into the skip fingerprint:
  editing any of them re-runs the suite (`--cov=.` with no `include`/`omit`
  keeps per-file granularity). The measured set is read the way pytest-cov and
  coverage.py build it: `--cov` values from the command line, the ini `addopts`,
  and `PYTEST_ADDOPTS` (space form, dotted module names, `src/` layouts, and
  absolute paths included); a bare `--cov` narrowed by `[run] source` /
  `source_pkgs` / `source_dirs`; and `[run] include` / `omit`, from
  `.coveragerc`, `setup.cfg`, `tox.ini`, `pyproject.toml`, or `--cov-config`,
  in coverage.py's lookup order. The files checked come
  from one directory walk that skips dot-folders, virtualenvs, and caches, so
  gitignored first-party code (e.g. generated `*_pb2.py`), submodules, and
  nested repos count too; inside git, unchanged tracked files reuse git's own
  hashes instead of being re-read, and symlinked files and package folders are
  followed and hashed by their target's content. A run without `--cov` reuses
  the scope of the coverage run that wrote the index it relies on. `omit` /
  `include` patterns support `[...]` character classes, either path separator,
  and Windows drive paths, and match case-insensitively on Windows. Test-named files are tracked
  separately: a changed test module re-runs only its own tests, while a changed test-named
  helper with no tests of its own (`test_utils.py`, a `TestMixin` base)
  re-runs the suite. A `--cov` scope matching nothing in the project gets a
  warning. Existing `--incremental` baselines reset once after upgrading.
- **`--since-green` detects in-place dependency upgrades.** The environment
  fingerprint now includes every installed distribution in the venv's
  site-packages (`*.dist-info`, legacy `*.egg-info` / `*.egg-link`), so a
  `pip install -U` that leaves the lockfile alone still resets the baseline.
  The project's own editable or in-project install is keyed by name only, so a
  git-derived version bump (setuptools-scm, hatch-vcs) does not force a full
  run. Existing baselines reset once after upgrading rstest.
- **`rstest replay`: re-run a recorded parallel schedule.** Every parallel run
  (`-n >= 2`, except `--dist each` and `--shard`) journals
  which worker ran which tests, in what order, to
  `.rstest_cache/replay/latest.json` (opt out with `RSTEST_NO_REPLAY_JOURNAL=1`).
  `rstest replay --journal <file>` pins that schedule back, so an
  order-dependent failure seen on CI reproduces locally. The journal is keyed by
  nodeid and stores paths relative to the project, so it survives the machine
  hop. Replay turns off reruns (`--reruns`, `[tool.rstest] reruns` and
  `@pytest.mark.flaky`), keeps `@pytest.mark.serial` tests exclusive, respawns a
  crashed worker with only its remaining tests, and ignores recorded
  `--lf`/`--sw`. See *Replaying a CI-only failure locally* in the CI quickstart.
  The bundled GitHub Action now leaves `.rstest_cache/replay` out of the cache
  it persists; the changed path spec makes the first run after upgrading miss
  the cache once.
- **Report files create their parent directory.** `--junitxml`,
  `--report-json`, `--html`, `--stream-json`, `--doctor-json`/`--doctor-md`,
  `--cov-diff-json`, the merged monorepo report and the
  `migrate-check`/`audit`/`bisect` JSON outputs now create a missing
  parent directory, as pytest does for `--junitxml`. Before, a path such as
  `test-results/junit.xml` on a fresh checkout ran the whole suite and then
  exited 1 with `No such file or directory`. A write that still fails names
  the path.
- **Verdict subcommands exit 2 on an error.** `try`, `migrate-check`, `audit`
  and `bisect` use exit `1` for "found something". An error inside them (no
  usable interpreter, a failed spawn) now exits `2` with an `Error:` line
  instead of `1`, and `migrate-check` exits `2` when its parallel pass
  produced no outcomes to judge. When such an error happens before the run
  (for example no usable interpreter), `--bisect-json` still records it in
  `error`, `--audit-json` says `ran: false`, and a stale `--migrate-check-json`
  is removed, so a CI gate never reads an earlier run's result.
- **`rstest try --python` reaches both runs.** The `rstest -n auto` half of
  `try` now uses the same interpreter as the pytest baseline; before, it
  re-ran discovery and, outside an activated venv, could find none.
- **The remote cache token is kept out of test processes' environment.**
  `RSTEST_CACHE_REMOTE_TOKEN` is removed from the environment of workers and
  of the pytest baseline `rstest try` runs. This is defense in depth: a test
  running as the same user can still read the rstest process's environment,
  so jobs that run untrusted code need a read-only token.
- **pytest-randomly works with pytest-xdist installed.** With both plugins
  installed, every `-n >= 2` run failed with an internal error, `TypeError:
  can only concatenate str (not "int") to str`: randomly's xdist hook copied its
  unresolved `"default"` seed over the one rstest broadcasts. A
  `pytest_configure_node` hook called while its plugin registers may now add
  `workerinput` keys but not overwrite them; it is called again at session
  start, as under a real xdist controller.
- **The hang watchdog is sized per test.** The watchdog rstest arms from a
  timeout used one limit for the whole run, 3 × `--timeout` + 10 s, so a
  test with a longer `@pytest.mark.timeout` was killed at the global limit
  (`timeout(300)` under `--timeout 30` died at 100 s). Each test's watchdog
  is now 3 × its own timeout + 10 s: its marker, else `--timeout`. A marker
  also arms the watchdog without `--timeout`, which gives Windows (no
  in-process interrupt) a backstop for marked tests. An explicit
  `--worker-timeout` still sets one limit for every test. Watchdog kills now
  say which limit fired.
- **GitHub action: `cache-push` input.** Decides which runs write the cache
  other jobs trust. With the default `auto`, the `remote` backend pushes only
  on `push` / `schedule` / `workflow_dispatch` runs of `warm-from-branch` and
  on `merge_group`, and the `actions-cache` backend no longer saves on
  `pull_request_target`, `issue_comment` or `workflow_run`. `true` always
  writes, `false` never does. The action also hands `cache-remote-token`
  only to the rstest command instead of its whole run step.
- **Byte-exact mode now prints pytest's own terminal output.** At `-n 0` /
  `-n 1` (and `-n auto` capped to one worker), with no `--output` set, the
  pytest session writes to stdout directly instead of rstest re-rendering it:
  the session header, `ERRORS` / `FAILURES` sections, warnings summary,
  `short test summary info`, `-r`, `--durations`, `-x`, and any plugin's
  `pytest_report_header` / `pytest_terminal_summary` lines now match plain
  pytest byte for byte. rstest prints no banner or summary of its own there,
  only its additions after pytest's (quarantined failures, doctor, coverage,
  gate messages); `--junitxml`, `--report-json`, `--html` and `--stream-json`
  are still written. Pass `--output dots|verbose|bar|...` to keep rstest's
  renderer. `-n ≥ 2` is unchanged.
- **`--junitxml` writes pytest's own document.** Each worker now runs
  pytest's junitxml plugin and streams every finished `<testcase>` element,
  and rstest merges them in collection order, so the file matches plain
  pytest's (apart from `time` / `timestamp` / `hostname`) at every `-n`:
  suite `pytest` in `<testsuites name="pytest tests">`, pytest's run order,
  real `message` attributes, `type="pytest.skip"` / `pytest.xfail`, and
  `junit_family`, `junit_logging`, `junit_suite_name`, `--junit-prefix`,
  `record_property`, `record_xml_attribute` and `record_testsuite_property`
  honored. Previously rstest wrote its own document (suite `rstest`, tests
  sorted by nodeid, `message="failed"`), and the `record_*` fixtures were
  lost. rstest's additions are standard `<property>` extensions: `flaky` on
  a test that passed after reruns, and `quarantined` on a quarantined
  failure, whose `<failure>` is removed so junit-gated CI agrees with the
  exit status. In byte-exact mode, pytest's closing summary is now followed
  by `rstest: N failures above are quarantined and do not fail the run`.
- **An empty folder runs one worker and says `no tests ran`.** `rstest` with
  nothing to collect started a full pool (one worker per core) and printed an
  empty summary line (` in 0.15s`). `-n auto` now drops to one worker when
  the collection walk finds no Python file at all and no argument names a
  path, and an empty run's summary reads `no tests ran`, like pytest.
- **`-x` / `--maxfail` from ini `addopts` now stop the whole run.** Only a
  command-line `-x` was coordinated across workers; one in `addopts` or
  `PYTEST_ADDOPTS` stopped each worker's own session, so the other workers
  kept going. Workers now report pytest's resolved limit and the
  orchestrator applies it globally, in both the eager and lazy pools.
- **Combined short flags are recognized.** `-sv` and `-vs` now force the
  single-process passthrough like `-s`, `-xv` is a global fail-fast like
  `-x`, `-sv` / `-v -v` count toward verbose output, and the two-token
  `--capture no` is treated like `--capture=no`. Previously only the exact
  tokens matched.
- **`--changed` and `--watch` no longer drop option values that are paths.**
  When replacing your positional paths with their selection they dropped
  every argument naming an existing path, including option values: with an
  `api/` directory, `-k api` lost `api` and broke the command line, and
  `--ignore tests/slow` lost its value. Lazy collection similarly collected
  an `--ignore`d directory as a test path. Option values are now kept; a
  plugin option not in rstest's table still works as `--option=value`.
- **Benchmarks re-measured under one methodology, plus CPU-bound results.**
  Every published number is now the median of 5 runs after a warm-up, with
  rstest and pytest-xdist at the same `-n`, on a documented machine. Numbers
  that moved: pandas `-n 8` is now 43s for rstest against 89s for xdist (the
  old single run had them at parity, 63s vs 61s; xdist's controller is the
  bottleneck on 193k tests), django-allauth at matched `-n 8` is 5.8s against
  8.9s (the old row compared xdist `-n 8` with rstest `-n 4`), and aiohttp's
  cold run is 150s at `-n 8` (was 126s). New: sympy and scikit-learn sweeps
  (parity with xdist, gains up to the performance-core count), a measured
  per-worker memory model, and a worker x BLAS-thread grid, which replaces
  the "one thread per worker is usually faster" advice. Tooling:
  `examples/cpu-bench`, and `corpus/bench.py --sweep-workers`, `--xdist`,
  `--memory`, `--grid`, `--cold`.
- **GitHub action: the fail-ratio gate no longer masks non-test failures.**
  With `fail-under-ratio` set, the gate used to judge only the JUnit ratio, so
  an interrupt (exit 2), a lost worker (3), a pytest usage error (4), or an
  empty collection (5) could pass green. It now fails the job on any rstest
  exit other than 0/1, and on exit 1 with no failing test (a gating flag such
  as `doctor-fail-on` fired, or rstest refused the run). An exit-0 run that
  wrote no JUnit (`changed` found nothing affected) passes instead of failing
  with "JUnit not found".
- **GitHub action: inputs can no longer inject shell code.** Every input now
  reaches the action's scripts through `env:` rather than being pasted into
  them. `args` is split with shell quoting rules (`-k "a and b"` stays one
  argument) and is never glob-expanded or evaluated.
- **GitHub action: "Re-run failed jobs" no longer fails at artifact upload.**
  Artifact names are unique per run across attempts, so a re-run hit a name
  conflict uploading its cache segment and JUnit. Segment names now carry the
  attempt (`rstest-seg-<suffix>--<run_id>-<attempt>-<shard>`; the warm pattern
  is unchanged and merges every attempt), and the JUnit upload overwrites the
  failed attempt's report under its stable name.
- **GitHub action: matrix legs no longer collide or mix caches.** Artifact
  names now carry a per-leg suffix (new `artifact-suffix` input; default
  `<os>-py<version>[-<working-directory>]`): segments are
  `rstest-seg-<suffix>--<run_id>-<attempt>-<shard>` and JUnit artifacts
  `rstest-junit-<suffix>[-shard-K]`, and the warm step pulls only its own
  leg's segments. **Upgrade note:** the first artifact-backend run after
  upgrading starts cold, and workflows that download JUnit by exact name need
  the new names (`pattern: rstest-junit-*` covers every leg).
- **GitHub action: only trusted runs seed the artifact cache.** The warm
  lookup is filtered to `warm-from-event` (new input, default `push`), so a
  pull_request run, including a fork PR from a branch named `main`, can never
  become the warm source.
- **GitHub action: warns when the rstest version is unpinned.**
- **rstest warns when `-s`/`--pdb`/`--capture=no` silently drop `-n`.** Those
  flags (and `--trace`, stepwise, `--debug`) run the session in one process
  with pytest's own output, so `rstest -n 4 -s` quietly ran unparallelized
  with no banner. An explicit worker count above 1 (flag or
  `[tool.rstest]`) now gets a one-line stderr warning naming the flag. The
  default `-n auto` and `--co` stay quiet.
- **Workers no longer inherit rstest's internal variables.** `RSTEST_WORKER_ID`,
  `RSTEST_DOCTOR`, `RSTEST_TIMEOUT` and the other orchestrator-to-worker
  variables (plus `PYTEST_XDIST_WORKER[_COUNT]` outside a pool) are now
  cleared before each worker starts, so a value exported in the shell, in CI,
  or by an outer rstest (a test that runs rstest itself) can no longer reach
  an `-n 0` test or switch on worker instrumentation. rstest itself no longer
  reads `RSTEST_DOCTOR` either: `export RSTEST_DOCTOR=1` has no effect (use
  `--doctor`), and `migrate-check`'s child runs request their instrumentation
  through an internal flag instead.
- **Parallel workers no longer inherit a stale `PYTEST_XDIST_WORKER`.** A
  `PYTEST_XDIST_WORKER` / `PYTEST_XDIST_WORKER_COUNT` already exported in the
  caller's environment used to win over each worker's real values, so every
  worker saw the same id and per-worker resources (test databases, ports,
  temp dirs keyed on it) collided. Workers now always set their own values.
- **`testrun_uid` works with pytest-xdist installed.** xdist's own
  `testrun_uid` fixture won over rstest's and read `workerinput["testrunuid"]`,
  a key rstest did not set, so any test using the fixture failed with
  `KeyError: 'testrunuid'` at `-n 2` or more. `workerinput` now carries both
  `testrunuid` (xdist's key) and `testrun_uid`, and `PYTEST_XDIST_TESTRUNUID`
  is set alongside `PYTEST_XDIST_WORKER`.
- **rstest's `worker_id` / `testrun_uid` fixtures take precedence over
  xdist's.** With pytest-xdist installed, the `--reruns` one-worker pool at
  `-n 0/1` reported `worker_id == "gw0"` (xdist's definition) instead of the
  documented `"master"`. rstest's definitions now win (a conftest or test
  module override still wins over both); in a pool of two or more workers the
  values are unchanged.
- **Monorepo roots forward more flags, and refuse the rest.** `--timeout`,
  `--fail-on-leak`, `--durations-regress`, `--require-baseline`,
  `--reruns-only-known-flaky`, `--collect`, `--incremental`, `--shuffle` and
  `--html` used to be dropped silently at a monorepo root; they now reach
  every project. Gates apply per project and a failing project fails the root.
  `--html` is written per project (`out.html` -> `out.libs-core.html`), and
  `--shuffle` picks one seed for every project (printed, so `--shuffle=SEED`
  reproduces the whole run). `--debug`, `--shard`, `--cov-diff-fail-under`,
  `--cov-diff-json`, `--stream-json` and `--since-green` (without `--changed`)
  now exit 1 with a message saying to run them inside a project. See
  [Flags at a monorepo root](docs/concepts/monorepo.md#flags-at-a-monorepo-root).
- **`RSTEST_CACHE` is namespaced per monorepo project.** Every project's
  rstest inherited the root's value as-is, so an absolute `RSTEST_CACHE` made
  all projects share one cache dir (durations and flake history mixed), a
  relative one resolved inside each project, and the root planner still read
  `<project>/.rstest_cache`. Each project now gets `<RSTEST_CACHE>/<slug>` (a
  relative value resolves against the monorepo root), and the planner weights
  projects from the same dirs. Unset, projects keep their own `.rstest_cache`
  as before.
- **`[tool.rstest]` typos are reported.** An unknown key or a wrong-typed value
  was silently ignored; it now prints one stderr warning naming the file and
  key (with a `did you mean` hint for a snake_case spelling such as
  `worker_timeout`) and falls back to the default, as before. Only types are
  checked at this point; values are validated when the run starts, unchanged.
- **pytest 9 config files are recognized.** rstest's own config lookup (used
  for `-n auto` sizing, `--collect lazy`, `--watch`, monorepo discovery and
  `bisect`) now probes `pytest.toml`, `.pytest.toml`, `pytest.ini`,
  `.pytest.ini`, `pyproject.toml`, `tox.ini` and `setup.cfg` in pytest 9's
  order, and reads pyproject's native `[tool.pytest]` table. A `pytest.ini`
  without a `[pytest]` section now counts as the config file, as in pytest.
- The "no surviving worker to run pytest_testnodedown" warning no longer
  carries a run of stray spaces mid-sentence.
- **Single-worker mode prints pytest's `[ NN%]` progress column.** `-n 0`/`-n 1`
  output used to drop it (`..F` instead of `..F [100%]`, and `-v` lines with
  no percentage) because the session never reported its collected count. The
  `-s`/`--pdb` passthrough path is unchanged: pytest itself hides the column
  when capture is off.
- **Monorepo merged `--report-json` now stamps the current schema.** The merged
  root document hard-coded `"schema": 4` while carrying schema-5 fields
  (`quarantined`); it now shares the single-project writer's version constant.

- **rstest warns about plugins pinned to an older pytest.** When a loaded
  plugin's own metadata excludes the pytest rstest runs (for example it declares
  `pytest<9`), rstest prints one `rstest: warning: ...` line to stderr per run.
  Such a pin only ever constrained pip: `import pytest` inside the plugin still
  gets the vendored pytest 9.1.1, so the pin was silently inert. Requirements
  behind an extra are ignored, and the run itself is unaffected.
- **Autogenerated output schemas in the docs.** The stable JSON outputs now
  publish a machine-readable [JSON Schema](https://json-schema.org/) (draft-07)
  plus a field-reference table, generated directly from the Rust types
  (`schemars`) and embedded into the docs so they can never drift from what the
  CLI emits. Covers every stable JSON surface: `--report-json`, `--doctor-json`,
  `--collect-only --report-json` (discovery), `migrate-check --migrate-check-json`,
  and the `flakes.json` flake log; see the new *Output schemas* reference page.
  The three previously hand-built (`serde_json::json!`) outputs are now emitted
  from typed structs with unchanged bytes. A golden test enforces schema
  freshness (`RSTEST_BLESS_SCHEMAS=1 cargo test -p rstest-cli schema`).
- **`--doctor` coverage-waste section: slow tests that add no unique coverage.**
  When the doctor run itself collects per-test coverage
  (`--doctor --cov --cov-context=test`), `--doctor` now flags slow passing tests
  whose every executed product line is also executed by another kept passing
  test, so they can all be deleted or merged together without dropping a
  covered line (duplicates are picked slowest first). An index from an earlier
  run or a cache is never used. Reports the reclaimable time, the count, and per-test detail
  (lines covered, distinct other tests sharing them). Emitted in the terminal
  report, the markdown report, and `--doctor-json` as `coverage_waste` (doctor
  JSON `schema` bumped to `3`). Silent without this run's coverage index.
- **`--changed` now reports coverage-map health (test impact analysis).**
  `--changed` has been coverage-aware since 0.4.0 (a warm
  `.rstest_cache/coverage_index.json` maps changed lines to the exact covering
  tests); it now makes that observable. With a warm map the selection banner
  shows the savings ratio (`N changed file(s) -> M of K mapped test(s)
  affected`), with any whole-file fallback targets counted separately. With a **cold** map and a changed non-test source file (exactly
  where coverage precision would have narrowed the set), it prints a one-line
  hint that it fell back to the import graph and that a prior
  `--cov --cov-context=test` run enables coverage-precise selection. No new
  flags; the hint is silent for test-only / config / non-Python changes.
- **`--watch` reselects incrementally and quits on `q`.** The import graph
  behind affected-test selection is kept warm for the session: each save
  re-checks every file's mtime and size and re-reads only the changed ones, and
  adding or deleting a `.py` file re-resolves every
  import against the new file set while reusing each untouched file's parse.
  Typing `q` then Enter now ends the session cleanly with exit 0 (`Ctrl+C` still
  works); closing stdin does not, and in modes that hand stdin to the test
  process (`--pdb`, `-s`, `--debug`, ...) only `Ctrl+C` exits. A watch started
  in the background (`rstest --watch &`) never reads the terminal, so the shell
  does not suspend it.
- **The duration cache self-heals when tests change.** `durations.json` now
  tags each entry with its test file's path and a sha256 of its contents. On
  load, an entry whose source file changed (edited body) or vanished
  (deleted/renamed) is dropped, so an edited test re-times on fresh numbers
  rather than scheduling on stale ones, and gone tests stop accumulating in the
  file forever. The hash is content-based and ignores line endings, so a
  restored cache still matches after a fresh clone or CI checkout. The path is
  recorded from the rootdir pytest reports on the run that timed the test, so
  runs with different rootdirs share one cache, and a test edited mid-run
  re-times next run. Entries a run cannot locate (such as `-n 0` runs of tests
  never timed in parallel) are kept untagged, as before. `--durations-regress`
  still compares an edited file's tests against their previous timings. `wall.json`,
  a whole-suite aggregate with no per-test source to fingerprint, instead ages
  out on `RSTEST_WALL_TTL_DAYS` (default 30, `0` disables). Both formats are
  read back-compatibly, so an upgrade keeps existing caches. (Issue #18.)
- **Fixture scope-promotion advisor in `--doctor`.** Doctor already flags hot
  function-scoped fixtures; it now *checks* the case for promoting them. Under
  `--doctor` each function-scoped fixture's produced value is fingerprinted on
  every call, and a fixture that returned the same value every time (in every
  worker) is reported as a `scope="session"` candidate with a projected saving:
  the largest per-worker `(calls − 1) × mean setup time`, the redundant
  re-setups removed, as wall time. New terminal "SCOPE-PROMOTION CANDIDATES"
  section and markdown table; the `--doctor-json` document (now `schema: 3`) carries `constant` and
  `projected_saving_seconds` per fixture. Conservative: only immutable
  builtin values (`str`, numbers, `bytes`, and tuples/frozensets of them)
  qualify, and fixtures with per-test teardown (`yield`,
  `request.addfinalizer`), narrower-scoped dependencies, parametrize
  arguments, or failed/skipped setups are never flagged. See
  [doctor guide](docs/guides/doctor.md#scope-promotion-candidates).
- **`rstest audit`: one-command parallel-safety check.** Runs the suite at
  `-n auto` (repeat with `--audit-repeat` to catch probabilistic races), diffs
  against the `-n 0` oracle, and classifies every test that fails *only* in
  parallel (reusing `migrate-check`'s `-n 0` ×2 + `--dist loadfile`
  discriminators and verdicts). Prints the serial-fixable (isolation and
  wall-clock) failures with a ready-to-paste `conftest.py` block that marks
  exactly those nodeids `@pytest.mark.serial` (one paste, no per-test edits),
  plus the real per-verdict fix. Order-dependent failures get a
  `--dist loadfile` recommendation instead, since serial would separate them
  from the tests they depend on. Intrinsic flakes, inconclusive results and
  pre-existing `-n 0` failures are called out separately. Exits non-zero on any parallel-only failure (CI-gateable);
  `--audit-json` writes the findings, the serial set, and the conftest block.
  See [`audit`](docs/reference/cli-commands.md#audit).
- **Fail-fast dispatch ordering (`--order fail-fast`).** A new
  `--order <throughput|fail-fast>` flag chooses how `--dist load` sequences the
  ready queue. `throughput` (default) keeps the slow-tests-first packing that
  optimizes wall-clock. `fail-fast` orders for the earliest red signal:
  recently-failed tests first, then the flakiest (both from
  `.rstest_cache/flakes.json`), then the usual throughput order for clean
  tests, so a broken run paired with `--maxfail`/`-x` dies in seconds. Both
  input signals were already cached; no new data collection. Auto-selected under
  `--watch`; also settable as `[tool.rstest] order`. See
  [`--order`](docs/reference/cli.md#-order-throughputfail-fast).
- **`rstest bisect <nodeid>`: order-dependency polluter finder.** For a test
  that fails only when run after some other test, bisect delta-debugs the
  predecessor set at `-n 0` (`ddmin`) down to the minimal set of earlier tests
  that reproduce the failure (the polluters) and prints a shell-quoted minimal
  reproducing command (`rstest -n 0 <culprit…> <victim>`). Serial by
  construction, so it isolates ordering (not concurrency); handles a single
  polluter and interacting pairs, bounded to ~80 child runs. Uses pytest's
  own rootdir and collects the whole suite even from a subdirectory, keeps
  child runs on the collection's interpreter, rootdir and config file,
  disables pytest-randomly and uses a private cleared cache so the victim
  always runs last (`--ff`/`--lf`/`-x` in `addopts` included; `--nf`/`--sw`
  are refused by name), leaves your `.pytest_cache` untouched, scales past the
  OS argv limit, and forwards pytest options given after `--`. Exits `0` when a culprit is found, `1` when the test isn't
  order-dependent (fails alone / no reproduction from order), `2` for an
  unknown nodeid. `--bisect-json` writes the result. See
  [`bisect`](docs/reference/cli-commands.md#bisect-nodeid).
- **`rstest shard-verify`: prove a `--shard` matrix covered the whole suite.**
  Sharding partitions the suite independently in each job, so a divergent
  duration cache or a differently collected suite could silently drop or
  double-run tests and still exit 0. A `--shard` run's `--report-json` now
  carries a `meta.shard` stamp (`k`, `n`, and the sha256 `collection_hash` and
  size of the full collection), and `rstest shard-verify shard.*.json`
  reconciles the per-shard reports in a final job. Exits `0` only when the
  shards agree on one collection, the shard set is exactly `1..=N`, and every
  collected test ran on exactly one shard; exits `1` on any drop, overlap,
  missing or duplicate shard, or divergent collection. Reads only the JSON
  files, so it needs no interpreter. Full-collection runs only (`--collect
  lazy` shards stamp no hash). See
  [`shard-verify`](docs/reference/cli-commands.md#shard-verify).
- **`rstest explain <nodeid>`: one test's dossier from the caches.** Merges
  the duration cache, flake/fail log, last-green outcome set, and coverage
  index for a single nodeid (last duration, flake history, last outcome, files
  and lines covered) without running anything or needing an interpreter.
  `--json` prints a schema-stamped object for editors and CI steps. See
  [`explain`](docs/reference/cli-commands.md#explain).
- **`migrate-check` child runs honor `--python`.** Its serial, loadfile and
  polluter discriminator runs re-resolved an interpreter from the environment
  and could land on a different one than the collection used; they are now
  pinned to the same interpreter. Its polluter search also disables
  pytest-randomly, which could shuffle the candidates after the victim.
- **`migrate-check` discriminator runs ignore rstest `reruns`.** A configured
  `[tool.rstest] reruns` routed its serial runs through the one-worker pool
  (duration-ordered) and a passing rerun could hide the failure being
  classified; the child runs now pin `--reruns 0`, and pass their pytest args
  after `--` so no rstest flag among them changes how the child runs.
- **Id-bearing collection works with `-p no:cacheprovider`.** The worker read
  `config.cache` unguarded, so with the cacheprovider disabled the collection
  report was never sent.
- **A pytest `@argsfile` counts as an explicit selection.** At a monorepo root
  with no pytest config, `rstest @tests.txt` fanned out over every subproject
  instead of running the listed tests as one project.
- **Heads-up when a parallel run pairs with a "dark" report plugin.** At
  `-n ≥ 2`, invoking a flag whose plugin aggregates on the (absent) xdist master
  (`--json-report`, `--report-log`, `--ctrf`, `--nunit-xml`, `--md`, `--csv`,
  `--benchmark*`) now prints a warning before the run naming the plugin and the
  parallel-safe alternative (native `--report-json` / `--junitxml`, or `-n 0`).
  Argv-driven, so it never flags rstest's own merged `--html` / `--junitxml` /
  `--report-json`, and stays silent at `-n 0`.
- **Plugin compatibility matrix extended to the top 100.** The
  [compatibility reference](docs/reference/top-100-plugins.md) now classifies
  the 100 most-downloaded pytest plugins (was 50) for behavior under the
  parallel pool.
- **pytest-mypy no longer crashes under the pool.** Its worker branch reads
  `workerinput["mypy_config_stash_serialized"]` (the mypy results-cache path an
  xdist controller injects), so merely installing it raised
  `KeyError: 'mypy_config_stash_serialized'` at `-n ≥ 2` (rstest runs no
  controller). rstest now seeds a unique per-worker cache path; mypy runs lazily
  on each worker (`MypyResults.from_session`), so type errors surface identically
  at `-n auto` and `-n 0`.
- **pytest-random-order no longer crashes under the pool.** Its
  `pytest_configure` reads `workerinput["random_order_seed"]` unconditionally
  whenever `workerinput` exists (even with reordering disabled, the default),
  so merely installing the plugin raised `KeyError: 'random_order_seed'` at
  `-n ≥ 2`. rstest now seeds that key like it does `randomly_seed`: a single
  run-derived value shared by every worker (so the shuffled collection hashes
  agree), keeping the plugin's `default:` prefix so order is untouched unless
  you opt in with `--random-order[-bucket|-seed]`; an explicit
  `--random-order-seed=<n>` is honored.

## 0.7.0 (2026-09-10)

- **Live progress while testing.** Runs now report ongoing progress as
  tests complete, so long suites give continuous feedback instead of going
  silent until the end.
- **Debugger support (`--debug[=PORT]`).** Run under
  [debugpy](https://github.com/microsoft/debugpy) for editor debugging
  (VS Code and any DAP client). Like `--pdb`, this forces the debugger:
  rstest starts debugpy in the worker and blocks until a client attaches.
  Bare `--debug` listens on `127.0.0.1:5678`; `--debug=PORT` overrides the
  port. The target interpreter (`--python`) must have `debugpy` installed;
  without it the run proceeds without a debugger and prints a hint.

## 0.6.1 (2026-09-08)

- The worker record file (`rstest-pytest-record.json`, or the path in
  `RSTEST_RECORD`) is now written atomically: the recorder writes to a
  `.tmp` sibling and `os.replace`s it into place, so a concurrent reader
  never observes a truncated or partially written JSON document.

## 0.6.0 (2026-09-07)

- **BREAKING: run-less modes are now subcommands, not flags.** The four
  modes that never run your suite are invoked as a leading subcommand:
  - `rstest --verify-vendor` → `rstest verify-vendor`
  - `rstest --try` → `rstest try`
  - `rstest --migrate-check` → `rstest migrate-check`
  - `rstest --cache-compact` → `rstest cache-compact`

  Their paired options are unchanged and now follow the subcommand token:
  `rstest migrate-check --migrate-check-json out.json --migrate-allow SUBSTR`
  and `rstest cache-compact --cache-remote URL`. The subcommand must be the
  first argument (`rstest verify-vendor --python 3.12`); a path literally named
  after a subcommand is disambiguated with `rstest ./try` or `rstest -- try`.

  The old flags no longer exist. Passing `--try` / `--migrate-check` /
  `--verify-vendor` / `--cache-compact` now forwards them to the pytest session
  (pytest then rejects the unknown argument), so **update CI scripts, aliases,
  and Makefiles**. Also note `rstest migrate-check` is now required to run the
  preflight: a bare `--migrate-check-json` no longer triggers it implicitly.

- pytest-retry now works under the pool without pytest-xdist installed. The
  plugin gates its report server on `has_plugin("xdist")` and only reads
  `workerinput["server_port"]` on the worker side; rstest is not xdist but does
  set `workerinput`, so each worker fell through to the client branch and
  `KeyError`'d on a port no master had provisioned (aborting collection at
  `-n > 1`). Each worker now stands up pytest-retry's own report server and
  seeds that port, so retries and the `flaky` marker work unmodified at any
  worker count. (When pytest-xdist *is* installed, the plugin self-provisions
  as before and rstest stays out of the way.)

- Monorepo worker planning now weights each project by its recorded
  whole-suite **wall time** (fixture setup/teardown included), not by the sum
  of test *call* durations. A fixture-bound project (one whose per-test call
  time is near zero but whose fixtures cost tens of seconds) was rated
  near-free on the warm run and starved to a single worker, so it serialized
  and dominated the monorepo wall (a warm run could run *slower* than the
  cold, cache-less run). Projects that pin their own `numprocesses` are
  unaffected; caches predating this release fall back to call-duration
  weighting until their first run under 0.6.0.

## 0.5.0 (2026-09-06)

- Incremental testing based on coverage: `--changed` now leans on the
  recorded coverage index to select only the tests whose coverage touches
  changed lines, falling back to the import graph on a cold cache.
- Dispatch-level result caching: `--incremental` collects the whole suite,
  then skips tests that were green last run and whose covered source files
  (and own test file) are all byte-identical now, injecting them as cached
  passes. Opt-in; a config or conftest change disables skipping for that run.
- Native timeout: per-test timeouts enforced by rstest directly, without
  `pytest-timeout`, and honored under parallelism.
- Native HTML report: an HTML report rendered by the orchestrator that
  works with any worker count (`pytest-html` only supports `-n 1`).
- Leak detection: fail a test that leaves open file descriptors behind.
- Cache: refactored to fix corruption and consistency issues.

## 0.4.0 (2026-08-31)

- Coverage overhaul: full pytest-cov parity under parallelism:
  `--cov=PKG` measured in every worker, all `--cov-report` targets
  (`term`/`term-missing`, `xml`, `html`, `json`, `lcov`, `annotate`)
  rendered by the orchestrator after combining, and `--cov-fail-under=N`
  enforced post-merge. `--cov-context=test` records which test covered
  each line, preserved through the parallel merge, and also writes a
  line→test index to `.rstest_cache/coverage_index.json`. `--changed`
  becomes coverage-aware: with a warm index it maps changed lines to only
  the tests whose recorded coverage touches them; a cold cache or
  unmeasured/brand-new code falls back to the import graph. `--cov-branch`
  forwards through. See the Coverage guide.
- `--reruns-only-known-flaky`: with `--reruns` active, spend the rerun
  budget only on tests the flake history already knows: new failures
  fail fast instead of being retried, while genuine known-flakes are
  still rescued. Also settable as `[tool.rstest] reruns-only-known-flaky`.
  See the Flaky tests guide.
- GitHub Action: run rstest in CI without a manual install/setup step.

## 0.3.1 (2026-08-30)

- Version bump only; no changes since 0.3.0.

## 0.3.0 (2026-08-30)

- Cache invalidation support.
- `--doctor` threshold option.
- Fixed compatibility with `pytest-rerunfailures`; allow reruns below 2
  workers.
- Mimic `pytest-randomly` `workerinput` handling and fix xdist hookspec
  handling.
- Fixed not persisting negative cache keys.

## 0.2.1 (2026-07-15)

- Release CI fixes only; no user-facing behavior changes. Corrected the
  build cache handling in the release workflow and dropped a step
  unsupported on the free tier.

## 0.2.0 (2026-07-15)

- `--doctor` PARALLEL EFFICIENCY section: the realized parallel speedup
  measured from the run just finished (`test time / wall` vs worker
  count), the per-worker busy-time load balance, and the long pole that
  caps it: the after-the-fact answer to "why isn't `-n auto` faster?".
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

- Flake history now **ages out**. A test with no flake or failure inside
  the retention window (default 90 days) is dropped from
  `.rstest_cache/flakes.json` on the next run, so a fixed test stops
  carrying "flaked _N_x before" annotations and the ranked candidate list
  stays current. Tune with `RSTEST_FLAKE_RETENTION_DAYS`; `0` keeps history
  forever. See the Flaky tests guide.

- Interpreter-probe cache now keys on file size **and** mtime, not mtime
  alone. A same-second in-place rewrite or an mtime-preserving restore
  (`cp -p`, `touch -r`, tar/rsync `--times`, reinstalling the same version)
  no longer serves a stale probe for a swapped binary. Old cache files load
  unchanged and re-probe on first use.

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

- `--output azure`: Azure Pipelines style: the normal `dots` log plus a
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
  version 13 stream with a trailing plan, no human chrome). Like
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
  `::warning` annotation (`flaky: passed only after N reruns`): the
  run stays green, but the flake shows up inline on the PR.

- Flake history + `--quarantine <file>`: every run records per-test
  flaky/failed counts to `.rstest_cache/flakes.json` (sparse: only
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

## 0.1.0 (2026-06-23)

- Vendored pytest upgraded 9.0.3 → 9.1.1 (re-extracted verbatim from the
  PyPI wheel; no local modifications). rstest's runner hooks are unaffected;
  the full e2e gate passes.

## 0.0.5 (2026-06-13)

- Windows promoted from experimental to supported: the full test gate
  runs on `windows-latest` in CI every commit (not just a wheel smoke
  test) and passes. Large-suite corpus validation remains macOS/Linux.


- `--report-json` schema 3: the envelope now carries `counts`
  (pytest-accounting outcome totals, the same numbers as the terminal
  summary line, so consumers never re-derive them by walking `tests`),
  `duration_seconds`, `started_at_epoch`, `workers`, and `argv`. The
  monorepo merged report aggregates grand totals and adds per-project
  `counts` under `meta.projects`.

- Monorepo `--report-json` now writes ONE merged document: root-relative
  nodeid keys, merged `meta.exitstatus`, and per-project status (incl.
  `--changed` skips) under `meta.projects`: no more globbing slugged
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
  `--worker-timeout` kill), so machine consumers no longer re-parse
  terminal output or mistake a crash for an assertion failure.

- xdist environment parity: workers now set `PYTEST_XDIST_WORKER` and
  `PYTEST_XDIST_WORKER_COUNT`, and `workerinput` carries `testrun_uid`
  (one uid per run, shared across workers; monorepo children inherit
  the root's), so plugins and conftests that grep the environment work
  without edits.

- Round-four documentation review fixes: report-json field table
  repaired and version history completed; the CI duration-cache recipe
  no longer freezes (actions/cache keys are immutable: unique key +
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
  false-skip warning (declared-metadata edges only; keep merge-queue
  gating on full runs), per-project coverage verified by the gate, and
  the worker-runtime vs tool-install mechanism spelled out.

- Monorepo mode: at a repo root with per-package pytest configs, rstest
  discovers the subprojects and runs each as its own session group in
  one command: own rootdir/ini/conftest semantics per project (cwd
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
  apply per project: a `numprocesses` pin survives the worker planner
  (`numprocesses = 0` = that project runs pytest-exact while siblings
  split the rest). (`git diff --relative` fix rides along: `--changed`
  from any repo subdirectory now sees its own files.)

- `--collect lazy` (D5 single-point collection): each test file is
  collected exactly once, on one worker, on demand, instead of every
  worker collecting the whole suite. One distributed collection pass;
  the collection-mismatch failure class cannot occur by construction.
  3x faster narrow `-k` selections on big suites (aiohttp). Strict
  file affinity by default; an explicit `--dist load` enables
  work-stealing for suites with giant files. Session fixtures persist
  across per-file collection; module fixtures tear down exactly at
  file boundaries. Suites that depend on whole-suite import side
  effects (sys.modules-reading skipifs, cross-file registries,
  run-order pollution) should stay on `--collect full`: every
  divergence found in the corpus reproduces under plain pytest with
  the same isolation or ordering. `[tool.rstest] collect` configures
  it per project.

- `--dist loadscope` and `--dist loadgroup` (with
  `@pytest.mark.xdist_group`): xdist's remaining affinity modes.

- `--dist each` (xdist's last mode): every worker runs the full suite
  (multi-environment validation). Outcomes are keyed `nodeid [gwN]`,
  counts are per-worker totals, a crash replacement runs only the dead
  worker's remaining items (xdist semantics), and the duration cache is
  left untouched. `--reruns` is rejected in this mode.

- xdist MASTER-side hooks emulated in workers: `pytest_configure_node`,
  `pytest_testnodeready`, `pytest_testnodedown`. Suites whose conftest
  fills `node.workerinput` from the controller now run in parallel:
  SQLAlchemy's follower-database provisioning (`follower_ident`) was
  the canonical blocker: its suite now runs at `-n 4` in 76s vs 519s
  under sequential pytest, outcome-identical. The configure_node call
  fires synchronously at plugin registration (sqlalchemy registers its
  hooks mid-configure and reads the result on the next line; a plain
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
  block, rendered by the orchestrator (it was silently swallowed before,
  since worker terminals are captured). Merged across workers, pytest's
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
  `sys.path` differ from the parent's). Found via anyio's own suite:
  28 tests, including `test_identical_sys_path`.

- Public-suite corpus: `corpus/run.py` reproduces parity + timing runs
  against 31 well-known pytest suites (pandas, fastapi, aiohttp, …)
  with SHA-pinned checkouts and a strict network-then-offline phase
  split. Baseline pytest is pinned to the vendored version.

## 0.0.4 (2026-06-11)

- `@pytest.mark.flaky(reruns=N)` per-test rerun budgets and
  `--only-rerun REGEX` (pytest-rerunfailures semantics); the plugin is
  neutralized inside workers to prevent double reruns.

- `[tool.rstest]` in pyproject.toml: project-level defaults for
  `numprocesses`, `dist`, `reruns`, `worker-timeout` (CLI wins).
- Rerun reliability: workers now stay connected after draining, so
  failures in a worker's final batch (including single-test runs) are
  retried like any other; previously tail-of-queue failures could not
  rerun. Sessions close via an explicit end-of-run signal.

- Release workflow: tag-triggered wheel builds for linux/macos/windows,
  signed with GitHub artifact attestations (Sigstore provenance,
  `gh attestation verify`), staged as a draft GitHub release with
  SHA256SUMS.

- `--worker-timeout SECS`: hang backstop that kills and replaces a worker
  stuck on one test, reporting that test failed (off by default).

- Experimental Windows support: anonymous-pipe worker transport
  (CreatePipe + inheritable handles), `Scripts/python.exe` venv discovery,
  platform-correct `PYTHONPATH` joining. CI builds and smoke-tests a
  Windows wheel; the full compatibility battery has not yet run on
  Windows.
- JUnit XML: flaky tests (passed after `--reruns`) carry a
  `<property name="flaky" value="true"/>`.

## 0.0.3 (2026-06-11)

- Fixed: `-n auto`'s suite-size heuristic undercounted suites using the
  `*_test.py` naming convention (pytest's default matches both `test_*.py`
  and `*_test.py`), capping parallelism to one worker.
- Added: gate checks for both test-file naming conventions.

## 0.0.2 (2026-06-11)

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

## 0.0.1 (2026-06-10)

- Initial wheel: Rust orchestrator + vendored pytest 9.0.3 core
  (`rstest_worker._vendor`), item-level dispatch across workers,
  duration-aware scheduling, live progress, pytest-style summaries.
- Verified: 100% per-test outcome parity vs pytest baselines on pandas
  (193,627 tests), aiohttp, django-allauth, and rich, with real plugins.
