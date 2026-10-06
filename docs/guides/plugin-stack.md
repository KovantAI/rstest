# Your plugin stack

A playbook for checking that the pytest plugins you rely on work under rstest
before you switch.

## Who this is for

You maintain a suite that leans on the common pytest plugin stack
(pytest-django, pytest-asyncio, hypothesis, pytest-cov, pytest-mock,
pytest-html, pytest-sugar, freezegun, pytest-timeout, pytest-rerunfailures,
pytest-xdist), and the one question that matters
before switching runners is: *will these keep working under rstest's
parallel pool and its vendored pytest 9 core?*

The one-line reassurance: **every plugin in this stack either works as-is or
is replaced by an rstest-native equivalent (coverage, timeouts, reruns,
parallelism), and nothing needs porting**. The adjustments are two
report/terminal plugins you move to `-n 0`, pytest-timeout, which rstest
replaces ([pytest-timeout](plugins.md#pytest-timeout)), and pytest-cov's
`--cov`, which belongs on the rstest command line rather than in `addopts`.
xdist's `--dist` mode moves to `[tool.rstest] dist`. Plugins load
through the standard `pytest11` entry points against a real
[pluggy](https://github.com/pytest-dev/pluggy), as under pytest
([Plugins](plugins.md)). The one thing to watch is flag names rstest owns,
such as `--timeout` and `--html`
([Flags rstest owns](migrate-from-pytest.md#flags-rstest-owns)).

The one command to check your own suite against the vendored core:

```console
$ rstest -n 0
```

This is a single vendored-pytest-9.1.1 session over your arguments
(single-worker mode; see [Compatibility](../concepts/compatibility.md)).
Green here means the whole stack is happy with pytest 9 before you add a
single worker.

## The typical-stack scorecard

The verified column is **V** = runtime-verified (an e2e gate or corpus suite
exercises it) vs **i** = inferred from category, not yet runtime-verified,
from the [top-100 matrix](../reference/top-100-plugins.md). **V\*** = partly
verified: the row's note says which part the corpus or a gate covers and what
it does not.

| Plugin | Status (verdict) | V/i | Per-plugin caveat |
|---|---|---|---|
| pytest-django | ✅ Works | V\* | Per-worker test DB suffixed by `workerid` ([top-100](../reference/top-100-plugins.md)). Verified on SQLite `:memory:` only (django-allauth); a server-backed per-worker DB (Postgres, MySQL) is not in the corpus, so confirm yours with one parallel run. |
| pytest-asyncio | ✅ Works | V | Per-test event loop; rstest provides the worker context it sniffs ([top-100](../reference/top-100-plugins.md)). |
| hypothesis | ✅ Works | V | Property-based per worker. Known gap: shared `.hypothesis` example DB untested past `-n 8`. See [known gaps](../concepts/compatibility.md) for the per-worker-DB mitigation. |
| pytest-cov | 🟦 Native | V | rstest orchestrates the coverage combine across workers via native `--cov`. Pass `--cov` on the rstest command line: from `addopts` alone a parallel run writes no report. See [Coverage](coverage.md). |
| pytest-mock | ✅ Works | V | Per-test `mocker` fixture; vetted ([top-100](../reference/top-100-plugins.md)). |
| pytest-html | 🔴 Silent | V | **Writes no report at `-n ≥ 2`**: a silent no-op, not a crash. Use rstest's native `--html`, or a `-n 0` pass for the plugin's own layout ([HTML & aggregated reporting](plugins.md#html-aggregated-reporting-under-parallelism)). |
| pytest-sugar | 🔶 `-n 0` | V | Terminal-rendering; **not painted at `-n ≥ 2`**: rstest owns the terminal. Non-visual behavior unaffected; run at `-n 0` when you want its rendering. |
| freezegun | ✅ Works | V | In-process time freezing is per-worker; five corpus suites load it in parallel ([tested compatibility](plugins.md#tested-compatibility)). Keep `now()` out of parametrize IDs unless every worker computes the same string ([unstable parametrize IDs](../concepts/compatibility.md#unstable-parametrize-ids)). |
| pytest-timeout | 🟦 Native | V | rstest's own `--timeout` and `@pytest.mark.timeout` replace it at every worker count. Uninstall or disable it, moving its ini setting first ([pytest-timeout](plugins.md#pytest-timeout)). |
| pytest-rerunfailures | 🟦 Native | V | Unregistered in the pool; rstest owns reruns (`--reruns`, `--only-rerun`, `@mark.flaky(reruns=N)`). The mark's budget (`reruns=` or positional) and `condition` are honored; `reruns_delay`, `only_rerun` and `rerun_except` are not carried over ([Flaky tests](flaky-tests.md#detect-reruns)). |
| pytest-xdist | ➖ N/A | V | Neutralized inside workers: rstest is the parallel runner and xdist's options parse but stay inert. Keep it installed while a conftest implements its hooks ([Controller-side hooks](migrate-from-xdist.md#controller-side-hooks)). |

Notes on freezegun: it is a library, not a pytest plugin, so it has no
top-100 row; the [tested-compatibility table](plugins.md#tested-compatibility)
uses the same verdict marks as the top-100 matrix. The pytest-plugin wrappers
around it, **pytest-freezegun** and **pytest-freezer**
([top-100](../reference/top-100-plugins.md)), are marked **✅ Works (i,
inferred)**: same in-process time-freeze model, not yet runtime-verified.

Seven of the eleven need no change on your side: five run as-is,
pytest-rerunfailures is replaced by rstest's own reruns, and pytest-xdist is
neutralized. Four need one: pytest-cov needs `--cov` on the rstest command
line (from `addopts` alone a parallel run writes no report), pytest-html and
pytest-sugar need a `-n 0` run for their own output, and pytest-timeout must
be uninstalled or disabled with `-p no:timeout`.

## Plugin versions vs the vendored pytest 9

rstest vendors **pytest 9.1.1**, unmodified, and plugins load *into* that
core, so each plugin's own code must support pytest 9 (its scorecard status
above is about parallel behavior only) and a `pytest<9` install pin is inert
at runtime; rstest warns when it sees one. The full rule is in
[Plugin versions vs the vendored core](../concepts/compatibility.md#plugin-versions-vs-the-vendored-core).

**`rstest -n 0` exercises every installed plugin against vendored pytest
9.1.1** and surfaces any pytest-9 incompatibility *exactly as a real pytest
upgrade would*, because that is effectively what it is. Clear it there first.

A stack that is warning-clean on a recent pytest 8.x is almost always
already pytest-9-clean; if it isn't, clear the deprecations *before* you
switch the runner
([Your suite runs on pytest 9](../getting-started/installation.md#your-suite-runs-on-pytest-9),
with the step-by-step in [Upgrading to pytest 9](upgrade-to-pytest9.md)).

### Known-good versions

The versions below are the ones the [compatibility corpus](../concepts/compatibility.md)
resolved and ran against vendored pytest 9.1.1. They are **known-good
floors to aim for, not proven minimums**: older releases were not tested,
and the corpus installs plugins unpinned, so a newer corpus run may resolve
newer versions. Every corpus run records the versions it installed, and the
rows are regenerated from that data with
`python3 corpus/plugin_versions.py --from-venvs` (reads the populated corpus
venvs under `corpus/work/*/venv`, so run the corpus first).
The declared range is the plugin's own `Requires-Dist` on pytest.

| Plugin | Verified with | Declared pytest range | Exercised by |
|---|---|---|---|
| pytest-django | 4.14.0 | `>=7.0.0` | django-allauth |
| pytest-asyncio | 1.4.0 | `>=8.4,<10` | aiohttp, django-allauth, langchain, langgraph, structlog |
| hypothesis | 6.165.10 / 6.167.1 | `>=4.6` (`[pytest]` extra) | attrs, anyio, packaging, pandas, pydantic, python-dateutil |
| pytest-cov | 7.1.0 (coverage 7.16.0) | `>=7` | aiohttp, arrow, fastapi, langchain, python-dateutil, requests |
| pytest-mock | 3.15.1 | `>=6.2.5` | aiohttp, anyio, arrow, langchain, langgraph, pydantic |
| pytest-sugar | 1.1.1 | `>=6.2.0` | fastapi |
| freezegun | 1.5.5 | none (no pytest dependency) | aiohttp, freezegun, itsdangerous, langchain, python-dateutil |
| pytest-html | 4.2.0 | `>=7` | e2e gate (`gate_pytest_html_real_plugin`), not a corpus suite |

If you are on an older release than the one listed, `rstest -n 0` is the
check: it either passes, or fails the same way a real pytest 9 upgrade would.

## What to move to `-n 0`, and why

pytest-html and pytest-sugar go quiet under the pool because rstest owns a
single merged terminal and runs no Python controller to aggregate worker
output. The parallel-safe report paths and the `-n 0` fallback are in
[HTML & aggregated reporting](plugins.md#html-aggregated-reporting-under-parallelism).

## Go deeper

- [Plugins](plugins.md): how loading works, the tested-compatibility
  table, hook coverage, and the self-audit script for a home-grown reporter.
- [Top 100 plugin compatibility matrix](../reference/top-100-plugins.md):
  every plugin here except freezegun, plus 90 more, each with its verdict and V/i mark.
- [Plugins exercised by the corpus](../reference/corpus-plugins.md): the
  runtime inventory of which real suites load which plugins under rstest.
- [Compatibility](../concepts/compatibility.md): the parity contract, the
  vendored-pytest policy, and the known-gaps list.
- [Coverage](coverage.md): pytest-cov under the parallel pool, per-test
  contexts, and the diff-coverage gate.
