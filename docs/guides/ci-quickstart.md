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
      - uses: KovantAI/rstest/.github/actions/rstest@v0.8.0
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

      # Persist the duration cache: from the second run on, the scheduler
      # starts the slowest tests first.
      - uses: actions/cache@v6
        with:
          # Replay journals are per-run; upload them on failure instead
          # (see "Replaying a CI-only failure locally" below).
          path: |
            .rstest_cache
            !.rstest_cache/replay
          # Same components as the bundled action: OS + Python + lockfile
          # hash, so a 3.12 or Windows run never seeds a 3.13 Linux one.
          # Unique per run: actions/cache never RE-saves an existing key,
          # so a fixed key freezes the cache at its first run. restore-keys
          # picks the newest match (this branch first, then the base branch).
          key: rstest-${{ runner.os }}-py3.13-${{ hashFiles('requirements.txt') }}-${{ github.run_id }}
          restore-keys: |
            rstest-${{ runner.os }}-py3.13-${{ hashFiles('requirements.txt') }}-

      - name: test
        # --output github emits ::error per failure and ::warning for flaky
        # reruns. Add --doctor to also publish diagnostics to the job summary.
        run: rstest -n auto --output github --junitxml junit.xml

      # Long pole? Fan the suite across a runner matrix with --shard K/N;
      # see the Sharding guide.

      # Monorepo roots: caches live in EACH project (.rstest_cache per
      # package; widen the cache path to **/.rstest_cache and exclude
      # !**/.rstest_cache/replay), and junit
      # files are written per project as junit.<slug>.xml; glob them
      # in the artifact step.

      - uses: actions/upload-artifact@v7
        if: always()
        with:
          name: junit
          path: junit.xml
```

For a shard matrix, swap the `actions/cache` step for the segment-merge
[shared cache](ci-shared-cache.md). It sidesteps the `run_id` key dance and
lets every shard contribute its own segment. Keep every shard on the same
cache snapshot, though: see
[Keep one cache snapshot across the matrix](sharding.md#keep-one-cache-snapshot-across-the-matrix).

## Worked example: Django on ephemeral CI

A Django suite is the common case: pytest-django, a real database, ephemeral
GitHub runners where nothing survives between runs unless you persist it. The
two things people get wrong are the **cold-vs-warm cache** and **per-worker
databases**. Both are handled below.

pytest-django is exercised continuously in rstest's battery *including
per-worker test databases under parallelism* (see [Plugins](plugins.md)):
rstest supplies each worker the xdist-style worker identity pytest-django keys
off, so every worker gets its own isolated test DB (`test_app_gw0`,
`test_app_gw1`, …) automatically. No extra flags, same as under xdist.

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
      - run: pip install -r requirements.txt && pip install rstest==0.8.0

      # The cache is what makes run two fast. Keyed like the bundled action
      # (OS + Python + lockfile hash), unique per run (actions/cache never
      # re-saves an existing key); restore-keys picks the newest match.
      - uses: actions/cache@v6
        with:
          path: |
            .rstest_cache
            !.rstest_cache/replay
          key: rstest-${{ runner.os }}-py3.13-${{ hashFiles('requirements.txt') }}-${{ github.run_id }}
          restore-keys: |
            rstest-${{ runner.os }}-py3.13-${{ hashFiles('requirements.txt') }}-

      # --reuse-db keeps the migrated test DB across runs on a warm workspace;
      # on ephemeral runners the DB is fresh each time, so it's a no-op there
      # (harmless to leave in, useful on self-hosted runners).
      - run: rstest -n auto --reuse-db --output github --junitxml junit.xml

      - uses: actions/upload-artifact@v7
        if: always()
        with: { name: junit, path: junit.xml }
```

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
    cost. Run this workflow on pushes to your default branch too (GitHub lets
    PR jobs restore the base branch's cache entries), so PRs restore a warm
    `.rstest_cache` instead of rebuilding timing data from scratch. For a
    matrix/shard layout, prefer the [shared-cache backend](ci-shared-cache.md)
    (it sidesteps the `run_id` key dance entirely).

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
      - uses: KovantAI/rstest/.github/actions/rstest@v0.8.0
        with:
          python-version: "3.13"
          # Run inside the package, not `rstest libs/core` from the root:
          # a root-relative path would skip the package's own [tool.rstest],
          # its .venv, and its .rstest_cache.
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

## Suite-health trending with doctor

`--doctor-json` writes the doctor analysis as a versioned JSON document
(see [Suite diagnostics](doctor.md)). Archive it per run and compare a
PR's report against the main branch's. No extra tooling is required: the
document already contains totals, wait-bound tests, parallel-floor gate
tests, and fixture costs by name.

Any doctor run also publishes the report as markdown to the CI job
summary automatically (appended to `$GITHUB_STEP_SUMMARY` on GitHub
Actions, piped to `buildkite-agent annotate` on Buildkite), so the
current run's analysis is on the run page with no post-processing step.
(GitLab and TeamCity have no native markdown summary; use `--doctor-md`
and publish the file as an artifact.)

The baseline travels via the actions cache: pushes to main save it, PR
jobs restore it (GitHub lets PRs read the base branch's cache entries):

```yaml
      - name: test (with doctor)
        run: rstest -n auto --junitxml junit.xml --doctor-json doctor.json

      # Save the baseline on main; restore the latest one on PRs.
      - uses: actions/cache@v6
        with:
          path: doctor-baseline.json
          key: doctor-baseline-${{ github.sha }}
          restore-keys: doctor-baseline-

      - name: compare against main
        if: github.event_name == 'pull_request'
        run: |
          [ -f doctor-baseline.json ] || { echo "no baseline yet"; exit 0; }
          {
            echo "## Suite health vs main"
            jq -rn --slurpfile a doctor-baseline.json --slurpfile b doctor.json '
              def d(f): ($b[0][f] - $a[0][f]);
              "tests: \($a[0].tests) -> \($b[0].tests)",
              "test time: \($a[0].test_time_seconds|round)s -> \($b[0].test_time_seconds|round)s (\(d("test_time_seconds")|round)s)",
              "wait-bound: \($a[0].wait_bound.wait_pct // 0|round)% -> \($b[0].wait_bound.wait_pct // 0|round)%"
            '
            echo "new wait-bound tests:"
            comm -13 \
              <(jq -r '.wait_bound.tests[]?.nodeid' doctor-baseline.json | sort) \
              <(jq -r '.wait_bound.tests[]?.nodeid' doctor.json | sort) \
              | sed 's/^/- /' || true
          } >> "$GITHUB_STEP_SUMMARY"

      - name: refresh baseline
        if: github.ref == 'refs/heads/main'
        run: cp doctor.json doctor-baseline.json
```

Two practical notes:

- **Don't fail the job on timing deltas.** CI runners are noisy;
  single-digit-percent changes in `test_time_seconds` are jitter. Treat
  the summary as a review aid; alert only on structural signals (new
  wait-bound tests, a fixture's `count` doubling, a new parallel-floor
  gate test) or on large sustained moves.
- **Compare like with like.** `wall_seconds` depends on the worker
  count; if runner sizes vary, compare `test_time_seconds` (summed test
  time) and per-test signals instead.

## Gating new parallel-unsafe tests with migrate-check

[`migrate-check`](../reference/cli-commands.md#migrate-check) exits `1` when a
test has a run-to-run unstable id or fails only under parallelism (and `2`
when it couldn't judge, so a red job tells you which one to fix), so a
dedicated job keeps a migrating suite from regressing: no new co-location
leak, order dependency, or unstable-id site sneaks in green. Use
`--migrate-allow` to tolerate a triaged backlog so the gate fires only on
**new** issues, and `--migrate-check-json` to archive the findings
([schema](../reference/report-json.md#migrate-check-json)):

```yaml
      - name: migrate-check gate
        run: |
          rstest migrate-check --migrate-check-json migrate.json \
                 --migrate-allow tests/legacy/   # known-unsafe backlog, tolerated
      - uses: actions/upload-artifact@v7
        if: always()
        with:
          name: migrate-check
          path: migrate.json
```

This is heavier than a normal run (it collects twice and reruns the failing
files under discriminators), so run it on its own job or a schedule rather than
every push if the suite is large. Once the suite reports `ready`, drop the gate
and just run `rstest`.

## Replaying a CI-only failure locally

A test that fails on CI but passes on your machine is usually an ordering
problem: on CI it shared a worker with a test that leaked state, and locally
the scheduler put them apart. Every parallel run (`-n >= 2`) records which
worker ran which tests, in what order, to `.rstest_cache/replay/latest.json`.
Keep that file when a job fails and [`rstest replay`](../reference/cli-commands.md#replay)
re-runs the same schedule on your machine.

!!! warning "Sharded jobs record no journal"
    A run with `--shard` (or the action's `shard`/`shard-total` inputs) writes
    no journal, and neither do `-n 0`/`-n 1` and `--dist each`. To replay a failure from a sharded matrix, re-run the
    failing shard's tests unsharded with `-n` set to the CI worker count; that
    run records a journal you can replay.

**1. In CI, upload the journal when the tests fail.** With the bundled action,
give it an `id` and add one step after it:

```yaml
      - uses: KovantAI/rstest/.github/actions/rstest@v0.8.0
        id: rstest
        with:
          python-version: "3.13"
          args: "-n auto"
      - uses: actions/upload-artifact@v7
        if: always() && steps.rstest.outputs.exit-code != '0'
        with:
          name: rstest-replay-${{ github.job }}-${{ strategy.job-index }}
          path: .rstest_cache/replay/latest.json
          if-no-files-found: ignore
```

The condition reads the action's `exit-code` output, not `failure()`. With
`fail-under-ratio` set, the action's step can pass while tests failed, and
`failure()` would then skip the upload. `if-no-files-found: ignore` covers a
run that recorded nothing (for example `-n auto` resolving to one worker).

The job name and matrix index in the artifact name keep the uploads apart
when a matrix (Python versions, OSes) fails in several jobs at once:
`upload-artifact` (v4 and later) refuses a second artifact with the same name
in a run. Outside a matrix, `strategy.job-index` is `0`.

Where the journal lands:

- **With a `working-directory`**, prefix the path with it
  (`libs/core/.rstest_cache/replay/latest.json`).
- **At a monorepo root**, each project records its own journal inside the
  project directory. Upload them all with a glob and keep the directory
  layout, so you know which project each came from. `upload-artifact`
  skips dot-directories such as `.rstest_cache` when it expands a glob, so
  turn that off, or the step uploads nothing:

    ```yaml
          path: "**/.rstest_cache/replay/latest.json"
          include-hidden-files: true
    ```

    Replay from inside that project's directory, not from the root.

- **With `RSTEST_CACHE` set**, journals move with the cache:
  `$RSTEST_CACHE/replay/latest.json` for a single project, or
  `$RSTEST_CACHE/<slug>/replay/latest.json` per project at a monorepo root.

With raw YAML, put the same `upload-artifact` step after your `rstest -n auto`
step, with `if: failure()`. On other CI systems, save
`.rstest_cache/replay/latest.json` as a failure artifact the same way you
save `junit.xml`.

**2. Locally, check out the failing commit and download the journal.**

```console
$ git checkout <failing-sha>
$ pip install -r requirements.txt          # same deps as CI (use your lockfile)
$ gh run download <run-id> -n rstest-replay-tests-0 -D ci-replay   # job "tests", matrix index 0
```

Run `gh run download <run-id>` with no `-n` to fetch every artifact of the run
if you're unsure of the name.

**3. Replay it.**

```console
$ rstest replay --journal ci-replay/latest.json
rstest: replay: run 18d93d9429580fb015f7d (0.8.0 recorded), 4 worker(s), 22 test(s) across 4 slot(s)
rstest: replay: args: tests -k 'not slow'
...
--- FAILED [gw0] tests/test_m2.py::test_victim ---
```

Check the `args:` line before the tests start. It shows the pytest
arguments the CI run was given on the command line (paths, `-k`, `-p` and
so on), `(none)` if there were none. rstest's own flags such as `-n` are
not part of it, and neither are `addopts` or `PYTEST_ADDOPTS`, which
replay reads from your checkout. Replay hands the recorded arguments to
pytest as they are, so only replay journals from runs you trust
(see [Security: replay journals](../reference/security.md#replay-journals)).
To pick the interpreter, put `--python` after the subcommand:
`rstest replay --journal ci-replay/latest.json --python .venv/bin/python`.

Replay forces the recorded worker count (even on a laptop with fewer cores),
runs each worker's recorded tests in the recorded order, and turns off reruns
(`@pytest.mark.flaky` and `[tool.rstest] reruns` included), work stealing and
shuffling. `@pytest.mark.serial` tests still run alone, after every other
worker has finished, as they did on CI. A worker that crashes is replaced and
the replacement picks up where it died. The test args (paths, `-k`, `-m`) come
from the journal, so pass none. Recorded `--lf`/`--sw` are ignored with a note:
they would select by your local pytest cache, not CI's. The failing test's `[gwN]` tag tells you which worker
to look at: the tests that ran before it on that worker hold the likely
polluter. Once you have a fix, run the same command again. Green means the
fix holds for the schedule that broke CI.

Things that keep a journal portable:

- **Run from the same directory as CI.** Nodeids are relative to the rootdir,
  so in a monorepo replay from the package directory the CI job ran in.
- **Keep test paths inside the project.** Absolute paths under the directory
  rstest ran in (`$GITHUB_WORKSPACE/tests`) are stored relative to it, so they
  resolve on your checkout. A path outside it is stored as given; replay
  warns when such a path doesn't exist on your machine.
- **Match the code and dependencies.** If the suite changed since the
  recording, replay says so, runs the tests that still match, and reports how
  many recorded tests no longer collect. The reproduction may then be lost.
- **Only parallel runs record.** `-n 0`/`-n 1`, `--dist each` and
  `--shard` write no journal. A `--collect lazy` run records too, and replay
  re-runs its schedule with full collection.
- **Keep journals out of the CI cache.** The bundled action already leaves
  `.rstest_cache/replay` out of the cache it persists. If you cache
  `.rstest_cache` yourself (raw YAML, or another CI system's cache), exclude
  that directory too (`!.rstest_cache/replay` for `actions/cache`). Otherwise
  every save carries up to 11 journals, several MB each on a large suite, and
  a job that recorded nothing can upload an older `latest.json` it restored.

Replay reproduces what each worker ran and in what order, which is what
state-leak and ordering failures depend on. How the workers' timing lines up
with each other is not reproduced, so a true timing race (two workers touching
the same file or port at the same moment) may need several replays or may
not show up at all.

## Notes

- **Exit codes** follow pytest's vocabulary (0 pass, 1 failures, 5 nothing
  collected, ...), merged across workers. The CI gotcha: when rstest itself
  rejects a run (a bad flag combination, no usable interpreter) it also exits
  **1**, the same as test failures, so check the log or the report file, not
  just the code. The full table and each gating flag's codes are in
  [Exit codes](../reference/exit-codes.md).
- **Nothing affected, no reports.** A single-project `--changed` run that
  selects no tests exits before running and writes no `--junitxml` or
  `--report-json`. Set `if-no-files-found: ignore` on artifact uploads and make
  report steps tolerate a missing file. At a monorepo root, `--report-json` is
  still written, with every project marked `"skipped": true` (exit 0, or 5
  with `--changed-strict`); no JUnit is written. See
  [Selecting changed tests](changed.md#ci-usage).
- **`--junitxml`** is rendered by rstest from merged results; point your
  CI's test-report integration at it as you would pytest's.
- **`--report-json`** emits a per-test outcome snapshot (stable schema) if
  you build tooling on top of results.
- **`--output github`** keeps the normal log and additionally emits
  `::error` annotations for each failure, so failures appear inline on the
  PR diff. See [`--output`](../reference/cli.md#-output-dotsverbosebargithubjson).
- **Crash safety matters most in CI**: a segfaulting test costs one FAILED
  entry instead of an aborted job with partial results.
- **Worker count**: `-n auto` starts from the runner's available cores
  (on Linux, the count Rust's `available_parallelism` reports, which honors
  the CPU affinity mask and cgroup quota), then **caps** it: never more
  workers than test files, and, once durations are cached, roughly one worker
  per 2 seconds of total test time. That is the right default for a plain run.
  With `--shard`, pin `-n 2` or more: if `auto` resolves to one worker (a
  1-vCPU runner, a one-file suite, a warm cache under ~2s), `--shard` fails
  with exit 1. See [Sharding](sharding.md).
- **Containers / Kubernetes**: `-n auto` sees a CPU *limit* (a cgroup CPU
  quota, e.g. `docker run --cpus=2` or a pod `resources.limits.cpu`), but a
  pod with only a CPU *request* and no limit has no quota, so `auto` counts
  every core on the node and can start far more workers than the pod is
  scheduled for. Set `-n` to the CPU request there. Memory scales with the
  worker count: each worker is its own Python process, so budget roughly
  (one worker's peak memory) × `-n` against the container's memory limit.
  When the OOM killer takes a worker, rstest reports it like any worker
  crash: the running test fails with the crash message, the worker is
  restarted, and the restart counts against the per-run budget; past that
  budget, the dead worker's remaining tests are reported lost
  ([Crash handling: budgets](../concepts/crash-handling.md#budgets)). A job
  full of crash failures on a memory-limited runner usually means `-n` is too
  high.
- **Timeouts**: a hung test otherwise runs until the CI job limit (6 hours
  on GitHub). Set a per-test [`--timeout SECS`](../reference/cli.md#-timeout-secs)
  (fails the stuck test with a traceback, and also arms the
  [`--worker-timeout`](../reference/cli.md#-worker-timeout-secs) watchdog for
  C code that never returns), and a job-level cap such as `timeout-minutes:`
  on GitHub Actions. When that cap cancels the job, the runner sends
  SIGTERM (or SIGINT): a parallel run then stops its workers, names the test
  each was running and reports it failed (`crashed` in `--report-json`), says
  how many tests did not run, writes the replay journal and any
  `--junitxml`/`--report-json`, and exits 2. The stopped tests are not added
  to `lastfailed`, flake history or the duration cache. A second signal exits
  at once.
- **Reproducing order-dependent failures**: `--shuffle` prints its seed;
  rerun with `--shuffle=SEED` to replay the same order (`rstest replay`
  re-runs the exact per-worker schedule of the failed run). `rstest bisect
  <nodeid>` narrows a test that fails only after others down to the
  polluting test(s).
- **Colors** are disabled automatically when output is not a terminal;
  force with `--color=yes` (or `FORCE_COLOR=1`, which the workers' assertion
  diffs honor too) if your CI renders ANSI. A job on a pty with `CI` set
  (Buildkite, `docker -t`) keeps its colors but gets the plain `dots` log:
  no live footer and no cursor movement.
- **Platform**: these recipes are written for Linux runners but work
  unchanged on `windows-latest` and `macos-latest` (swap the runner image);
  rstest's full test gate runs on all three every commit. On macOS/Windows
  `-n auto` starts from the runner's logical cores (the cgroup/affinity
  narrowing above is Linux-specific), with the same file and time caps.
  Windows has two behavior differences, both with automatic fallbacks:

    - The per-test timeout has no signal-based interrupt, so a slow test is
      only stopped by the hang watchdog, at 3 × its timeout + 10 s, which
      kills its worker. Set `--worker-timeout` for a tighter cap. See
      [`--timeout`](../reference/cli.md#-timeout-secs).
    - File-descriptor leak tracking is unavailable (it reads `/proc/self/fd`
      or `/dev/fd`), so `--doctor` reports thread leaks but not fd leaks. See
      [Resource leaks](resource-leaks.md).

## Go deeper

- [More CI systems](ci-recipes.md): AWS CodeBuild, Google Cloud Build,
  GitLab, Azure, CircleCI, Jenkins, Buildkite, and pre-commit.
- [Shared cache across CI jobs](ci-shared-cache.md): the segment-merge cache
  for a shard matrix.
- [Sharding across CI jobs](sharding.md): how partitions are computed and the
  identical-cache-snapshot rule.
