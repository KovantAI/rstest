# Monorepos

A Python monorepo (many packages, each with its own pytest
configuration and test tree) is something pytest cannot run from the
root: one invocation means one rootdir, one ini file, and colliding
conftest trees. The usual workaround is N serial pytest invocations
with no shared scheduling and no merged result.

rstest runs the whole repo in one command:

```console
$ cd my-monorepo && rstest
rstest 0.7.0 — monorepo: 3 projects, 8 workers (libs/cli:-n2, libs/core:-n4, services/api:-n2)

=============== project: libs/core ===============
...
=============== monorepo summary ===============
  libs/cli                                 ok
  libs/core                                ok
  services/api                             FAILED (exit 1)
3 projects in 41.20s (exit 1)
```

## How projects are found

Run `rstest` from a root that has **no pytest configuration of its own**:
rstest finds each package by its pytest config and runs them all. To
restrict or pin the set, list globs in the root `pyproject.toml`:

```toml
[tool.rstest]
projects = ["libs/*", "services/api"]
```

Passing an explicit path (`rstest libs/core`) opts out of monorepo mode and
runs that project alone, exactly as before. The full discovery rules (search
depth, which config files count, what's pruned) are in
[Monorepo mode](../concepts/monorepo.md#discovery).

## How it runs

Each project is an isolated child run, and projects run concurrently under
one worker budget weighted by each project's last-known suite time, so a repo
dominated by one package finishes in roughly that package's wall time. (How
isolation and the budget split work:
[Session isolation](../concepts/monorepo.md#session-isolation) and
[Worker budget and scheduling](../concepts/monorepo.md#worker-budget-and-scheduling).)

```console
$ rstest          # langgraph monorepo, 14-core machine
rstest 0.7.0 — monorepo: 6 projects, 14 workers (libs/langgraph:-n9, libs/checkpoint:-n1, libs/cli:-n1, libs/sdk:-n1, libs/prebuilt:-n1, libs/checkpoint-sqlite:-n1)
...
6 projects in 245.7s   # cold run; six serial pytest invocations: 880.4s (3.6×)
```

The 245.7s figure is the measured cold (first) run. A warm run (planned
from the duration caches the first run writes) is projected at 121–133s
(6.6–7.3×); see [Benchmarks](../reference/benchmarks.md#monorepo).

What to set up and expect:

- **Per-project settings.** A project can pin its own `[tool.rstest]`, e.g.
  `numprocesses = 0` for an order-sensitive package; root command-line flags
  override everywhere.
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

## Validation

The reference target is langchain-ai/langgraph: 8 `libs/*` packages,
each with its own `[tool.pytest]` config. rstest at the repo root
discovers all 8 (the JS package, which has no Python config, is
correctly skipped). The measured subset below is the six libs that need no live
services; the other two (postgres-backed checkpoint stores) require a
running database under any runner. One command at the root replaces six
serial pytest invocations and cuts wall time several-fold, with per-project
outcomes matched to the digit, including the dominant package's
fail/pass/error signature, which its service-dependent tests produce
identically under vanilla pytest. The corpus run measured 100% per-test
parity across all 4,284 tests. The one fragile spot is a TTL timing test
that langgraph's own source marks `@pytest.mark.flaky`; it lives in
`checkpoint-sqlite`, a small suite the corpus runs in byte-exact mode (`-n 0`).
That pin was once forced by a pytest-retry limitation (`server_port`); it is
now resolved: pytest-retry runs its `@pytest.mark.flaky` marker correctly
under the pool too (see [Benchmarks](../reference/benchmarks.md#monorepo) for
the wall times and the policy).
