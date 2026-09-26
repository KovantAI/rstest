# More CI systems

The [CI quickstart](ci-quickstart.md) covers GitHub Actions and the two
worked examples (Django, monorepo). This page is the per-provider reference
for everyone else (AWS CodeBuild, Google Cloud Build, GitLab CI, Azure
Pipelines, CircleCI, Jenkins, Buildkite), plus the pre-commit hooks.

Every recipe follows the same shape as the quickstart: install rstest, run it
with `-n auto`, publish the JUnit file to the provider's test-report UI, and
persist `.rstest_cache` between runs so scheduling stays warm. Where a
provider's native cache can't merge across a shard matrix, each recipe points
at the [shared-cache backend](ci-shared-cache.md) instead.

!!! tip "Pin for reproducible CI"
    The recipes use a bare `pip install rstest`. For reproducible builds,
    pin an exact version (`pip install rstest==0.7.0`) or install from your
    lockfile, ideally with hashes (`pip install --require-hashes -r
    requirements.txt`). rstest is pre-1.0, so a range like `~=0.7` can still
    pull in breaking changes.

## AWS CodeBuild

CodeBuild has no log-side annotation command (no equivalent of GitHub's
`::error` or Azure's `##vso`), so there is no dedicated `--output` style.
The integration surface is the JUnit file. Point a [CodeBuild report
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
correct here (one full run owns the authoritative cache). If you **shard**
across CodeBuild batch jobs, don't let each shard save: follow the
[sharding guide](sharding.md)'s discipline (shards restore a stable cache
read-only; one separate full job saves the fresh one), or the shards will
race to write divergent duration caches and their partitions will drift.

**Shared cache (sharding, no write race).** Drop the `cache:` block and the
single-writer discipline entirely: the build's IAM role already reaches S3, so
point [`--cache-remote`](ci-shared-cache.md#object-store-s3gcsr2-oidc-no-secrets)
at a bucket and every batch shard pushes its own immutable segment (no
clobber), pulls the union:

```yaml
build:
  commands:
    - rstest -n 4 --shard "$SHARD/$SHARDS"
        --cache-remote s3://ci-cache/rstest --cache-pull --cache-push
        --cache-compact-threshold 500 --junitxml "junit.$SHARD.xml"
```

`--cache-compact-threshold` folds the loose segments inline once they exceed N,
so no maintenance job is needed. `aws s3 sync … ./rcache` + `--cache-remote
./rcache` remains valid if you prefer materializing a dir. The build's service
role needs `s3:ListBucket` + `s3:{Get,Put,Delete}Object` on the prefix
(`Delete` only for the inline compaction).

## Google Cloud Build

Cloud Build likewise has no annotation protocol. It streams step logs
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
(`gsutil rsync`) at the start and end of the step. The workspace itself
is not retained across builds. Colors auto-disable off-tty, so the log
stays clean; the JUnit file is the machine-readable surface for any
downstream test-reporting tool.

**Shared cache (sharding, no rsync bookends).** The build's service account
already reaches GCS, so point [`--cache-remote`](ci-shared-cache.md#object-store-s3gcsr2-oidc-no-secrets)
straight at a `gs://` bucket: rstest drives the `gcloud storage` (or `gsutil`)
CLI on the step, immutable segments make concurrent shard pushes safe, no
start/end sync:

```yaml
steps:
  - name: python:3.13
    entrypoint: bash
    args:
      - -c
      - |
        pip install -r requirements.txt && pip install rstest
        rstest -n 4 --shard "$_SHARD/$_SHARDS" \
          --cache-remote gs://$PROJECT_ID-ci-cache/rstest --cache-pull --cache-push \
          --cache-compact-threshold 500 --junitxml junit.xml
```

The Cloud Build service account needs `storage.objects.{list,get,create,delete}`
on the bucket/prefix (`roles/storage.objectAdmin` scoped to it).

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

**Sharding (`parallel:`).** GitLab exposes `CI_NODE_INDEX` (1-based) and
`CI_NODE_TOTAL` when you set `parallel:`, which map straight onto
[`--shard K/N`](sharding.md). With GitLab's own cache, the shards restore it
read-only so they partition identically:

```yaml
test:
  image: python:3.13
  parallel: 4
  cache:
    key: rstest-durations-$CI_COMMIT_REF_SLUG
    paths: [.rstest_cache]
    policy: pull        # shards restore only; don't race to save
  script:
    - pip install -r requirements.txt && pip install rstest
    - rstest -n 4 --shard ${CI_NODE_INDEX}/${CI_NODE_TOTAL} --output gitlab --junitxml junit.xml
  artifacts:
    when: always
    reports:
      junit: junit.xml   # GitLab merges per-job JUnit natively
```

Something has to write that cache: add a separate non-parallel job with
`policy: pull-push` that runs the full suite, as in the
[GitHub Actions sharding example](sharding.md#github-actions). The shared
cache below removes the need for that job.

**Shared cache (parallel matrix).** GitLab's `cache:` is one blob per key. It
can't merge segments across `parallel:` jobs. For a duration-balanced matrix,
use the [shared-cache backend](ci-shared-cache.md#object-store-s3gcsr2-oidc-no-secrets)
against an object store the runner is authed to (S3/GCS/R2) or an authenticated
`https://` endpoint (`RSTEST_CACHE_REMOTE_TOKEN`):

```yaml
test:
  image: python:3.13
  parallel: 4
  before_script:
    - pip install -r requirements.txt && pip install rstest
  script:
    - rstest -n 4 --shard "$CI_NODE_INDEX/$CI_NODE_TOTAL"
        --cache-remote s3://ci-cache/rstest --cache-pull --cache-push
        --cache-compact-threshold 500 --output gitlab --junitxml junit.xml
  artifacts: { when: always, reports: { junit: junit.xml } }
```

A shared runner mount (`--cache-remote /cache/rstest`) needs no bookends at all.

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

  # Persist the duration cache between runs. The key is unique per build:
  # Azure never overwrites an existing cache entry, so a branch-only key would
  # freeze the durations at the branch's first run. restoreKeys prefix-match
  # the newest entry for this branch, then for any branch.
  - task: Cache@2
    inputs:
      key: 'rstest | "$(Agent.OS)" | "$(Build.SourceBranchName)" | "$(Build.BuildId)"'
      restoreKeys: |
        rstest | "$(Agent.OS)" | "$(Build.SourceBranchName)"
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

**Shared cache (sharding).** rstest has no native Azure Blob transport, so an
`azblob://` remote is rejected loudly rather than silently written to a junk
dir. Two supported paths:

- **Materialize a dir** (works with any store): download the segments to a local
  dir before the run, upload them after, and point `--cache-remote` at the dir.
  The immutable, uniquely-named segments make the up/download safe across shards.

  ```yaml
  - script: |
      # download-batch keeps blob names, so rstest/segments/seg-*.json lands in
      # ./rcache/rstest/segments/; point --cache-remote at ./rcache/rstest.
      az storage blob download-batch -d ./rcache -s ci-cache --pattern 'rstest/*' || true
      mkdir -p ./rcache/rstest/segments
      ls ./rcache/rstest/segments > .warm-segs
      rstest -n 4 --shard "$(shard)/4" \
        --cache-remote ./rcache/rstest --cache-pull --cache-push --junitxml junit.xml
      # Upload only this run's new segment(s), back under rstest/segments/.
      mkdir -p ./push
      for f in ./rcache/rstest/segments/seg-*.json; do
        [ -e "$f" ] || continue
        grep -qxF "$(basename "$f")" .warm-segs || cp "$f" ./push/
      done
      az storage blob upload-batch -d ci-cache --destination-path rstest/segments -s ./push
    displayName: test (shared cache)
  ```

  The pipeline's service connection / managed identity needs **Storage Blob Data
  Contributor** on the container (the `az` batch calls read, write, and delete).

- **Authenticated `https://` endpoint**: front the store with a static file
  server honoring the [listing contract](../concepts/caching.md#transports) and
  use `--cache-remote https://… ` with `RSTEST_CACHE_REMOTE_TOKEN`.

## CircleCI

CircleCI has no log-side annotation protocol, so there is no dedicated
`--output` style. The integration surface is the JUnit file, consumed by
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

**Sharding (`parallelism:`).** CircleCI provides `CIRCLE_NODE_INDEX`
(**0-based**) and `CIRCLE_NODE_TOTAL`, so add 1 to the index for
[`--shard K/N`](sharding.md). The shards restore the cache read-only, and a
separate non-parallel job runs the full suite to write it (without that job,
every run partitions cold: an even split with no wall-time balancing):

```yaml
jobs:
  test:
    docker:
      - image: cimg/python:3.13
    parallelism: 4
    steps:
      - checkout
      - restore_cache: { keys: ["rstest-durations-{{ .Branch }}"] }
      - run: pip install -r requirements.txt && pip install rstest
      - run: rstest -n 4 --shard $((CIRCLE_NODE_INDEX + 1))/$CIRCLE_NODE_TOTAL --junitxml test-results/junit.xml
      - store_test_results: { path: test-results }   # a directory, not a file
  durations:
    docker:
      - image: cimg/python:3.13
    steps:
      - checkout
      - restore_cache: { keys: ["rstest-durations-{{ .Branch }}"] }
      - run: pip install -r requirements.txt && pip install rstest
      - run: rstest -n auto -q
      - save_cache:
          key: rstest-durations-{{ .Branch }}-{{ .Revision }}
          paths: [".rstest_cache"]
workflows:
  test-and-cache:
    jobs:
      - test
      - durations
```

CircleCI keys are immutable once written, so the `{{ .Revision }}` suffix
makes each run save a fresh key that the shards' branch-prefix
`restore_cache` picks up on the next push. The Tests tab aggregates
per-container results; for one merged `junit.xml` artifact, add a
downstream collect-and-merge step.

**Shared cache (parallelism).** `save_cache`/`restore_cache` is one blob per key.
It can't merge across `parallelism: N` containers. Point
[`--cache-remote`](ci-shared-cache.md#object-store-s3gcsr2-oidc-no-secrets) at an
object store the job is authed to (S3/GCS via a context or OIDC) so each
container pushes its segment and pulls the union:

```yaml
- run: |
    rstest -n 4 --shard "$((CIRCLE_NODE_INDEX+1))/$CIRCLE_NODE_TOTAL" \
      --cache-remote s3://ci-cache/rstest --cache-pull --cache-push \
      --cache-compact-threshold 500 --junitxml test-results/junit.xml
```

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

Persist `.rstest_cache` between runs to keep scheduling warm: stash/unstash
it, or use a shared workspace/volume on the agent. If you run a TAP harness
instead, `--output tap` makes stdout a pure TAP 13 stream for the [TAP
plugin](https://plugins.jenkins.io/tap/).

**Shared cache (agents, sharding): zero glue.** Jenkins agents usually share
an NFS/volume mount, which *is* the [shared-cache
remote](ci-shared-cache.md#self-hosted-shared-mount-zero-glue): no
stash/unstash, no pull/push bookends beyond the flags. Parallel stages / matrix
shards each push their immutable segment to the same mount and pull the union:

```groovy
sh '''
  rstest -n 4 --shard "${SHARD}/${SHARDS}" \
    --cache-remote /mnt/ci-cache/rstest --cache-pull --cache-push \
    --junitxml junit.xml
'''
```

No mount? Point `--cache-remote` at `s3://…` / `gs://…` (the agent's cloud CLI
drives it) instead.

## Buildkite

rstest has a native Buildkite style: `--output buildkite` prints each
failure under an auto-expanded `+++` log group, and `--doctor` pipes its report to
`buildkite-agent annotate` when the agent is available. `--changed` detects
the PR base from `BUILDKITE_PULL_REQUEST_BASE_BRANCH`.

```yaml
# .buildkite/pipeline.yml
steps:
  - label: ":pytest: rstest"
    command: |
      pip install -r requirements.txt
      pip install rstest==0.7.0
      rstest -n auto --output buildkite --junitxml junit.xml
    artifact_paths:
      - junit.xml
    parallelism: 1        # for a shard matrix: set N and use
                          # rstest -n 4 --shard "$$((BUILDKITE_PARALLEL_JOB + 1))/$$BUILDKITE_PARALLEL_JOB_COUNT"
```

Feed `junit.xml` to the [Test Engine
collector](https://buildkite.com/docs/test-engine) or the JUnit annotate plugin
for a test report. Buildkite agents usually don't persist `.rstest_cache`
between builds; use the [shared-cache backend](ci-shared-cache.md) (an
`s3://` prefix works well on AWS-hosted agents) to keep scheduling warm.

## Pre-commit

rstest ships [pre-commit](https://pre-commit.com) hooks so a suite runs
before code lands. Add to your project's `.pre-commit-config.yaml`:

```yaml
repos:
  - repo: https://github.com/KovantAI/rstest
    rev: v0.7.0             # pin a released tag
    hooks:
      - id: rstest         # whole suite, on push
```

Two hook ids are provided:

- `rstest`: runs the whole suite.
- `rstest-changed`: runs only tests affected by the working-tree changes
  (`rstest --changed`), for a fast per-commit gate.

`rstest` defaults to the `pre-push` stage (a full suite is heavy for every
commit); move it to each commit with `stages: [pre-commit]`.

`rstest-changed` defaults to `pre-commit`, because `--changed` diffs the
working tree against HEAD. At pre-push everything is already committed, so
it would select zero tests and pass silently. On CI, set `GITHUB_BASE_REF`
or `CI_MERGE_REQUEST_*` and `--changed` diffs against the PR base instead.

`--changed` gets **tighter** when a coverage index is warm: run your suite
once with `--cov-context=test` (e.g. a scheduled main-branch job) and it maps
changed *lines* to only the tests that cover them, not every importer. The
`.rstest_cache` you already persist carries the index, so PR jobs pick
it up automatically; without it, `--changed` falls back to the import graph.
See [Selecting changed tests](changed.md).

Pass extra flags with `args`:

```yaml
      - id: rstest-changed
        args: ["-q", "--maxfail=1"]
```

## Go deeper

- [CI quickstart](ci-quickstart.md): GitHub Actions, the Django and monorepo
  worked examples, doctor trending, and the migrate-check gate.
- [Shared cache across CI jobs](ci-shared-cache.md): the segment model and
  every transport, for a shard matrix.
- [Sharding across CI jobs](sharding.md): how partitions are computed.
- [Exit codes](../reference/exit-codes.md) · [Report JSON](../reference/report-json.md):
  the machine-readable surfaces to build gates on.
