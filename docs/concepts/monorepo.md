# Monorepo mode

How rstest runs a multi-package repo as one command. For the task-oriented
quickstart — running it, pinning the project set, environments, tox/nox — see
the [Monorepos guide](../guides/monorepo.md). This page is the reference for
*what the mode guarantees and how each flag behaves* across projects.

## Discovery

Monorepo mode engages when the current directory has **no pytest
configuration of its own** but subdirectories do. Discovery descends at most
five directory levels below the root (projects nested deeper are not found —
list them in `projects` explicitly), looking for any of pytest's config files
(`pytest.ini`, `pyproject.toml` with `[tool.pytest.ini_options]`, `tox.ini`,
`setup.cfg`) — a `pyproject.toml` without a pytest section does not count.
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

Each project runs as its own full session group — an isolated child run with
the working directory set to the project, so rootdir, ini options, and
conftest loading behave exactly like running pytest inside that directory.
There is no cross-project fixture or conftest leakage *by construction*, and
caches (durations, lastfailed) live in each project where they belong.

Per-project `[tool.rstest]` settings are honored: a project that pins
`numprocesses` keeps it — `numprocesses = 0` runs that project in
single-worker [byte-exact mode](glossary.md#byte-exact-mode) (the escape
hatch for order-sensitive suites) while its siblings split the remaining
budget. `dist`, `reruns`, and `worker-timeout` set in a project apply to that
project; flags given on the root command line override everywhere.

## Worker budget and scheduling

Projects run **concurrently** under one worker budget: your `-n` (or `auto`)
is split across projects weighted by each project's last-known suite time (its
duration cache), minimum one worker each. A repo where one package dominates
finishes in roughly that package's own wall time — the small ones ride along
on spare workers.

First runs (no duration caches yet) split the budget evenly; from the second
run on, the weights kick in. Output is printed per project, in completion
order, each block whole.

**Scale note:** every project gets at least one worker and all projects launch
concurrently — a 40-package repo on a 2-core CI runner means 40 concurrent
single-worker children, which oversubscribes. On small runners, shard with
`[tool.rstest] projects` (or path arguments) until a project-level concurrency
cap exists.

## Output and artifacts

- **Exit code** is the merge of per-project exits (pytest semantics: failures
  dominate; "no tests collected" only if every project says so). See
  [Exit codes](../reference/exit-codes.md).
- **`--report-json`** writes ONE merged document at the requested path: test
  keys are root-relative nodeids (`libs/core/tests/test_x.py::test_y` — what
  pytest would call them from the root), `meta.exitstatus` is the merged exit,
  and `meta.projects` maps each project to `{"exitstatus": N, "counts": {...}}`
  or `{"skipped": true}` (skipped by `--changed`). No globbing, no client-side
  merging. See [Report JSON](../reference/report-json.md) for the exact shape.
- **`--junitxml` and `--doctor-json`** are written per project with the
  project slug inserted before the extension (JUnit consumers want one
  testsuite file per package). The slug is the project's path relative to the
  root with separators replaced by `-`: `libs/core` -> `junit.libs-core.xml`,
  `services/api` -> `junit.services-api.xml`. Files anchor at the invocation
  directory. A project skipped by `--changed` writes no files.
- **Output files are written as each project finishes** (each project is an
  isolated child run) — a hang in one package does not cost you the completed
  packages' JUnit/report files.
- **`--output` style** is forwarded to every project, so `dots`, `verbose`,
  `bar`, and `github` all apply per package (each project's block is captured
  and reprinted under its header; `github` `::error` annotations are rewritten
  with the project's root-relative path so they land on the right file in the
  PR diff). `--output json` is **refused** at a monorepo root — the
  per-project banners make a single clean NDJSON stream impossible; use the
  merged `--report-json` document, or run `--output json` inside one project.
- **Coverage** works per project: workers write their data files in each
  project's directory and the combined report renders inside that project's
  output block; projects cannot cross-contaminate (verified by the test gate).
- **`--pdb` / `-s` / `--collect-only`** need a single pytest session: run them
  inside one project.

## Changed-aware runs

`--changed` is monorepo-aware. Changed files are classified once at the root:
projects containing changes run with `--changed` (their own import graph
narrows further); projects *depending* on a changed project — via
`[project].dependencies`, optional dependencies, or `[dependency-groups]`,
transitively — run their full suite (their own files didn't change, so there
is nothing to narrow by); everything else is **skipped** outright. Changes
outside every project (root configs, shared scripts) conservatively run
everything in full. Dependency-group edges count on purpose: a package whose
dev group installs a sibling runs that sibling's code in its tests.

Plain `--changed` edges come from **declared** metadata only — a package
importing a sibling without declaring it would be skipped incorrectly. For
gating (merge queues), use
[`--changed-strict`](../reference/cli.md#-changed-strict): it scans each
project's imports and counts undeclared sibling imports as edges (warning
loudly), forces a full run for any changed file the graph can't connect to a
test, and exits 5 when nothing ran. The one residual hole is imports built
from runtime strings — if your repo does that across packages, keep full runs
on the gating path.
