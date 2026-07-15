# Monorepos

A Python monorepo — many packages, each with its own pytest
configuration and test tree — is something pytest cannot run from the
root: one invocation means one rootdir, one ini file, and colliding
conftest trees. The usual workaround is N serial pytest invocations
with no shared scheduling and no merged result.

rstest runs the whole repo in one command:

```console
$ cd my-monorepo && rstest
rstest 0.2.1 — monorepo: 3 projects, 8 workers (libs/cli:-n2, libs/core:-n4, services/api:-n2)

=============== project: libs/core ===============
...
=============== monorepo summary ===============
  libs/cli                                 ok
  libs/core                                ok
  services/api                             FAILED (exit 1)
3 projects in 41.20s (exit 1)
```

## How projects are found

Monorepo mode engages automatically when the current directory has **no
pytest configuration of its own** but subdirectories do — rstest discovers
each package by its pytest config and runs them all. To restrict or pin the
set, list globs in the root `pyproject.toml`:

```toml
[tool.rstest]
projects = ["libs/*", "services/api"]
```

Passing an explicit path (`rstest libs/core`) opts out of monorepo mode and
runs that project alone, exactly as before. The full discovery rules (search
depth, which config files count, what's pruned) are in
[Monorepo mode](../concepts/monorepo.md#discovery).

## How it runs

Each project is an isolated child run — its own rootdir, ini, and conftests,
no cross-project leakage. Projects run **concurrently** under one worker
budget, split across them by each project's last-known suite time, so a repo
dominated by one package finishes in roughly that package's wall time:

```console
$ rstest          # langgraph monorepo, 14-core machine
rstest 0.2.1 — monorepo: 6 projects, 14 workers (libs/langgraph:-n9, libs/checkpoint:-n1, libs/cli:-n1, libs/sdk:-n1, libs/prebuilt:-n1, libs/checkpoint-sqlite:-n1)
...
6 projects in 245.7s   # cold run; six serial pytest invocations: 880s (3.6×)
```

The 245.7s figure is the measured cold (first) run. A warm run — planned
from the duration caches the first run writes — is projected at 121–133s
(6.6–7.3×); see [Benchmarks](../reference/benchmarks.md#monorepo).

A project can pin its own `[tool.rstest]` (`numprocesses = 0` for byte-exact
mode, its own `dist`/`reruns`/`worker-timeout`); root command-line flags
override everywhere. Results merge into one exit code and one
`--report-json`; JUnit/coverage are written per project. `--changed` is
monorepo-aware — it skips packages no change can reach. For the exact
per-flag behavior (exit merge, report-json shape, JUnit slug rules,
`--changed` dependency edges, `--output json` refusal), see
[Monorepo mode](../concepts/monorepo.md).

## Environments

Projects share the active virtualenv by default — the uv-workspace
layout, and editable installs of sibling packages into a single venv,
both work naturally. A project with its **own `.venv`** automatically
uses it (the project-local interpreter beats the inherited
environment); an explicit `--python` overrides everything.

## tox / nox

rstest replaces the pytest *invocation*, not the environment manager:
inside a tox or nox env, `rstest` works as a drop-in for the `pytest`
command (workers use that env's interpreter). Replacing the matrix
itself — one rstest invocation spanning multiple Pythons — is not
supported; keep the matrix in tox/CI and put rstest inside each cell.

## Validation

The reference target is langchain-ai/langgraph: 8 `libs/*` packages,
each with its own `[tool.pytest]` config. rstest at the repo root
discovers all 8 (the JS package is correctly skipped — no Python
config). The measured subset below is the six libs that need no live
services; the other two (postgres-backed checkpoint stores) require a
running database under any runner. One command at the root replaces six
serial pytest invocations and cuts wall time several-fold, with per-lib
outcomes matched to the digit — including the dominant package's
fail/pass/error signature, which its service-dependent tests produce
identically under vanilla pytest. The corpus run measured 100% per-test
parity across all 4,284 tests. The one fragile spot is a TTL timing test
that langgraph's own source marks `@pytest.mark.flaky`; it lives in
`checkpoint-sqlite`, which the corpus pins to `-n 0` so pytest-retry runs
that marker on its non-xdist path (see
[Benchmarks](../reference/benchmarks.md#monorepo) for the wall times,
the policy, and the pytest-retry gap behind it).
