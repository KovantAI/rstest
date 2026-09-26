# Guides

Task-oriented how-tos. Start with the one that matches what you are doing.

## Use-case playbooks

- [Wait-bound / IO suites](wait-bound.md): suites dominated by sleeps, network, and timeouts
- [Local dev / inner loop](local-dev.md): fast edit-run cycles on your machine
- [Your plugin stack](plugin-stack.md): checking the plugins you depend on before switching

## Migrating

- [Migrating from pytest](migrate-from-pytest.md): what changes when you switch, and how to fall back
- [Upgrading to pytest 9](upgrade-to-pytest9.md): rstest vendors pytest 9.1.1; how to get a suite written for older pytest green on it
- [Migrating from pytest-xdist](migrate-from-xdist.md): flag map, differences, and the CPU-bound case

## Running tests well

- [Parallel safety](parallel-safety.md): rails for tests that can't parallelize
- [Suite diagnostics](doctor.md): reading `--doctor`
- [Resource leaks](resource-leaks.md): detecting and gating tests that leak threads and file descriptors
- [Flaky tests](flaky-tests.md): reruns, flake history, quarantine
- [Watch mode](watch-mode.md): the edit loop
- [Selecting changed tests](changed.md): `--changed` via import graph or coverage index
- [Monorepos](monorepo.md): one command across a multi-package repo

## CI

- [CI quickstart](ci-quickstart.md): GitHub Actions, Django and monorepo worked examples, doctor trending, migrate-check gating
- [More CI systems](ci-recipes.md): GitLab, Azure, CircleCI, Jenkins, cloud builders, pre-commit
- [Shared cache across CI jobs](ci-shared-cache.md): merging durations, flakes, and coverage across jobs
- [Sharding across CI jobs](sharding.md): splitting one suite over N machines with `--shard K/N`

## Plugins and coverage

- [Plugins](plugins.md): how plugin loading works, what's verified
- [Coverage](coverage.md): pytest-cov under parallel workers
