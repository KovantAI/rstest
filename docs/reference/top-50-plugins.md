# Top 50 pytest plugins — rstest compatibility

The 50 most-downloaded pytest plugins (30-day PyPI counts via
[pythontest.com/top-pytest-plugins](https://pythontest.com/top-pytest-plugins/),
Aug 2026), each classified for how it behaves under rstest's parallel pool.

**Verdicts**

| Mark | Meaning |
|---|---|
| ✅ Works | Parallel-safe as-is; no change needed. |
| 🟦 Native | Works, but rstest ships a built-in that supersedes it (prefer the native flag). |
| ⚠️ Caveat | Works with a stated limitation, usually a subset that needs `-n 0`. |
| 🔶 `-n 0` | Run single-worker for this plugin's feature (terminal painting, benchmarks, ordering). |
| 🔴 Silent | Produces **nothing** at `-n ≥ 2` (report aggregators gated on the xdist master); use `-n 0` or a native equivalent. |
| ➖ N/A | Unaffected by parallelism (assertion / fixture / format helpers). |

**Verified** column: **V** = exercised by rstest's tested-compat table, the
[vetted allowlist](../guides/plugins.md#detecting-the-dead-master-path-automatically),
or loaded by a corpus suite (see the runtime inventory in
[Plugins exercised by the corpus](corpus-plugins.md)); **i** = inferred from the
plugin's category, not yet runtime-verified.

| # | Plugin | ~Downloads | Verdict | V | Note |
|---|---|---|---|---|---|
| 1 | pytest-asyncio | 275.9M | ✅ Works | V | Per-test event loop; vetted (rstest provides the worker context it sniffs). |
| 2 | pytest-json-ctrf | 273.0M | 🔴 Silent / 🟦 | V | Report aggregator; no file at `-n ≥ 2`, written at `-n 0` (e2e gate). Use native `--report-json` under the pool. |
| 3 | pytest-cov | 235.9M | 🟦 Native | V | rstest orchestrates coverage combine across workers; native `--cov`. Vetted. |
| 4 | pytest-xdist | 177.1M | ➖ N/A | V | Neutralized inside workers — rstest *is* the parallel runner; its options parse but stay inert. |
| 5 | pytest-mock | 105.0M | ✅ Works | V | Per-test `mocker` fixture; vetted. |
| 6 | pytest-timeout | 103.1M | 🟦 Native | V | Fires in both modes; rstest has native `--timeout` (don't run both — two SIGALRM handlers). |
| 7 | pytest-rerunfailures | 79.1M | 🟦 Native | V | Unregistered in the pool (its xdist `sock_port` branch would KeyError); rstest owns reruns (`--reruns`, `@mark.flaky`). |
| 8 | hypothesis | 48.3M | ✅ Works | V | Vetted; property-based per worker. Known gap: shared `.hypothesis` DB untested past `-n 8`. |
| 9 | pytest-metadata | 35.2M | ➖ N/A | V | Session metadata for report plugins; plugin active on workers, no parallel hazard (e2e gate). |
| 10 | pytest-env | 24.0M | ✅ Works | V | Env vars set on every worker. |
| 11 | pytest-httpx | 22.9M | ✅ Works | V | Per-test httpx mock fixture; isolated per worker (e2e gate). |
| 12 | pytest-html | 21.8M | 🔴 Silent | V | Writes no report at `-n ≥ 2` (gates on the master `workerinput`); use `-n 0` or native `--html`. |
| 13 | pytest-django | 21.6M | ✅ Works | V | Per-worker test DB suffixed by `workerid`; vetted. |
| 14 | pytest-split | 21.0M | 🟦 Native | V | Group selection is deselection (honored under the pool — e2e gate); rstest sharding is native `--shard K/N`. |
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
| 26 | pytest-dependency | 8.9M | ⚠️ Caveat | V | Cross-test deps may span workers at `-n ≥ 2`; honored at `-n 0` (dependent skipped when dep fails — e2e gate), or use `--dist loadscope`. |
| 27 | pytest-sugar | 8.2M | 🔶 `-n 0` | V | Terminal-rendering; not painted at `-n ≥ 2` (rstest owns the terminal). Corpus-flagged as a benign silent/terminal case. |
| 28 | allure-pytest | 8.1M | ✅ Works | V | Writes one result file per test to a results dir — one file per test across workers (e2e gate). |
| 29 | pytest-custom-exit-code | 7.9M | ⚠️ Caveat | V | rstest computes the exit status from merged results; the plugin's per-worker exit hook is overridden — `--suppress-tests-failed-exit-code` cannot green a failing run (e2e gate). |
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
| 44 | pytest-memray | 2.9M | 🔶 `-n 0` | V | `@limit_memory` is enforced per worker process — same pass/fail under the pool as at `-n 0` (e2e gate). Only the memory *summary* is terminal-owned; read it at `-n 0`. |
| 45 | pytest-codspeed | 2.9M | 🔶 `-n 0` | V | The `benchmark` fixture + `@mark.benchmark` coexist under the pool and `--codspeed` measurement completes at any `-n` (e2e gate); measure at `-n 0` for stable numbers. |
| 46 | pytest-random-order | 2.8M | ✅ Works | V | rstest seeds the `workerinput["random_order_seed"]` its `pytest_configure` reads unconditionally — without it the plugin KeyError'd every `-n ≥ 2` run (dead-master-path, now closed; e2e gate). All workers share the seed; global execution order still follows rstest's duration-first dispatch, so use `-n 0` or native `--shuffle` for a strict end-to-end shuffle. |
| 47 | pytest-factoryboy | 2.8M | ✅ Works | V | Fixture generation; registered factory fixture resolves on workers (e2e gate). |
| 48 | pytest-ordering | 2.8M | ⚠️ Caveat | V | Same as pytest-order — `@mark.run(order=N)` honored within a worker / at `-n 0` (e2e gate). |
| 49 | pytest-snapshot | 2.6M | ✅ Works | V | Asserts parallel-safe; update at `-n 0`, assert under the pool (e2e gate). |
| 50 | pytest-retry | 2.6M | ✅ Works | V | rstest seeds the `server_port` its worker branch reads (each worker plays master for itself); vetted. Native reruns also available. |

## Summary by verdict (of 50)

- **✅ Works — 25:** the majority need nothing (asyncio, mock, hypothesis, env, httpx, django, repeat, socket, syrupy, base-url, randomly, playwright, homeassistant, allure, postgresql, recording, aiohttp, dotenv, subtests, httpserver, bdd, factoryboy, snapshot, retry, random-order).
- **🟦 Native — 6:** rstest has a first-class replacement (cov, timeout, rerunfailures, split, timeouts, github-actions-annotate).
- **🔶 `-n 0` — 7:** terminal / benchmark / memory features want single-worker (benchmark, sugar, instafail, testmon, durations, memray, codspeed).
- **⚠️ Caveat — 4:** works with a limit (dependency, custom-exit-code, order, ordering).
- **🔴 Silent — 3:** report aggregators gated on the master (json-ctrf, html, json-report) — emit at `-n 0` or use a native artifact.
- **➖ N/A — 5:** no parallel interaction (xdist neutralized, metadata, unordered, icdiff, check).

**36 of 50 run unchanged or via a native flag; 4 more work with a caveat; the
remaining 10 want `-n 0` for one reporting / ordering / benchmark feature. None
are broken.**

The recurring fault line is unchanged: a plugin that reads xdist-**master**-injected
`workerinput` keys, aggregates from a single master, or paints the terminal.
rstest closes the first class per plugin (seeding/​emulation), owns the terminal
and dispatch for the second and third, and ships native equivalents for the
common reporters.

## Corpus cross-check (dead-master-path scan)

Running [`--warn-on-dead-master-path`](../guides/plugins.md#detecting-the-dead-master-path-automatically)
across all 33 corpus suites flagged **two** plugins — both **false positives**,
which pin down the static detector's current precision limits:

- **pytest-sugar** (fastapi) — classified *crash* because it pairs a
  `hasplugin("xdist")` gate with a `slaveinput` read, but the read is a
  defensive `getattr(config, "slaveinput", None)` (not a subscript;
  `confirmed=False`), so it can't actually `KeyError`. Real behavior: terminal
  not painted at `-n ≥ 2` (the benign 🔶 `-n 0` case above).
- **sqlalchemy** test conftest — a real `workerinput["follower_ident"]`
  subscript (`confirmed=True`), yet it runs crash-free at `-n 4` because rstest
  **seeds** `follower_ident` via its `pytest_configure_node` emulation. The
  static scan doesn't model that runtime seeding.

Takeaways for the detector: a *crash* classification with `confirmed=False`
(no direct subscript) is really the silent/terminal class and could be
downgraded; and keys rstest provisions through `configure_node` emulation are
not crashes and should be excluded from the confirmed set. Both refinements are
tracked in `BACKLOG.md` §"Dead-master-path detector precision". No **genuinely**
non-compatible plugin surfaced across the corpus.

Reproduce this sweep with `python3 corpus/scan_plugins.py` (offline, assumes the
corpus is prepared).
