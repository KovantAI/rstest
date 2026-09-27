# Your plugin stack

A playbook for checking that the pytest plugins you rely on work under rstest before you switch.

## Who this is for

You maintain a suite that leans on the common pytest plugin stack
(pytest-django, pytest-asyncio, hypothesis, pytest-cov, pytest-mock,
pytest-html, pytest-sugar, freezegun), and the one question that matters
before switching runners is: *will these keep working under rstest's
parallel pool and its vendored pytest 9 core?*

The one-line reassurance: **every plugin in this stack loads and runs; the
only adjustments are two report/terminal plugins you move to `-n 0`, and
nothing needs re-installing, porting, or configuring**: plugins load
through the standard `pytest11` entry points against a real
[pluggy](https://github.com/pytest-dev/pluggy), as under pytest
([Plugins](plugins.md)). The one thing to watch is flag names rstest owns,
such as `--timeout` and `--html`
([Flags rstest owns](migrate-from-pytest.md#flags-rstest-owns)).

The one command to check your own suite against the vendored core:

```console
$ rstest -n 0
```

This is a single vendored-pytest-9.1.1 session over your arguments, the
byte-exact contract ([Compatibility](../concepts/compatibility.md)).
Green here means the whole stack is happy with pytest 9 before you add a
single worker.

## The typical-stack scorecard

Status markers are copied verbatim from the sources. The verified column is
**V** = runtime-verified (an e2e gate or corpus suite exercises it) vs **i**
= inferred from category, not yet runtime-verified, from the
[top-100 matrix](../reference/top-100-plugins.md). An `i` here has *not* been
upgraded to `V`.

| Plugin | Status (verdict) | V/i | Per-plugin caveat |
|---|---|---|---|
| pytest-django | ✅ Works | V | Per-worker test DB suffixed by `workerid` ([top-100](../reference/top-100-plugins.md)). Verified on SQLite `:memory:` only (django-allauth); confirm a Postgres/MySQL setup with one parallel run. |
| pytest-asyncio | ✅ Works | V | Per-test event loop; rstest provides the worker context it sniffs ([top-100](../reference/top-100-plugins.md)). |
| hypothesis | ✅ Works | V | Property-based per worker. Known gap: shared `.hypothesis` example DB untested past `-n 8`. See [known gaps](../concepts/compatibility.md) for the per-worker-DB mitigation. |
| pytest-cov | 🟦 Native | V | rstest orchestrates the coverage combine across workers via native `--cov`; percentages match a serial run exactly. See [Coverage](coverage.md). |
| pytest-mock | ✅ Works | V | Per-test `mocker` fixture; vetted ([top-100](../reference/top-100-plugins.md)). |
| pytest-html | 🔴 Silent | V | **Writes no report at `-n ≥ 2`**: a silent no-op, not a crash (gates on the master `workerinput`). A command-line `--html` is rstest's native report; for the plugin's own report run `-n 0 -- --html=...`. See below. |
| pytest-sugar | 🔶 `-n 0` | V | Terminal-rendering; **not painted at `-n ≥ 2`**: rstest owns the terminal. Non-visual behavior unaffected; run at `-n 0` when you want its rendering. |
| freezegun | Works | n/a | From the [tested-compatibility table](plugins.md) (in-process time freezing is per-worker); **but don't put `now()` in parametrize IDs** ([time-derived IDs gap](../concepts/compatibility.md)). |

Notes on freezegun: the tested-compatibility table tiers it as **Works**
(that table uses Works / caveat / parallel-unsafe, not V/i). The
pytest-plugin wrappers around it, **pytest-freezegun** and
**pytest-freezer** ([top-100](../reference/top-100-plugins.md)), are marked
**✅ Works (i, inferred)**: same in-process time-freeze model, not yet
runtime-verified.

Six of the eight run unchanged or via a native flag; the only two that need
a mode switch are pytest-html and pytest-sugar.

## Plugin versions vs the vendored pytest 9

rstest vendors **pytest 9.1.1**, unmodified
([Compatibility](../concepts/compatibility.md)), and plugins load *into* that
core. The consequence to internalize:

- **`import pytest` inside any plugin resolves to the vendored pytest
  9.1.1.** So the plugin's *own code* must support pytest 9. Its status in
  the scorecard above is about parallel behavior, but the plugin still has
  to be a pytest-9-compatible release.
- **A `pytest<9` install pin is inert at runtime.** It only constrains pip
  at install time; it does not change which pytest a plugin sees once the
  worker is running. Under rstest the worker always runs vendored 9.
- **rstest warns about inert pins.** When a loaded plugin's own metadata
  excludes the running pytest (say it declares `pytest<9`), rstest prints one
  `rstest: warning: <plugin> <version> requires pytest<9, ...` line to stderr
  per run. The run is unaffected; the warning just tells you the pin is not
  doing anything.
- **`rstest -n 0` exercises every installed plugin against vendored-9** and
  surfaces any pytest-9 incompatibility *exactly as a real pytest upgrade
  would*, because that is effectively what it is. Clear it there first.

Because pytest 9 is a **cleanup major** (it removes APIs that already warned
throughout 8.x and keeps the collection model, fixture engine, `_pytest.*`
paths, and pluggy contract; see [Compatibility](../concepts/compatibility.md)),
a stack that is warning-clean on a recent pytest 8.x is almost always
already pytest-9-clean. If it isn't, clear the deprecations *before* you
switch the runner: the step-by-step is
[Upgrading to pytest 9](upgrade-to-pytest9.md): run
`pytest -W error::pytest.PytestDeprecationWarning` on your current pytest, then
`rstest -n 0` as the backstop.

### Known-good versions

The versions below are the ones the [compatibility corpus](../concepts/compatibility.md)
resolved and ran against vendored pytest 9.1.1. They are **known-good
floors to aim for, not proven minimums**: older releases were not tested,
and the corpus installs plugins unpinned, so a newer corpus run may resolve
newer versions. Every corpus run records the versions it installed, and the
rows are regenerated from that data with `python3 corpus/plugin_versions.py`.
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

Two plugins in this stack are terminal/report-owned and go quiet under the
pool. This is not breakage. It is rstest owning a single merged terminal
and having no Python master to aggregate worker output.

- **pytest-html: the report writer.** At `-n ≥ 2` no report is written:
  pytest-html registers its writer only on a node *without* `workerinput`
  (its xdist "am I the master?" check), and every rstest pool worker carries
  a `workerinput`, so nothing ever owns report generation. Merging all
  workers into one file needs a single master process rstest doesn't run.
  Two good paths:
  - Keep the fast parallel run and emit from merged artifacts: native
    `--html` (a self-contained report), `--junitxml` (for CI dashboards),
    or `--report-json` (render your own), all **intercepted by rstest and
    rendered from merged results** at any worker count. Nothing re-runs. See
    [Plugins](plugins.md).
  - Or, if pytest-html's *exact* layout is a hard requirement, run a
    dedicated `-n 0`/`-n 1` reporting pass and hand `--html` to pytest, not
    to rstest: `rstest -n 0 -- --html=report.html` (arguments after `--` go
    straight to the pytest session), or set it in `addopts`. A plain
    `rstest -n 0 --html report.html` still writes rstest's native report,
    because rstest owns a command-line `--html` at every worker count.
  - rstest **warns you automatically** when a parallel run is invoked with a
    flag whose plugin goes dark, see [Plugins](plugins.md). A `--html` that
    reaches pytest-html through `addopts` or after `--` is not caught by that
    check: at `-n ≥ 2` it silently writes nothing.

- **pytest-sugar: the progress UI.** Its terminal rendering is not painted
  at `-n ≥ 2` because rstest owns the terminal. Its non-visual behavior is
  unaffected; run at `-n 0` only when you specifically want its rendering
  ([Plugins](plugins.md)).

The rule of thumb: if a plugin's job is to *aggregate across workers from
the master* or *paint the terminal*, it wants `-n 0`. Everything else in
this stack (django, asyncio, hypothesis, cov, mock, freezegun) runs
parallel as-is.

## Go deeper

- [Plugins](plugins.md): how loading works, the tested-compatibility
  table, hook coverage, and the self-audit script for a home-grown reporter.
- [Top 100 plugin compatibility matrix](../reference/top-100-plugins.md):
  every plugin here plus 92 more, each with its verdict and V/i mark.
- [Plugins exercised by the corpus](../reference/corpus-plugins.md): the
  runtime inventory of which real suites load which plugins under rstest.
- [Compatibility](../concepts/compatibility.md): the parity contract, the
  vendored-pytest policy, and the known-gaps list.
- [Coverage](coverage.md): pytest-cov under the parallel pool, per-test
  contexts, and the diff-coverage gate.
