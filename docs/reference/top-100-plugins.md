# Top 100 pytest plugins: rstest compatibility

The 100 most-downloaded pytest plugins (30-day PyPI counts via
[pythontest.com/top-pytest-plugins](https://pythontest.com/top-pytest-plugins/),
Aug–Sep 2026), each classified for how it behaves under rstest's parallel pool.
Ranks 1–50 are fully runtime-verified (every row `V`); ranks 51–100 are
classified by category, with the code-touched and genuinely-risky ones
(pytest-mypy, the report aggregators) and the plugins a corpus suite loads in
parallel also runtime-`V`.

**Verdicts**

| Mark | Meaning |
|---|---|
| ✅ Works | Parallel-safe as-is; no change needed. |
| 🟦 Native | Works, but rstest ships a built-in that supersedes it (prefer the native flag). |
| ⚠️ Caveat | Works with a stated limitation, usually a subset that needs `-n 0`. |
| 🔶 `-n 0` | Run single-worker for this plugin's feature (terminal painting, benchmarks, ordering). |
| 🔴 Silent | Produces **nothing** at `-n ≥ 2` (report aggregators gated on the xdist master); use `-n 0` or a native equivalent. |
| ➖ N/A | Unaffected by parallelism (assertion / fixture / format helpers). |

**Verified** column: **V** = exercised by an e2e gate, rstest's
[tested-compat table](../guides/plugins.md#tested-compatibility), or loaded by a
corpus suite (see the runtime inventory in
[Plugins exercised by the corpus](corpus-plugins.md)); **i** = inferred from the
plugin's category, not yet runtime-verified.

| # | Plugin | ~Downloads | Verdict | V | Note |
|---|---|---|---|---|---|
| 1 | pytest-asyncio | 275.9M | ✅ Works | V | Per-test event loop; vetted (rstest provides the worker context it sniffs). |
| 2 | pytest-json-ctrf | 273.0M | 🔴 Silent / 🟦 | V | Report aggregator; no file at `-n ≥ 2`, written at `-n 0` (e2e gate). Use native `--report-json` under the pool. |
| 3 | pytest-cov | 235.9M | 🟦 Native | V | rstest orchestrates coverage combine across workers; native `--cov`. Vetted. |
| 4 | pytest-xdist | 177.1M | ➖ N/A | V | Neutralized inside workers: rstest *is* the parallel runner; its options parse but stay inert. |
| 5 | pytest-mock | 105.0M | ✅ Works | V | Per-test `mocker` fixture; vetted. |
| 6 | pytest-timeout | 103.1M | 🟦 Native | V | Its ini `timeout =` setting fires in both modes. rstest has a native `--timeout` and honors `@pytest.mark.timeout` itself at every worker count; a command-line `--timeout` is rstest's and never reaches the plugin. To avoid two SIGALRM timers, uninstall the plugin or pass `-p no:timeout`. See [Plugins](../guides/plugins.md#tested-compatibility). |
| 7 | pytest-rerunfailures | 79.1M | 🟦 Native | V | Unregistered in the pool (its xdist `sock_port` branch would KeyError); rstest owns reruns (`--reruns`, `@mark.flaky`). |
| 8 | hypothesis | 48.3M | ✅ Works | V | Vetted; property-based per worker. Known gap: shared `.hypothesis` DB untested past `-n 8`. |
| 9 | pytest-metadata | 35.2M | ➖ N/A | V | Session metadata for report plugins; plugin active on workers, no parallel hazard (e2e gate). |
| 10 | pytest-env | 24.0M | ✅ Works | V | Env vars set on every worker. |
| 11 | pytest-httpx | 22.9M | ✅ Works | V | Per-test httpx mock fixture; isolated per worker (e2e gate). |
| 12 | pytest-html | 21.8M | 🔴 Silent | V | Writes no report at `-n ≥ 2` (gates on the master `workerinput`); a command-line `--html` is rstest's native report; for the plugin's own, `rstest -n 0 -- --html=...`. |
| 13 | pytest-django | 21.6M | ✅ Works | V | Per-worker test DB suffixed by `workerid`. Verified on django-allauth, which uses SQLite `:memory:`; server-backed databases (Postgres, MySQL) are not in the corpus yet. |
| 14 | pytest-split | 21.0M | 🟦 Native | V | Group selection is deselection (honored under the pool: e2e gate); rstest sharding is native `--shard K/N`. |
| 15 | pytest-repeat | 15.5M | ✅ Works | V | `@mark.repeat(N)` items distribute across workers. |
| 16 | pytest-json-report | 15.0M | 🔴 Silent / 🟦 | V | Report aggregator; no file at `-n ≥ 2`, written at `-n 0` (e2e gate). Use native `--report-json`. |
| 17 | pytest-benchmark | 14.2M | 🔶 `-n 0` | V | Auto-disables at `-n ≥ 2` (sees the pool as xdist); benchmark at `-n 0`, read `--benchmark-json`. e2e gate: no `--benchmark-json` written at `-n ≥ 2`, measured + written at `-n 0`. |
| 18 | pytest-socket | 13.9M | ✅ Works | V | `--disable-socket` blocks identically in parallel. |
| 19 | syrupy | 13.4M | ✅ Works | V | Snapshot asserts are per-test; run `--snapshot-update` at `-n 0` to avoid same-file write races. Corpus: langchain (`libs/core`) + langgraph snapshot asserts at `-n auto`. |
| 20 | pytest-unordered | 11.0M | ➖ N/A | V | Assertion helper; works under the pool (e2e gate). |
| 21 | pytest-base-url | 10.2M | ✅ Works | V | Config/fixture only; `--base-url` delivered to every worker (e2e gate). |
| 22 | pytest-randomly | 10.0M | ✅ Works | V | rstest synthesizes the `randomly_seed` the master would inject (one run-level seed, all workers agree); vetted. Native `--shuffle` also available. |
| 23 | pytest-icdiff | 9.7M | ➖ N/A | V | Assertion-diff repr; side-by-side diff reaches worker failure output (e2e gate). |
| 24 | pytest-playwright | 9.6M | ✅ Works | V | Per-worker browser context; `page` fixture works under the pool (e2e `plugin-services` gate). |
| 25 | pytest-homeassistant-custom-component | 9.4M | ✅ Works | V | Fixture bundle; per-worker `hass` fixture works under the pool (e2e `plugin-services` gate). |
| 26 | pytest-dependency | 8.9M | ⚠️ Caveat | V | Cross-test deps may span workers at `-n ≥ 2`; honored at `-n 0` (dependent skipped when dep fails: e2e gate), or use `--dist loadscope`. |
| 27 | pytest-sugar | 8.2M | 🔶 `-n 0` | V | Terminal-rendering; not painted at `-n ≥ 2` (rstest owns the terminal). Corpus-flagged as a benign silent/terminal case. |
| 28 | allure-pytest | 8.1M | ✅ Works | V | Writes one result file per test to a results dir: one file per test across workers (e2e gate). |
| 29 | pytest-custom-exit-code | 7.9M | ⚠️ Caveat | V | rstest computes the exit status from merged results; the plugin's per-worker exit hook is overridden: `--suppress-tests-failed-exit-code` cannot green a failing run (e2e gate). |
| 30 | pytest-instafail | 7.3M | 🔶 `-n 0` | V | Inline failure printing is terminal-owned; coexists under the pool (e2e gate). rstest streams failures natively (live progress). |
| 31 | pytest-postgresql | 6.6M | ✅ Works | V | Per-worker DB instance on a free port (e2e `plugin-services` gate). |
| 32 | pytest-recording | 6.1M | ✅ Works | V | VCR cassettes are per-test files; record and replay both parallel-safe. Corpus: langchain (`libs/langchain_v1` unit tests) VCR replay at `-n auto`. |
| 33 | pytest-order | 6.1M | ⚠️ Caveat | V | Ordering holds only within a worker; `-n 0` or `--dist loadfile`/`loadscope`. |
| 34 | pytest-aiohttp | 6.0M | ✅ Works | V | Vetted; per-test aiohttp loop. |
| 35 | pytest-dotenv | 5.5M | ✅ Works | V | `.env` loaded per worker (e2e gate). |
| 36 | pytest-subtests | 4.9M | ✅ Works | V | Sub-results ride the normal report hook; failing subtests attributed per worker (e2e gate). |
| 37 | pytest-testmon | 4.8M | 🔶 `-n 0` / 🟦 | V | Shared `.testmondata` isn't concurrency-safe; runs at `-n 0` (e2e gate). rstest has native `--incremental` / coverage-skip. |
| 38 | pytest-check | 4.0M | ➖ N/A | V | Soft multi-assert per test; all soft failures survive the merge (e2e gate). |
| 39 | pytest-httpserver | 3.9M | ✅ Works | V | Per-test server fixture on its own port (e2e gate). |
| 40 | pytest-timeouts | 3.7M | 🟦 Native | V | Phase timeouts; coexists under the pool (e2e gate). rstest has native `--timeout` / `--worker-timeout`. |
| 41 | pytest-github-actions-annotate-failures | 3.7M | 🟦 Native | V | Coexists under the pool; rstest emits GitHub `::error` annotations natively via `--output github` (e2e gate). |
| 42 | pytest-bdd | 3.6M | ✅ Works | V | Generates items from `.feature` at collection; scenario runs under the pool (e2e gate). |
| 43 | pytest-durations | 3.5M | 🔶 `-n 0` / 🟦 | V | Duration summary is terminal-owned; coexists under the pool (e2e gate). rstest has `--durations` and `--doctor`. |
| 44 | pytest-memray | 2.9M | 🔶 `-n 0` | V | `@limit_memory` is enforced per worker process: same pass/fail under the pool as at `-n 0` (e2e gate). Only the memory *summary* is terminal-owned; read it at `-n 0`. |
| 45 | pytest-codspeed | 2.9M | 🔶 `-n 0` | V | The `benchmark` fixture + `@mark.benchmark` coexist under the pool and `--codspeed` measurement completes at any `-n` (e2e gate); measure at `-n 0` for stable numbers. |
| 46 | pytest-random-order | 2.8M | ✅ Works | V | rstest seeds the `workerinput["random_order_seed"]` its `pytest_configure` reads unconditionally: without it the plugin KeyError'd every `-n ≥ 2` run (dead-master-path, now closed; e2e gate). All workers share the seed; global execution order still follows rstest's duration-first dispatch, so use `-n 0` or native `--shuffle` for a strict end-to-end shuffle. |
| 47 | pytest-factoryboy | 2.8M | ✅ Works | V | Fixture generation; registered factory fixture resolves on workers (e2e gate). |
| 48 | pytest-ordering | 2.8M | ⚠️ Caveat | V | Same as pytest-order: `@mark.run(order=N)` honored within a worker / at `-n 0` (e2e gate). |
| 49 | pytest-snapshot | 2.6M | ✅ Works | V | Asserts parallel-safe; update at `-n 0`, assert under the pool (e2e gate). |
| 50 | pytest-retry | 2.6M | ✅ Works | V | rstest seeds the `server_port` its worker branch reads (each worker plays master for itself); vetted. Native reruns also available. |
| 51 | pytest-docker | 2.4M | ⚠️ Caveat | i | Session docker-compose fixture → one stack **per worker**. Fine if the service is per-worker; for a single shared stack use `--dist loadgroup` or `-n 0`. |
| 52 | pytest-vcr | 2.3M | ✅ Works | i | Per-test VCR cassette files; record/replay parallel-safe (same class as pytest-recording #32). |
| 53 | pytest-lazy-fixtures | 2.2M | ➖ N/A | i | Resolves fixture *values* inside `parametrize`; no parallel interaction. |
| 54 | pytest-flakefinder | 2.1M | ✅ Works | i | Multiplies each item N× at collection; the copies distribute across workers like any parametrization. |
| 55 | pytest-profiling | 1.9M | 🔶 `-n 0` | i | Per-test `.prof` files written per worker; the combined svg/call graph + summary are terminal/aggregate: read at `-n 0`. |
| 56 | pytest-celery | 1.9M | ⚠️ Caveat | i | Broker/worker fixtures (often Docker-backed); one broker per worker, or `-n 0` for a shared broker. |
| 57 | pytest-watcher | 1.8M | 🟦 Native | i | External file-watch re-run wrapper around the pytest process; rstest has native watch (`rstest --watch`). |
| 58 | pytest-datadir | 1.8M | ✅ Works | i | Per-test copied data-dir fixture; isolated per test/worker. |
| 59 | pytest-docker-tools | 1.7M | ⚠️ Caveat | i | Container fixtures; same per-worker-stack caveat as pytest-docker (#51). |
| 60 | pytest-test-groups | 1.6M | 🟦 Native | i | `--test-group`/`--test-group-count` selection is deselection (honored under the pool); rstest sharding is native `--shard K/N`. |
| 61 | pytest-testinfra | 1.5M | ✅ Works | i | Per-test host/connection fixtures (ssh/docker/local); isolated per test. |
| 62 | pytest-flask | 1.5M | ✅ Works | i | `live_server` binds an ephemeral port per worker; app/client fixtures are per-test. |
| 63 | pytest-freezegun | 1.4M | ✅ Works | i | In-process time freeze per worker (see the freezegun note in [Tested compatibility](../guides/plugins.md#tested-compatibility): don't put `now()` in parametrize IDs). |
| 64 | pytest-freezer | 1.4M | ✅ Works | i | Same in-process time-freeze model as pytest-freezegun. |
| 65 | pytest-alembic | 1.3M | ✅ Works | i | Migration tests; per-worker test DB (pytest-django class). |
| 66 | pytest-pretty | 1.2M | 🔶 `-n 0` | i | Rich terminal painting; rstest owns the terminal at `-n ≥ 2`. Corpus-loaded only at `-n 0` (pydantic), so no parallel evidence yet. |
| 67 | pytest-opentelemetry | 1.2M | ✅ Works | i | Emits a span per test on each worker; aggregate at the collector (per-worker exporters, no shared master state). |
| 68 | pytest-qt | 1.2M | ✅ Works | i | Per-worker Qt app / `qtbot` fixture (needs a display or xvfb, as under any runner). |
| 69 | pytest-describe | 1.2M | ✅ Works | i | Generates items from `describe`/`it` blocks at collection; distribute normally. |
| 70 | pytest-sftpserver | 1.1M | ✅ Works | i | Per-test SFTP server fixture on its own port (pytest-httpserver class #39). |
| 71 | pytest-regressions | 1.0M | ✅ Works | i | Data-file regression asserts are parallel-safe; run `--force-regen` at `-n 0` to avoid same-file write races (snapshot class #49). |
| 72 | pytest-deadfixtures | 1.0M | 🔶 `-n 0` | i | `--dead-fixtures` is a whole-suite static scan; single process. |
| 73 | pytest-cases | 1.0M | ➖ N/A | i | Case collection/parametrization at collection time; no parallel hazard. |
| 74 | pytest-reportportal | 1.0M | ⚠️ Caveat | i | Streams to a ReportPortal launch per worker; use one launch id (config) or `-n 0` to avoid N launches. |
| 75 | pytest-ansible | 983K | ✅ Works | i | Per-test ansible host/inventory fixtures. |
| 76 | pytest-find-dependencies | 983K | 🔶 `-n 0` | i | `--find-dependencies` reorders the whole suite to detect inter-test coupling; single process. |
| 77 | pytest-watch | 946K | 🟦 Native | i | External re-run wrapper (`ptw`); rstest has native watch. |
| 78 | pytest-azurepipelines | 858K | 🔴 Silent / 🟦 | i | Azure CI result upload / `##vso` logging is master/terminal-owned; use native `--junitxml` under the pool. |
| 79 | pytest-picked | 819K | 🟦 Native | i | Runs tests from git-changed files (selection = deselection); rstest has native changed-based selection. |
| 80 | pytest-anyio | 809K | ✅ Works | i | anyio async tests per worker. The `anyio` package's own built-in plugin is corpus-loaded in parallel (7 suites); this separate distribution is not. |
| 81 | pytest-reportlog | 802K | 🔴 Silent | V\* | `--report-log` writes **no file** at `-n ≥ 2` (gated on the master); written at `-n 0`. e2e gate: no crash under the pool. Native `--report-json`. |
| 82 | pytest-race | 793K | ✅ Works | i | `--race` runs one test concurrently in threads to surface races; per-test, in-process. |
| 83 | pytest-assume | 790K | ➖ N/A | i | Soft multi-assert; all assumptions ride the normal report (pytest-check class #38). |
| 84 | pytest-md | 737K | 🔴 Silent | V\* | `--md` writes an empty *"0 tests"* report at `-n ≥ 2` (aggregates on the master); real report at `-n 0`. e2e gate: no crash. |
| 85 | pytest-harvest | 731K | 🔶 `-n 0` | i | Collects fixture/test results into a whole-session store; cross-worker aggregation needs `-n 0`. |
| 86 | pytest-cache | 688K | ➖ N/A | i | Legacy backport of the now-core `cacheprovider`; inert alongside rstest's cache. |
| 87 | pytest-doctestplus | 678K | ✅ Works | i | Enhanced doctests collected at collection time; rstest also has native `--doctest-modules`. |
| 88 | pytest-mpl | 677K | ✅ Works | i | Per-test matplotlib image compare; generate baselines with `--mpl-generate-path` at `-n 0`. |
| 89 | pytest-variables | 661K | ✅ Works | i | `--variables <file>` delivered to every worker as a fixture (pytest-base-url class #21). |
| 90 | pytest-clarity | 651K | ➖ N/A | i | Assertion-diff prettifier; the diff reaches worker failure output (pytest-icdiff class #23). |
| 91 | pytest-nunit | 629K | 🔴 Silent | V\* | NUnit XML: **no file** at `-n ≥ 2` (single-master aggregator); written at `-n 0`. e2e gate: no crash. Native `--junitxml`. |
| 92 | pytest-pylint | 616K | ✅ Works | i | Pylint-as-tests collected per file; runs per worker (shared pylint cache is a mild contention caveat). |
| 93 | pytest-lazy-fixture | 608K | ➖ N/A | i | Legacy `lazy_fixture` (superseded by pytest-lazy-fixtures #53); value-level, no parallel hazard. |
| 94 | pylint-pytest | 601K | ➖ N/A | i | A **pylint** plugin (lints pytest code), not a pytest runtime plugin, never loaded by the test session. |
| 95 | pytest-examples | 595K | ✅ Works | i | Code-example / docstring testing. Corpus-loaded only at `-n 0` (pydantic), so no parallel evidence yet. |
| 96 | pytest-pytestrail | 566K | ⚠️ Caveat | i | TestRail reporter; per-worker case results, or `-n 0` for one run submission. |
| 97 | pytest-mypy | 557K | ✅ Works | V | **Dead-master-path closed.** Its worker branch reads `workerinput["mypy_config_stash_serialized"]`, a key only its xdist controller sets, so merely installing it `KeyError`'d every `-n ≥ 2` run. rstest now seeds a unique per-worker mypy results-cache path; mypy runs lazily per worker (`MypyResults.from_session`), so type errors surface identically at `-n auto` and `-n 0` (e2e gate). |
| 98 | pytest-csv | 552K | ⚠️ Caveat | V\* | `--csv` writes a **racy per-worker** CSV under the pool (each worker opens the same path, last close wins, may capture only one worker's subset); use `-n 0` or native `--report-json`. e2e gate: no crash. |
| 99 | pytest-subprocess | 515K | ✅ Works | i | Per-test `fake_process` fixture; isolated per test. |
| 100 | pytest-flake8 | 506K | ✅ Works | i | flake8-as-tests collected per file; runs per worker (shared flake8 cache is a mild contention caveat). |

`V` = runtime-verified (an e2e gate exercises the feature, or a corpus suite loads
the plugin at `-n auto`); `V*` = verified only that the plugin does **not crash**
under the pool and its artifact lands at `-n 0` (the shared report-aggregator
gate), not that a usable artifact is produced at `-n ≥ 2`; `i` = inferred from
the plugin's category, not yet runtime-verified. The `i` rows of 51–100 are the
next verification tranche.

## Summary by verdict (ranks 1–50)

- **✅ Works, 25:** the majority need nothing (asyncio, mock, hypothesis, env, httpx, django, repeat, socket, syrupy, base-url, randomly, playwright, homeassistant, allure, postgresql, recording, aiohttp, dotenv, subtests, httpserver, bdd, factoryboy, snapshot, retry, random-order).
- **🟦 Native, 6:** rstest has a first-class replacement (cov, timeout, rerunfailures, split, timeouts, github-actions-annotate).
- **🔶 `-n 0`, 7:** terminal / benchmark / memory features want single-worker (benchmark, sugar, instafail, testmon, durations, memray, codspeed).
- **⚠️ Caveat, 4:** works with a limit (dependency, custom-exit-code, order, ordering).
- **🔴 Silent, 3:** report aggregators gated on the master (json-ctrf, html, json-report); emit at `-n 0` or use a native artifact.
- **➖ N/A, 5:** no parallel interaction (xdist neutralized, metadata, unordered, icdiff, check).

**36 of 50 run unchanged or via a native flag; 4 more work with a caveat; the
remaining 10 want `-n 0` for one reporting / ordering / benchmark feature.**
None of them crash under the pool, but the 3 silent reporters produce no
artifact there, so treat them as not working in parallel and use the native
equivalent.

The recurring fault line: a plugin that reads xdist-**master**-injected
`workerinput` keys, aggregates from a single master, or paints the terminal.
rstest closes the first class per plugin (seeding/emulation), owns the terminal
and dispatch for the second and third, and ships native equivalents for the
common reporters.

## Summary by verdict (ranks 51–100)

- **✅ Works, 24:** vcr, flakefinder, datadir, testinfra, flask, freezegun, freezer, alembic, opentelemetry, qt, describe, sftpserver, regressions, ansible, anyio, race, doctestplus, mpl, variables, pylint, examples, mypy, subprocess, flake8.
- **🟦 Native, 4:** watcher, test-groups, watch, picked (rstest ships watch / sharding / changed-selection).
- **🔶 `-n 0`, 5:** profiling, deadfixtures, find-dependencies, harvest, pretty.
- **⚠️ Caveat, 6:** docker, celery, docker-tools, reportportal, pytestrail, csv.
- **🔴 Silent, 4:** reportlog, md, nunit, azurepipelines (single-master aggregators; emit at `-n 0` or use a native artifact).
- **➖ N/A, 7:** lazy-fixtures, cases, assume, cache, clarity, lazy-fixture, pylint-pytest.

The fault line is the same as in the top 50: a plugin that reads
xdist-**master**-injected `workerinput` keys (pytest-mypy, closed by seeding),
aggregates from a single master (the reporters, silent or racy under the pool),
or paints the terminal (pretty, profiling). None crash; the silent and racy
reporters have native rstest equivalents.

## Detecting dark plugins

**Shipped: runtime flag warning.** When a parallel run (`-n ≥ 2`) is invoked
with a flag whose plugin goes dark under the pool (`--json-report`,
`--report-log`, `--ctrf`, `--nunit-xml`, `--md`, `--csv`, `--benchmark*`),
rstest prints a heads-up before the run naming the plugin and the parallel-safe
alternative; see
[the silent-no-op class](../guides/plugins.md#the-silent-no-op-class). This is
argv-driven: it catches the known-dark flags deterministically, with no false
positives.

**Planned: static dead-master-path scan.** A general `--warn-on-dead-master-path`
detector that inspects *any* installed plugin's code for the "am I the xdist
master?" branch pattern (predicting silent-no-op **and** crash classes for
unlisted plugins) is designed but **not yet implemented**. Two false-positive shapes it will have to handle, from analyzing
the corpus plugins, illustrate the precision work still required:

- **pytest-sugar**: pairs a `hasplugin("xdist")` gate with a `slaveinput`
  read, but the read is a defensive `getattr(config, "slaveinput", None)` (not a
  subscript), so it can't `KeyError`. Real behavior: terminal not painted at
  `-n ≥ 2` (the benign 🔶 `-n 0` case above), not a crash.
- **sqlalchemy** test conftest: a real `workerinput["follower_ident"]`
  subscript, yet it runs crash-free at `-n auto` because rstest **seeds**
  `follower_ident` via its `pytest_configure_node` emulation; a static scan
  would need to model that runtime seeding.

No genuinely non-compatible plugin has surfaced across the corpus.
