# Monorepos

A Python monorepo (many packages, each with its own pytest
configuration and test tree) is something pytest cannot run from the
root: one invocation means one rootdir, one ini file, and colliding
conftest trees. The usual workaround is N serial pytest invocations
with no shared scheduling and no merged result.

rstest runs the whole repo in one command:

```console
$ cd my-monorepo && rstest
rstest 0.8.0 — monorepo: 3 projects, 8 workers (libs/cli:-n2, libs/core:-n4, services/api:-n2)

=============== project: libs/core ===============
...
=============== monorepo summary ===============
  libs/cli                                 ok
  libs/core                                ok
  services/api                             FAILED (exit 1)
3 projects in 41.20s (exit 1)
```

## 1. Run from the root

Run `rstest` from a root that has **no pytest configuration of its own**:
rstest finds each package by its pytest config and runs them all. Passing an
explicit path (`rstest libs/core`) opts out of monorepo mode and runs that
project alone. The full discovery rules (search depth, which config files
count, what's pruned) are in
[Monorepo mode](../concepts/monorepo.md#discovery).

## 2. Pin the project set (optional)

To restrict or pin the set, list globs in the root `pyproject.toml`:

```toml
[tool.rstest]
projects = ["libs/*", "services/api"]
```

## 3. Check the worker split

Each project is an isolated child run, and projects run concurrently under
one worker budget weighted by each project's last-known suite time, so a repo
dominated by one package finishes in roughly that package's wall time. (How
isolation and the budget split work:
[Session isolation](../concepts/monorepo.md#session-isolation) and
[Worker budget and scheduling](../concepts/monorepo.md#worker-budget-and-scheduling).)

```console
$ rstest          # langgraph monorepo, 14-core machine, first run
rstest 0.8.0 — monorepo: 5 projects, 14 workers (libs/checkpoint:-n3, libs/checkpoint-sqlite:-n3, libs/cli:-n3, libs/prebuilt:-n3, libs/sdk-py:-n2)
...
```

Projects are listed in sorted path order. On a first run there are no
duration caches yet, so the 14 workers are split evenly, as above. Later runs
weight each project's share by its recorded suite time, so the slowest
package gets most of the workers.

A project can pin its own `[tool.rstest]`, e.g. `numprocesses = 0` for an
order-sensitive package; root command-line flags override everywhere.

## 4. Wire it into CI

- **Results.** One merged exit code and one `--report-json` at the root;
  JUnit and `--doctor-json` files per project (`junit.libs-core.xml`). Point
  your CI's test-report step at `junit.*.xml`. Exact rules:
  [Output and artifacts](../concepts/monorepo.md#output-and-artifacts).
- **PRs.** `--changed` skips packages no change can reach; use
  `--changed-strict` on gating paths. How the dependency edges are found:
  [Changed-aware runs](../concepts/monorepo.md#changed-aware-runs).
- **Small CI runners.** Every project gets at least one worker and all start
  at once, so many packages on a 2-core runner oversubscribe; split them with
  `projects` globs or path arguments, or make each package its own CI job
  ([CI quickstart](ci-quickstart.md)).

## Environments

Projects share the active virtualenv by default: the uv-workspace
layout, and editable installs of sibling packages into a single venv,
both work naturally. A project with its **own `.venv`** automatically
uses it (the project-local interpreter beats the inherited
environment); an explicit `--python` overrides everything.

## tox / nox

rstest replaces the pytest *invocation*, not the environment manager:
inside a tox or nox env, `rstest` works as a drop-in for the `pytest`
command (workers use that env's interpreter). Replacing the matrix
itself (one rstest invocation spanning multiple Pythons) is not
supported; keep the matrix in tox/CI and put rstest inside each cell.

## Measured

On five langchain-ai/langgraph `libs/*` packages, one root run replaces five
serial pytest invocations with 100% per-test outcome parity; setup and wall
times are in [Benchmarks](../reference/benchmarks.md#monorepo).
