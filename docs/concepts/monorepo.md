# Monorepo mode

How rstest runs a multi-package repo as one command. For the task-oriented
quickstart (running it, pinning the project set, environments, tox/nox), see
the [Monorepos guide](../guides/monorepo.md). This page is the reference for
*what the mode guarantees and how each flag behaves* across projects.

## Discovery

Monorepo mode engages when the current directory has **no pytest
configuration of its own** but subdirectories do. Discovery descends at most
five directory levels below the root (projects nested deeper are not found;
list them in `projects` explicitly), looking for any of pytest 9's config
files: `pytest.toml`, `.pytest.toml`, `pytest.ini`, `.pytest.ini`,
`pyproject.toml`, `tox.ini`, `setup.cfg`. The first four count even when
empty; `pyproject.toml` counts with a `[tool.pytest]` table (pytest 9's native
TOML form) or `[tool.pytest.ini_options]` (even empty), `tox.ini` with a
`[pytest]` section, and `setup.cfg` with `[tool:pytest]`. A `pyproject.toml`
without a pytest section does not count. (**Unreleased:** rstest 0.7.0 knew
only `pytest.ini` with a `[pytest]` section, `pyproject.toml` with
`[tool.pytest.ini_options]`, `tox.ini` and `setup.cfg`.)
Hidden directories, virtualenvs, `node_modules`, and `site-packages` are
pruned, and a found project owns its subtree (nested configs belong to it).

Restrict or pin the set with `projects` globs in the root `pyproject.toml`:

```toml
[tool.rstest]
projects = ["libs/*", "services/api"]
```

Passing an explicit path (`rstest libs/core`) opts out of monorepo mode and
runs that project alone.

## Session isolation

Each project runs as its own full session group: an isolated child run with
the working directory set to the project, so rootdir, ini options, and
conftest loading behave exactly like running pytest inside that directory.
There is no cross-project fixture or conftest leakage *by construction*, and
caches (durations, lastfailed) live in each project where they belong (see
[Caches per project](#caches-per-project)).

Per-project `[tool.rstest]` settings are honored: a project that pins
`numprocesses` keeps it, `numprocesses = 0` runs that project in
single-worker [byte-exact mode](glossary.md#byte-exact-mode) (the escape
hatch for order-sensitive suites) while its siblings split the remaining
budget. `dist`, `reruns`, and `worker-timeout` set in a project apply to that
project; flags given on the root command line override everywhere.

The root `pyproject.toml`'s `[tool.rstest]` contributes only `projects`. Its
other keys do not become defaults for the projects: the worker budget is the
root command line's `-n` (else `auto`), and `dist`, `order`, `output`,
`reruns` and `worker-timeout` are forwarded only from the root command line.
Each project's rstest reads the nearest `pyproject.toml` walking up from the
project, so a project with its own `pyproject.toml` uses only its own
`[tool.rstest]` (none means defaults), while a project without a
`pyproject.toml` (configured by `pytest.toml`, `pytest.ini`, `tox.ini` or
`setup.cfg`) walks up to the root `pyproject.toml` and uses the root's
`[tool.rstest]`.

## Worker budget and scheduling

Projects run **concurrently** under one worker budget: your `-n` (or `auto`)
is split across projects weighted by each project's last-known suite time (its
duration cache), minimum one worker each. A repo where one package dominates
finishes in roughly that package's own wall time: the small ones ride along
on spare workers.

First runs (no duration caches yet) split the budget evenly; from the second
run on, the weights kick in. Output is printed per project, in completion
order, each block whole.

**Scale note:** every project gets at least one worker and all projects launch
concurrently, so a 40-package repo on a 2-core CI runner means 40 concurrent
single-worker children, which oversubscribes. On small runners, shard with
`[tool.rstest] projects` (or path arguments) until a project-level concurrency
cap exists.

## Caches per project

Each project reads and writes its own cache (durations, flake history,
coverage index, last-green baseline), and the planner weights projects from
that same cache. With `RSTEST_CACHE` unset that is `<project>/.rstest_cache`.
With `RSTEST_CACHE` set, each project gets `<RSTEST_CACHE>/<slug>` (the slug
as for output files below: `libs/core` -> `libs-core`), so projects never
share one cache dir; a relative `RSTEST_CACHE` resolves against the monorepo
root, not each project. (**Unreleased:** rstest 0.7.0 handed every project
the same `RSTEST_CACHE`, so an absolute value made the projects share one
cache dir.) `--cache-pull`/`--cache-push` are refused at the root;
run rstest per project for a shared remote cache.

## Flags at a monorepo root

Every root flag is either forwarded to each project's rstest, forwarded with a
per-project output path, handled at the root, or refused with exit 1; no run
flag is silently dropped. Rows marked **Unreleased** are new since rstest 0.7.0 (0.7.0
dropped those flags at a monorepo root).

| Flag | At a monorepo root |
|---|---|
| `-n` | split into per-project shares (see [Worker budget](#worker-budget-and-scheduling)) |
| `--python`, `--dist`, `--order`, `--output` (except `json`/`tap`), `--reruns`, `--only-rerun`, `--quarantine`, `--worker-timeout`, `--doctor`, `--doctor-fail-on` | forwarded to every project (`--order` is itself Unreleased) |
| `--changed[=REV]`, `--changed-strict` | classified once at the root, then forwarded to the directly changed projects (see [Changed-aware runs](#changed-aware-runs)) |
| `--timeout`, `--collect`, `--incremental`, `--reruns-only-known-flaky` | forwarded to every project (**Unreleased**) |
| `--fail-on-leak`, `--durations-regress`, `--require-baseline` | forwarded; each gate applies per project, and a project that fails its gate fails the root through the merged exit code (**Unreleased**) |
| `--shuffle[=SEED]` | resolved once at the root, so every project uses the same seed. A bare `--shuffle` picks one and prints `rstest: shuffle seed <N> for every project (reproduce with --shuffle=<N>)`. Each project still needs `-n 2` or more, so a project whose share is one worker errors (**Unreleased**) |
| `--junitxml`, `--doctor-json`, `--doctor-md` | one file per project, slug before the extension (see below) |
| `--html` | one file per project, like `--junitxml`: `out.html` -> `out.libs-core.html` (**Unreleased**) |
| `--report-json` | one merged document at the requested path (see below) |
| `--cache-remote`, `--cache-compact-threshold` | handled at the root; inert without pull/push, which are refused |
| `--cache-pull`, `--cache-push` | refused: each project has its own cache |
| `--watch`, `--output json`, `--output tap` | refused: run inside one project (use `--report-json` or `--junitxml` for machine-readable results) |
| `-s`, `--capture=...`, `--pdb`, `--trace`, `--co`, stepwise flags | refused: they need a single pytest session |
| `--debug` | refused (**Unreleased**): `--debug needs a single pytest session; run it inside one project of this monorepo` |
| `--shard` | refused (**Unreleased**): shard buckets and `shard-verify` cover one project's collection; run `--shard` inside each project |
| `--cov-diff-fail-under`, `--cov-diff-json` | refused (**Unreleased**): diff coverage is scored against one project's coverage data; run them inside each project |
| `--stream-json` | refused (**Unreleased**): one live stream can't carry several concurrent project sessions; run it inside one project, or use `--report-json` |
| `--since-green` | refused unless `--changed` is also given (**Unreleased**): use `--changed=<rev>` at the root, or run `--since-green` inside a project |

## Output and artifacts

- **Exit code** is the merge of per-project exits (pytest semantics: failures
  dominate; "no tests collected" only if every project says so). See
  [Exit codes](../reference/exit-codes.md).
- **`--report-json`** writes **one** merged document at the requested path: test
  keys are root-relative nodeids (`libs/core/tests/test_x.py::test_y`, what
  pytest would call them from the root), `meta.exitstatus` is the merged exit,
  and `meta.projects` maps each project to `{"exitstatus": N, "counts": {...}}`
  or `{"skipped": true}` (skipped by `--changed`). No globbing, no client-side
  merging. See [Report JSON](../reference/report-json.md) for the exact shape.
- **`--junitxml`, `--doctor-json`, `--doctor-md` and `--html`** are written
  per project with the project slug inserted before the extension (JUnit
  consumers want one testsuite file per project). The slug is the project's path relative to the
  root with separators replaced by `-`: `libs/core` -> `junit.libs-core.xml`,
  `services/api` -> `junit.services-api.xml`. Files anchor at the invocation
  directory. A project skipped by `--changed` writes no files.
- **Output files are written as each project finishes** (each project is an
  isolated child run): a hang in one package does not cost you the completed
  packages' JUnit/report files.
- **`--output` style** is forwarded to every project, so `dots`, `verbose`,
  `bar`, and `github` all apply per project (each project's block is captured
  and reprinted under its header; `github` `::error` annotations are rewritten
  with the project's root-relative path so they land on the right file in the
  PR diff). `--output json` is **refused** at a monorepo root: the
  per-project banners make a single clean NDJSON stream impossible; use the
  merged `--report-json` document, or run `--output json` inside one project.
- **Coverage** works per project: workers write their data files in each
  project's directory and the combined report renders inside that project's
  output block; projects cannot cross-contaminate (verified by the test gate).
- **`--pdb` / `-s` / `--collect-only` / `--debug`** need a single pytest
  session: run them inside one project.

## Changed-aware runs

`--changed` is monorepo-aware. Changed files are classified once at the root:
projects containing changes run with `--changed` (their own import graph
narrows further); projects *depending* on a changed project (via
`[project].dependencies`, optional dependencies, or `[dependency-groups]`,
transitively) run their full suite (their own files didn't change, so there
is nothing to narrow by); everything else is **skipped** outright. Changes
outside every project (root configs, shared scripts) conservatively run
everything in full. Dependency-group edges count on purpose: a package whose
dev group installs a sibling runs that sibling's code in its tests.

Plain `--changed` edges come from **declared** metadata only: a package
importing a sibling without declaring it would be skipped incorrectly. For
gating (merge queues), use
[`--changed-strict`](../reference/cli.md#-changed-strict): it scans each
project's imports and counts undeclared sibling imports as edges (warning
loudly), forces a full run for any changed file the graph can't connect to a
test, and exits 5 when nothing ran. The one residual hole is imports built
from runtime strings: if your repo does that across packages, keep full runs
on the gating path.
