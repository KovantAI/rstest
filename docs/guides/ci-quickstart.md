# CI quickstart

rstest behaves like pytest in CI: exit-code discipline, JUnit XML for your
test-report integration, and `--report-json` for tooling, with quiet human
output (machine consumers should parse the files, not stdout).

On top of that, rstest adds two things worth wiring up in CI: worker
parallelism with no extra plugin, and a duration cache that makes scheduling
smarter when persisted between runs.

!!! tip "Pin for reproducible CI"
    The recipes use a bare `pip install rstest`. For reproducible builds,
    pin a version (`pip install rstest==0.2.1` or `rstest~=0.2`) or install
    from your lockfile.

## GitHub Actions

The quickest path is the bundled composite action, which wraps install, the
duration cache (correctly keyed), `--changed` base-ref handling, and an
optional fail-ratio gate:

```yaml
jobs:
  tests:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: KovantAI/rstest/.github/actions/rstest@v1
        with:
          python-version: "3.13"
          args: "-n auto"
          upload-junit: true
```

That defaults `--output github` (so failures show as `::error` annotations and
flaky reruns as `::warning`), persists `.rstest_cache` across runs, and writes
`junit.xml`. See the [action README][action] for all inputs (`changed`,
`durations-regress`, `reruns`/`rerun-on`, `fail-under-ratio`, `shard`, …).

[action]: https://github.com/KovantAI/rstest/tree/main/.github/actions/rstest

### Under the hood

The action is a thin wrapper. If you prefer raw YAML — or need something the
action does not expose — the equivalent steps are:

```yaml
jobs:
  tests:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-python@v5
        with:
          python-version: "3.13"

      - name: install
        run: |
          pip install -r requirements.txt
          pip install rstest

      # Persist the duration cache: from the second run on, the scheduler
      # starts the slowest tests first.
      - uses: actions/cache@v4
        with:
          path: .rstest_cache
          # Unique key per run: actions/cache never RE-saves an
          # existing key, so a ref-only key freezes the cache at the
          # branch's first run. restore-keys picks the newest match.
          key: rstest-durations-${{ github.ref_name }}-${{ github.run_id }}
          restore-keys: |
            rstest-durations-${{ github.ref_name }}-
            rstest-durations-

      - name: test
        # --output github emits ::error per failure and ::warning for flaky
        # reruns; --doctor auto-publishes diagnostics to the job summary.
        run: rstest -n auto --output github --junitxml junit.xml

      # Long pole? Fan the suite across a runner matrix with --shard K/N —
      # see the Sharding guide.

      # Monorepo roots: caches live in EACH project (.rstest_cache per
      # package — widen the cache path to **/.rstest_cache), and junit
      # files are written per project as junit.<slug>.xml — glob them
      # in the artifact step.

      - uses: actions/upload-artifact@v4
        if: always()
        with:
          name: junit
          path: junit.xml
```

## AWS CodeBuild

CodeBuild has no log-side annotation command (no equivalent of GitHub's
`::error` or Azure's `##vso`), so there is no dedicated `--output` style
— the integration surface is the JUnit file. Point a [CodeBuild report
group](https://docs.aws.amazon.com/codebuild/latest/userguide/test-reporting.html)
at `--junitxml` output and CodeBuild renders pass/fail, durations, and
run-over-run trends in the console.

```yaml
# buildspec.yml
version: 0.2
phases:
  install:
    commands:
      - pip install -r requirements.txt
      - pip install rstest
  build:
    commands:
      # The `cache` block below persists .rstest_cache across builds, so
      # from the second run on the scheduler starts the slowest tests first.
      - rstest -n auto --junitxml junit.xml

reports:
  rstest:
    files:
      - junit.xml
    file-format: JUNITXML

# Persist .rstest_cache between builds so scheduling stays warm.
cache:
  paths:
    - '.rstest_cache/**/*'
```

`-n auto` uses the build container's vCPUs; size the compute type to the
parallelism you want. For a monorepo root, widen the report `files` glob
to `**/junit.*.xml` (junit is written per project as `junit.<slug>.xml`)
and the cache to `**/.rstest_cache/**/*`.

This single-job recipe re-saves `.rstest_cache` every build, which is
correct here — one full run owns the authoritative cache. If you **shard**
across CodeBuild batch jobs, don't let each shard save: follow the
[sharding guide](sharding.md)'s discipline (shards restore a stable cache
read-only; one separate full job saves the fresh one), or the shards will
race to write divergent duration caches and their partitions will drift.

## Google Cloud Build

Cloud Build likewise has no annotation protocol — it streams step logs
to Cloud Logging and has no native test-report UI, so again there is no
`--output` style to add. Run rstest as a build step and publish the
JUnit XML (and any doctor/report-json) as build
[artifacts](https://cloud.google.com/build/docs/building/store-artifacts-in-cloud-storage).

```yaml
# cloudbuild.yaml
steps:
  - name: python:3.13
    entrypoint: bash
    args:
      - -c
      - |
        pip install -r requirements.txt
        pip install rstest
        rstest -n auto --junitxml junit.xml

# Upload the JUnit (and doctor JSON, if produced) to Cloud Storage.
artifacts:
  objects:
    location: 'gs://$PROJECT_ID-ci-artifacts/$BUILD_ID/'
    paths:
      - 'junit.xml'
```

The duration cache lives in `.rstest_cache`; on Cloud Build persist it
between runs by syncing it to Cloud Storage
(`gsutil rsync`) at the start and end of the step — the workspace itself
is not retained across builds. Colors auto-disable off-tty, so the log
stays clean; the JUnit file is the machine-readable surface for any
downstream test-reporting tool.

## GitLab CI

GitLab reads JUnit from the `artifacts:reports:junit` key to render the
[test report](https://docs.gitlab.com/ci/testing/unit_test_reports/) and
per-MR diff. `--output gitlab` additionally folds each failure into a
[collapsible section](https://docs.gitlab.com/ci/jobs/job_logs/#custom-collapsible-sections)
so the job log stays readable.

```yaml
# .gitlab-ci.yml
test:
  image: python:3.13
  # Persist the duration cache between runs (keyed per branch).
  cache:
    key: rstest-$CI_COMMIT_REF_SLUG
    paths:
      - .rstest_cache/
  before_script:
    - pip install -r requirements.txt
    - pip install rstest
  script:
    - rstest -n auto --output gitlab --junitxml junit.xml
  artifacts:
    when: always
    paths:
      - junit.xml
    reports:
      junit: junit.xml
```

`-n auto` uses the runner's cores; size the runner (or set `-n <k>`) to
the parallelism you want. For a monorepo root, glob `junit.*.xml` in
`artifacts:paths` and widen the cache to `**/.rstest_cache/`.

## Azure Pipelines

`--output azure` emits an `##vso[task.logissue]` per failing test, which
Azure surfaces as an inline issue on the file in the PR. Publish the
JUnit with the
[`PublishTestResults`](https://learn.microsoft.com/azure/devops/pipelines/tasks/reference/publish-test-results-v2)
task for the run's Tests tab.

```yaml
# azure-pipelines.yml
pool:
  vmImage: ubuntu-latest

steps:
  - task: UsePythonVersion@0
    inputs:
      versionSpec: "3.13"

  # Persist the duration cache between runs.
  - task: Cache@2
    inputs:
      key: 'rstest | "$(Agent.OS)" | "$(Build.SourceBranchName)"'
      restoreKeys: |
        rstest | "$(Agent.OS)"
      path: .rstest_cache

  - script: |
      pip install -r requirements.txt
      pip install rstest
      rstest -n auto --output azure --junitxml junit.xml
    displayName: test

  - task: PublishTestResults@2
    condition: always()
    inputs:
      testResultsFormat: JUnit
      testResultsFiles: junit.xml
```

## CircleCI

CircleCI has no log-side annotation protocol, so there is no dedicated
`--output` style — the integration surface is the JUnit file, consumed by
[`store_test_results`](https://circleci.com/docs/collect-test-data/) for
the Tests tab and flaky-test detection.

```yaml
# .circleci/config.yml
version: 2.1
jobs:
  test:
    docker:
      - image: cimg/python:3.13
    steps:
      - checkout
      # Persist the duration cache between runs.
      - restore_cache:
          keys:
            - rstest-{{ .Branch }}
            - rstest-
      - run: pip install -r requirements.txt
      - run: pip install rstest
      - run: rstest -n auto --junitxml test-results/junit.xml
      - store_test_results:
          path: test-results
      - save_cache:
          key: rstest-{{ .Branch }}-{{ .Revision }}
          paths:
            - .rstest_cache
workflows:
  ci:
    jobs:
      - test
```

`-n auto` uses the resource-class vCPUs; pick a larger class for more
parallelism. Point `store_test_results` at a directory (not a single
file) so a monorepo's `junit.*.xml` are all collected.

## Jenkins

Jenkins renders JUnit via the [JUnit
plugin](https://plugins.jenkins.io/junit/); publish the file with
`junit` in a `post` block so results show even when the build fails.

```groovy
// Jenkinsfile
pipeline {
  agent { docker { image 'python:3.13' } }
  stages {
    stage('test') {
      steps {
        sh '''
          pip install -r requirements.txt
          pip install rstest
          rstest -n auto --junitxml junit.xml
        '''
      }
    }
  }
  post {
    always {
      junit 'junit.xml'
    }
  }
}
```

Persist `.rstest_cache` between runs to keep scheduling warm — stash/unstash
it, or use a shared workspace/volume on the agent. If you run a TAP harness
instead, `--output tap` makes stdout a pure TAP 13 stream for the [TAP
plugin](https://plugins.jenkins.io/tap/).

## Pre-commit

rstest ships [pre-commit](https://pre-commit.com) hooks so a suite runs
before code lands. Add to your project's `.pre-commit-config.yaml`:

```yaml
repos:
  - repo: https://github.com/KovantAI/rstest
    rev: v0.2.1             # pin a released tag
    hooks:
      - id: rstest         # whole suite, on push
```

Two hook ids are provided:

- `rstest` — runs the whole suite.
- `rstest-changed` — runs only tests affected by the working-tree changes
  (`rstest --changed`), for a fast per-commit gate.

`rstest` defaults to the `pre-push` stage (a full suite is heavy for every
commit); move it to each commit with `stages: [pre-commit]`.

`rstest-changed` defaults to `pre-commit`, because `--changed` diffs the
working tree against HEAD — at pre-push everything is already committed, so
it would select zero tests and pass silently. On CI, set `GITHUB_BASE_REF`
or `CI_MERGE_REQUEST_*` and `--changed` diffs against the PR base instead.

Pass extra flags with `args`:

```yaml
      - id: rstest-changed
        args: ["-q", "--maxfail=1"]
```


## Suite-health trending with doctor

`--doctor-json` writes the doctor analysis as a versioned JSON document
(see [Suite diagnostics](doctor.md)). Archive it per run and compare a
PR's report against the main branch's — no extra tooling required, the
document already contains totals, wait-bound tests, parallel-floor gate
tests, and fixture costs by name.

Any doctor run also publishes the report as markdown to the CI job
summary automatically — appended to `$GITHUB_STEP_SUMMARY` on GitHub
Actions, piped to `buildkite-agent annotate` on Buildkite — so the
current run's analysis is on the run page with no post-processing step.
(GitLab and TeamCity have no native markdown summary; use `--doctor-md`
and publish the file as an artifact.)

The baseline travels via the actions cache: pushes to main save it, PR
jobs restore it (GitHub lets PRs read the base branch's cache entries):

```yaml
      - name: test (with doctor)
        run: rstest -n auto --junitxml junit.xml --doctor-json doctor.json

      # Save the baseline on main; restore the latest one on PRs.
      - uses: actions/cache@v4
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

[`--migrate-check`](../reference/cli.md#-migrate-check) exits non-zero when a
test has a run-to-run unstable id or fails only under parallelism, so a
dedicated job keeps a migrating suite from regressing — no new co-location
leak, order dependency, or unstable-id site sneaks in green. Use
`--migrate-allow` to tolerate a triaged backlog so the gate fires only on
**new** issues, and `--migrate-check-json` to archive the findings
([schema](../reference/report-json.md#migrate-check-json)):

```yaml
      - name: migrate-check gate
        run: |
          rstest --migrate-check-json migrate.json \
                 --migrate-allow tests/legacy/   # known-unsafe backlog, tolerated
      - uses: actions/upload-artifact@v4
        if: always()
        with:
          name: migrate-check
          path: migrate.json
```

This is heavier than a normal run (it collects twice and reruns the failing
files under discriminators), so run it on its own job or a schedule rather than
every push if the suite is large. Once the suite reports `ready`, drop the gate
and just run `rstest`.

## Notes

- **Exit codes** are pytest's (0 pass, 1 failures, 2 interrupted, 3
  internal, 4 usage error, 5 nothing collected) with sensible merging across workers —
  see [Exit codes](../reference/exit-codes.md).
- **`--junitxml`** is rendered by rstest from merged results; point your
  CI's test-report integration at it as you would pytest's.
- **`--report-json`** emits a per-test outcome snapshot (stable schema) if
  you build tooling on top of results.
- **`--output github`** keeps the normal log and additionally emits
  `::error` annotations for each failure, so failures appear inline on the
  PR diff — see [`--output`](../reference/cli.md#-output-dotsverbosebargithubjson).
- **Crash safety matters most in CI**: a segfaulting test costs one FAILED
  entry instead of an aborted job with partial results.
- **Worker count**: `-n auto` uses the runner's available logical cores —
  on Linux it honors the CPU affinity mask and cgroup CPU quota, so a
  CPU-limited container gets its allocation, not the host's core count. CI
  runners are small (2–4 cores) and not oversubscribed, so `auto` is the
  right default there; pin `-n <k>` only if you need a fixed count.
- **Colors** are disabled automatically when output is not a terminal;
  force with `--color=yes` if your CI renders ANSI.
