# Guides

Task-oriented how-tos. Start with the one that matches what you are doing.

## Migrating

- [Migrating from pytest](migrate-from-pytest.md): what changes when you switch, and how to fall back
- [Upgrading to pytest 9](upgrade-to-pytest9.md): rstest vendors pytest 9.1.1; how to get a suite written for older pytest green on it
- [Migrating from pytest-xdist](migrate-from-xdist.md): flag map, differences, and the CPU-bound case

## Running tests well

- [Parallel safety](parallel-safety.md): rails for tests that can't parallelize, plus worker-count, memory and BLAS-thread sizing
- [Suite diagnostics](doctor.md): reading `--doctor`
- [Wait-bound / IO suites](wait-bound.md): suites dominated by sleeps, network, and timeouts
- [Local dev / inner loop](local-dev.md): fast edit-run cycles on your machine
- [Watch mode](watch-mode.md): the edit loop
- [Debugging a test](debugging.md): `breakpoint()`/pdb, attaching VS Code or PyCharm with `--debug`
- [Selecting changed tests](changed.md): `--changed` via import graph or coverage index
- [Running only what changed since green](since-green.md): `--since-green` (diff against the last green commit) and `--incremental` (skip unchanged green tests, no git)
- [Flaky tests](flaky-tests.md): reruns, flake history, quarantine
- [Catching slowdowns](slowdowns.md): fail CI when a test or the whole suite gets slower
- [Resource leaks](resource-leaks.md): detecting and gating tests that leak threads and file descriptors
- [Replaying a CI failure locally](replay.md): re-run a failed CI job's per-worker schedule with `rstest replay`
- [Monorepos](monorepo.md): one command across a multi-package repo
- [Running on Windows](windows.md): install, what differs (timeouts, Ctrl+C, no fork pool), validation level and CI tips

## CI

- [CI quickstart](ci-quickstart.md): GitHub Actions, Django and monorepo worked examples, doctor trending
- [CI output formats](ci-output.md): `--output` styles for CI annotations and machine-readable streams
- [More CI systems](ci-recipes.md): GitLab CI, Azure Pipelines, CircleCI, Jenkins, Buildkite, AWS CodeBuild, Google Cloud Build, pre-commit
- [Shared cache across CI jobs](ci-shared-cache.md): merging durations, flakes, and coverage across jobs
- [Sharding across CI jobs](sharding.md): splitting one suite over N machines with `--shard K/N`

## Plugins and coverage

- [Your plugin stack](plugin-stack.md): checking the plugins you depend on before switching
- [Plugins](plugins.md): how plugin loading works, what's verified
- [Coverage](coverage.md): pytest-cov under parallel workers

## Coding agents

- [Agent skills](agent-skills.md): install the `migrate-to-rstest` and `rstest-triage` skills for Claude Code, Codex, and other agents
