# CI quickstart

rstest behaves like pytest in CI: exit-code discipline, JUnit XML for your
test-report integration, and `--report-json` for tooling, with quiet human
output (machine consumers should parse the files, not stdout).

On top of that, rstest adds two things worth wiring up in CI: worker
parallelism with no extra plugin, and a duration cache that makes scheduling
smarter when persisted between runs.

This page gets you running on **GitHub Actions** and walks two worked examples
(Django, monorepo). For other CI systems (AWS CodeBuild, Google Cloud Build,
GitLab, Azure, CircleCI, Jenkins, Buildkite, pre-commit), see [More CI
systems](ci-recipes.md). For a shard matrix that needs a cache no native CI
cache can merge, see [Shared cache across CI jobs](ci-shared-cache.md).

--8<-- "docs/_snippets/ci-pin-tip.md"

## Which layout do I want?

| Your situation | Layout | Where |
|---|---|---|
| Single project | one job, `rstest -n auto`, cache `.rstest_cache` | [GitHub Actions](#github-actions) below |
| Monorepo, **few** packages (≤ runner cores) | one root job, `cd repo && rstest`, merged report | [Monorepos guide](monorepo.md) |
| Monorepo, **many** packages | one job **per package** via a matrix | [Monorepo worked example](#worked-example-monorepo-on-ephemeral-ci) |
| One long suite you split across CI nodes | `--shard K/N` matrix + [shared cache](ci-shared-cache.md) | [Sharding](sharding.md) + [Shared cache](ci-shared-cache.md) |

The rule of thumb: **the unit of CI parallelism should be the project, not the
root** once you have more packages than runner cores. One job per package
gives each the full runner and its own cache. Reach for `--shard` only when a
*single* project's suite is itself the long pole.

## GitHub Actions

The quickest path is the bundled composite action, which wraps install, the
duration cache (correctly keyed), `--changed` base-ref handling, and an
optional fail-ratio gate:

```yaml
jobs:
  tests:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v7
      - uses: KovantAI/rstest/.github/actions/rstest@v0.9.0
        with:
          python-version: "3.13"
          args: "-n auto"
          upload-junit: true
```

!!! note "Use `@v0.8.0` or later"
    Action tags before `v0.8.0` interpolate some inputs straight into their
    shell steps and lack the `warm-from-event` guard on the artifact cache
    backend. See
    [Security: GitHub action inputs](../reference/security.md#github-action-inputs).

That defaults `--output github` (so failures show as `::error` annotations and
flaky reruns as `::warning`), persists `.rstest_cache` across runs, and writes
`junit.xml`. See the [action README][action] for all inputs (`changed`,
`durations-regress`, `reruns`/`rerun-on`, `fail-under-ratio`, `shard`, …).

Pin the action to a release tag (`v0.8.0` or later) or a full commit SHA,
and set `version:` to pin the rstest wheel; without it the action installs the
latest rstest from PyPI. Under `runner: uv` (the default when the project has a
`uv.lock` or `[tool.uv]`) `version:` is ignored and rstest comes from your
lockfile, so pin it there.

The YAML on these pages references third-party actions by major tag
(`actions/checkout@v7`) for readability. If your security policy requires it,
pin those by full commit SHA too; see [Security & supply
chain](../reference/security.md).

In a matrix, the action names its artifacts per leg (the `artifact-suffix`
input, default `<os>-py<version>[-<working-directory>]`) so legs never share
cache segments or JUnit names. See the [action README][action].

[action]: https://github.com/KovantAI/rstest/tree/main/.github/actions/rstest

### Under the hood

The action is a thin wrapper. If you prefer raw YAML (or need something the
action does not expose), the equivalent steps are:

```yaml
jobs:
  tests:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v7
      - uses: actions/setup-python@v7
        with:
          python-version: "3.13"

      - name: install
        run: |
          pip install -r requirements.txt
          pip install rstest

      --8<-- "docs/_snippets/actions-cache-step.md"
      - name: test
        run: rstest -n auto --output github --junitxml junit.xml

      - uses: actions/upload-artifact@v7
        if: always()
        with:
          name: junit
          path: junit.xml
```

`--output github` emits an `::error` annotation per failure and a `::warning`
per flaky rerun. Add `--doctor` to also publish
[suite diagnostics](doctor.md) to the job summary.

Run from a monorepo root, the same steps need two changes: each project keeps
its own `.rstest_cache`, so widen the cache `path` to `**/.rstest_cache` (and
exclude `!**/.rstest_cache/replay`), and JUnit is written per project as
`junit.<slug>.xml`, so glob `**/junit.*.xml` in the upload step.

This `actions/cache` step is for a single job. If one suite is the long pole
and you fan it across a runner matrix with `--shard K/N`, use the action with
`cache-backend: artifact` and one `warm-run-id` shared by every shard: see
[Sharding: GitHub Actions](sharding.md#github-actions).

## Worked example: Django on ephemeral CI

A Django suite is the common case: pytest-django, a real database, ephemeral
GitHub runners where nothing survives between runs unless you persist it. The
two things people get wrong are the **cold-vs-warm cache** and **per-worker
databases**. Both are handled below.

rstest supplies each worker the xdist-style worker identity pytest-django
keys off, so with a server database every worker should create its own test
DB (`test_app_gw0`, `test_app_gw1`, …) with no extra flags, as under xdist.
rstest's own pytest-django coverage is
[SQLite only](../reference/corpus-plugins.md#what-pytest-djangos-evidence-covers), so confirm this with one
parallel run against your database before you rely on it.

```yaml
# .github/workflows/tests.yml
jobs:
  tests:
    runs-on: ubuntu-latest
    services:
      postgres:
        image: postgres:16
        env: { POSTGRES_USER: ci, POSTGRES_PASSWORD: ci, POSTGRES_DB: app }
        ports: ["5432:5432"]
        # createdb privilege matters: each worker CREATEs its own test DB
        options: >-
          --health-cmd "pg_isready -U ci" --health-interval 5s
          --health-timeout 5s --health-retries 5
    env:
      DJANGO_SETTINGS_MODULE: myapp.settings.test
      DATABASE_URL: postgres://ci:ci@localhost:5432/app
    steps:
      - uses: actions/checkout@v7
      - uses: actions/setup-python@v7
        with: { python-version: "3.13" }
      - run: pip install -r requirements.txt && pip install rstest

      --8<-- "docs/_snippets/actions-cache-step.md"
      - run: rstest -n auto --reuse-db --output github --junitxml junit.xml

      - uses: actions/upload-artifact@v7
        if: always()
        with: { name: junit, path: junit.xml }
```

`--reuse-db` keeps the migrated test database across runs on a warm
workspace. On ephemeral runners the database is fresh every run, so the flag
does nothing there; it is harmless to leave in and pays off on self-hosted
runners.

**What the two runs look like.** Duration-aware scheduling needs one run of
timing data, so the first run on a fresh cache key is *cold*: the scheduler
has no per-test durations and falls back to an even split. The second run
(and every run after, as long as the cache restores) is *warm*: it starts the
slowest tests first and packs workers tightly.

Concretely, on the runnable
[`examples/ci-bench`](https://github.com/KovantAI/rstest/tree/main/examples/ci-bench)
suite (161 wait-bound tests with duration skew), **measured**, `-n 4`, best of
3, Apple Silicon / CPython 3.13:

<!-- SOURCE OF TRUTH: examples/ci-bench/README.md, keep numbers in sync -->
| config | wall | vs pytest |
|---|---|---|
| pytest (serial) | 12.1s | 1.0× |
| rstest cold (`-n 4`, no cache) | 5.3s | 2.3× |
| rstest warm (`-n 4`, cached durations) | 3.6s | 3.3× |

Cold already wins from parallelism; warm adds ~1.5× on top by scheduling the
long pole first. That is a **synthetic** wait-bound example, not a Django app.
rstest ships no canonical Django timing, and a suite's win depends on its own
shape (see the self-check table in the
[README](https://github.com/KovantAI/rstest#will-rstest-speed-up-your-suite)).
To get *your* real numbers before committing, run [`rstest try`](../reference/cli-commands.md#try)
locally. It runs your suite under plain pytest and under `rstest -n auto`,
diffs outcomes, and reports the speedup, with no migration.

!!! warning "Ephemeral runners: warm the cache from your default branch"
    If PR jobs start from a cold cache every time, you only ever pay cold-run
    cost. Run this workflow on pushes to your default branch too: GitHub lets
    PR jobs restore the base branch's cache entries, so a single-job PR suite
    restores a warm `.rstest_cache` with `actions/cache` alone. What a PR job
    saves is scoped to that PR and never replaces the default branch's entry.
    A shard matrix is different: it needs the
    [shared-cache backend](ci-shared-cache.md) so every shard partitions from
    the same snapshot.

## Worked example: monorepo on ephemeral CI

Two monorepo constraints collide in ephemeral CI, and the fix resolves both
at once:

1. **Concurrency.** At the root, every project launches concurrently with at
   least one worker each ([Monorepo mode](../concepts/monorepo.md#worker-budget-and-scheduling)).
   Many packages on a small (2–4 core) runner oversubscribes: 20 packages on
   a 2-core runner is 20 concurrent single-worker children fighting for 2 cores.
2. **Shared cache.** `--cache-pull` and `--cache-push` are **refused at a
   monorepo root** (exit 1), and `--cache-remote` alone is inert there
   ([CLI](../reference/cli.md#-cache-remote-urldir-cache-pull-cache-push)):
   each project keeps its own `.rstest_cache`, so the segment-merge shared
   cache is a per-project feature.

**Both dissolve if you make the project the unit of CI parallelism**: one
job per package via a matrix, instead of one root job running everything
concurrently. Each job runs the bundled action **inside** its package
(`working-directory`), so it is a plain single-project run: the package's
own `[tool.rstest]`, lockfile or `.venv`, and `.rstest_cache` apply, it gets
the runner's *full* core count with no oversubscription, and it can use the
[shared cache](ci-shared-cache.md) normally:

```yaml
# .github/workflows/tests.yml
permissions: { contents: read, actions: read }
jobs:
  discover:
    runs-on: ubuntu-latest
    outputs:
      projects: ${{ steps.list.outputs.projects }}
    steps:
      - uses: actions/checkout@v7
      # Emit the matrix from your project layout. Keep this list in sync with
      # [tool.rstest] projects in the root pyproject.toml (single source of truth).
      - id: list
        run: |
          echo 'projects=["libs/core","libs/cli","services/api"]' >> "$GITHUB_OUTPUT"

  test:
    needs: discover
    runs-on: ubuntu-latest
    strategy:
      fail-fast: false
      matrix:
        project: ${{ fromJSON(needs.discover.outputs.projects) }}
    steps:
      - uses: actions/checkout@v7
      - uses: KovantAI/rstest/.github/actions/rstest@v0.9.0
        with:
          python-version: "3.13"
          # Run inside the package, not `rstest libs/core` from the root:
          # a root-relative path would skip the package's own [tool.rstest]
          # and its .venv.
          working-directory: ${{ matrix.project }}
          # Per-package segment-merge shared cache over GitHub artifacts.
          # Artifact names carry the package (artifact-suffix defaults to
          # <os>-py<version>-<working-directory>, e.g. Linux-py3.13-libs-core),
          # so packages never warm from each other's segments.
          cache-backend: artifact
          # Non-uv package: install its dependencies here. For a uv package,
          # delete this line: a set `install:` replaces the `uv sync --dev`
          # the action would otherwise run, while tests still run under uv.
          install: pip install -r requirements.txt rstest
          args: "-n auto"
          upload-junit: true
```

Each package is its own job. It gets the whole runner, warms its own cache
segments from the latest green run on `main` (cold on run one, warm from run
two, exactly like the single-suite case), and uploads a fresh segment plus
its JUnit under per-package artifact names. Isolation is free (matrix jobs
don't share a runner), and a slow package no longer steals workers from a
fast one. Each leg resolves its warm run on its own; that is fine here,
because legs are different packages and never merge each other's segments
(a [shard matrix](sharding.md#keep-one-cache-snapshot-across-the-matrix) of
one suite is different). The action only warms from green runs, so while
`main` is red every leg keeps warming from the last green one. The
segment-merge mechanics are in [Shared cache across CI jobs](ci-shared-cache.md).

If a package is a member of a root uv workspace (one `uv.lock` at the repo
root, none in the package), set `runner: uv` so the action doesn't fall
back to pip. Point `working-directory` at a package, never at the monorepo
root itself: the action caches `<working-directory>/.rstest_cache` and
uploads a single JUnit file, which is not what a root run writes. The action
refuses a monorepo root with an error that names the subprojects.

!!! note "When to keep the root run instead"
    If your packages are **few** (roughly ≤ the runner's core count) the root
    `cd repo && rstest` from the [Monorepos guide](monorepo.md) is simpler:
    one job, one merged `--report-json`, per-project `junit.<slug>.xml` (glob
    `**/junit.*.xml`), and `.rstest_cache` persisted via `actions/cache` on
    `**/.rstest_cache` (with `!**/.rstest_cache/replay` excluded). It also
    keeps `--changed`'s cross-package skip logic in one place. Reach for the
    matrix above when project count outgrows the runner, or when you want the
    segment-merge shared cache per package. A project-level concurrency cap
    for the root case is on the roadmap ([Monorepo
    mode](../concepts/monorepo.md#worker-budget-and-scheduling)).

## Notes

- **Exit codes** follow pytest's vocabulary (0 pass, 1 failures, 5 nothing
  collected, ...), merged across workers. The CI gotcha: when rstest itself
  rejects a run (a bad flag combination, no usable interpreter) it also exits
  **1**, the same as test failures, so check the log or the report file, not
  just the code. A malformed command line (a flag missing its value, `-n -5`)
  exits **2**, the same as an interrupted run. The full table and each gating
  flag's codes are in [Exit codes](../reference/exit-codes.md).
- **Nothing affected, no reports.** A single-project `--changed` run that
  selects no tests exits before running and writes no `--junitxml` or
  `--report-json`. Set `if-no-files-found: ignore` on artifact uploads and make
  report steps tolerate a missing file. See
  [Selecting changed tests](changed.md#ci-usage) (including the monorepo-root
  behavior).
- **Reports.** `--junitxml` is rendered from merged results; point your CI's
  test-report integration at it as you would pytest's. `--report-json` is a
  per-test outcome snapshot with a stable schema, for tooling.
  [`--output github`](../reference/cli.md#output)
  adds `::error` annotations so failures appear inline on the PR diff.
- **Worker count.** `-n auto` starts from the runner's available cores
  (honoring the CPU affinity mask and cgroup quota on Linux), then caps by
  test files and cached test time; see
  [`-n`](../reference/cli.md#-n-numprocesses-nauto). With `--shard`, pin
  `-n 2` or more ([Sharding](sharding.md)).
- **Containers / Kubernetes.** A pod with a CPU *request* but no *limit* has
  no quota, so `auto` counts every core on the node: set `-n` to the CPU
  request. Each worker is its own Python process, so budget roughly one
  worker's peak memory × `-n` against the memory limit. An OOM-killed worker
  is reported like any worker crash
  ([Crash handling: budgets](../concepts/crash-handling.md#budgets)); a job
  full of crash failures on a memory-limited runner usually means `-n` is too
  high.
- **Timeouts.** A hung test otherwise runs until the CI job limit (6 hours
  on GitHub). Set a per-test [`--timeout SECS`](../reference/cli.md#-timeout-secs)
  and a job-level cap such as `timeout-minutes:`. When that cap cancels the
  job, a parallel run names the test each worker was running, reports it
  failed, writes the replay journal and any `--junitxml`/`--report-json`, and
  exits 2.
- **Colors** are disabled automatically when output is not a terminal;
  force with `--color=yes` (or `FORCE_COLOR=1`) if your CI renders ANSI.
- **Platform.** These recipes work on `macos-latest` and `windows-latest`.
  On Windows, `--timeout` is enforced only by the hang watchdog, a cancelled
  job writes no journal or reports, and bash-syntax `run:` steps need
  `shell: bash` ([Running on Windows: CI tips](windows.md#ci)).

## Go deeper

- [More CI systems](ci-recipes.md): AWS CodeBuild, Google Cloud Build,
  GitLab, Azure, CircleCI, Jenkins, Buildkite, and pre-commit.
- [Shared cache across CI jobs](ci-shared-cache.md): the segment-merge cache
  for a shard matrix.
- [Sharding across CI jobs](sharding.md): how partitions are computed and the
  identical-cache-snapshot rule.
- [Replaying a CI failure locally](replay.md): every parallel run records its
  per-worker schedule to `.rstest_cache/replay/latest.json`; upload it when a
  job fails and `rstest replay` re-runs that schedule on your machine.
- [Suite-health trending in CI](doctor.md#suite-health-trending-in-ci): archive
  `--doctor-json` per run and compare each PR against main in the job summary.
- [The migrate-check preflight](migrate-from-pytest.md#the-migrate-check-preflight):
  while a suite is still migrating, a `rstest migrate-check` job keeps new
  parallel-unsafe tests from landing green.
