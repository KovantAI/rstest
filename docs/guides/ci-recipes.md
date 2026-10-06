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

--8<-- "docs/_snippets/ci-pin-tip.md"

All of them follow the [CI cache rules](ci-shared-cache.md#ci-cache-rules):
a recipe that persists `.rstest_cache` removes `.rstest_cache/replay` before
the cache is saved ([why](ci-shared-cache.md#keep-replay-journals-out-of-the-cache)),
and a sharded recipe gives every shard one cache snapshot, uploads segments only
from default-branch builds, and gates on `rstest shard-verify`
([why](ci-shared-cache.md#one-snapshot-per-shard-matrix)).

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
  post_build:
    commands:
      # Runs even when the build phase failed, before the cache is saved:
      # keep replay journals out of the cache.
      - rm -rf .rstest_cache/replay

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
and the cache to `**/.rstest_cache/**/*`, and remove every project's journals:
`find . -path '*/.rstest_cache/replay' -prune -exec rm -rf {} +`.

This single-job recipe re-saves `.rstest_cache` every build, which is
correct here: one full run owns the cache. Don't reuse it for a sharded batch
build, where every shard would save its own partial cache.

**Sharding (batch build, shared cache).** A
[batch build graph](https://docs.aws.amazon.com/codebuild/latest/userguide/batch-build-buildspec.html)
runs one snapshot build, the shards, and a verify build. The build's IAM role
already reaches S3, so the snapshot is a copy inside the bucket, keyed by
commit so a retried batch reuses it and partitions the same way:

```yaml
# buildspec.yml
version: 0.2
batch:
  build-graph:
    - identifier: snapshot
      env: { variables: { STEP: snapshot } }
    - identifier: shard1
      depend-on: [snapshot]
      env: { variables: { STEP: shard, SHARD: "1" } }
    - identifier: shard2
      depend-on: [snapshot]
      env: { variables: { STEP: shard, SHARD: "2" } }
    - identifier: verify
      depend-on: [shard1, shard2]
      env: { variables: { STEP: verify } }
env:
  shell: bash
  variables:
    SHARDS: "2"
    LIVE: s3://ci-cache/rstest
phases:
  install:
    commands:
      - pip install rstest
      - if [ "$STEP" = shard ]; then pip install -r requirements.txt; fi
  build:
    commands:
      - |
        SNAP="s3://ci-cache/rstest-snap/$CODEBUILD_RESOLVED_SOURCE_VERSION"
        REPORTS="s3://ci-cache/rstest-reports/$CODEBUILD_RESOLVED_SOURCE_VERSION"
        case "$STEP" in
        snapshot)
          aws s3 ls "$SNAP/" >/dev/null || aws s3 sync "$LIVE" "$SNAP"
          ;;
        shard)
          mkdir -p rcache/segments && aws s3 sync "$SNAP" rcache
          ls rcache/segments > .warm-segs
          code=0
          rstest -n 4 --shard "$SHARD/$SHARDS" \
            --cache-remote rcache --cache-pull --cache-push \
            --report-json "shard.$SHARD.json" --junitxml "junit.$SHARD.xml" || code=$?
          aws s3 cp "shard.$SHARD.json" "$REPORTS/"
          # Default branch only: publish the segment this shard wrote.
          if [ "$CODEBUILD_WEBHOOK_HEAD_REF" = refs/heads/main ]; then
            for f in rcache/segments/seg-*.json; do
              [ -e "$f" ] || continue
              grep -qxF "$(basename "$f")" .warm-segs || aws s3 cp "$f" "$LIVE/segments/"
            done
          fi
          test "$code" -eq 0
          ;;
        verify)
          aws s3 cp "$REPORTS/" . --recursive
          rstest shard-verify shard.*.json
          ;;
        esac

reports:
  rstest:
    files:
      - "junit.*.xml"
    file-format: JUNITXML
```

The service role needs `s3:ListBucket` + `s3:{Get,Put}Object` on the three
prefixes (add `s3:DeleteObject` for the job that compacts). Pull-request builds
skip the upload; give them a role that can't write `rstest/` at all. Expire
`rstest-snap/` and `rstest-reports/` with a bucket lifecycle rule after a day.
For a single unsharded build, the S3 remote needs no snapshot:
`rstest -n auto --cache-remote s3://ci-cache/rstest --cache-pull --cache-push
--cache-compact-threshold 50`. Keep that threshold low (20 to 50): each pull
reads every loose segment with its own `aws` process, one after another.

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
(`gsutil rsync`) at the start and end of the step, removing
`.rstest_cache/replay` before the upload. The workspace itself
is not retained across builds. Colors auto-disable off-tty, so the log
stays clean; the JUnit file is the machine-readable surface for any
downstream test-reporting tool.

For a single build, `--cache-remote gs://$PROJECT_ID-ci-cache/rstest
--cache-pull --cache-push` replaces the rsync bookends: rstest drives the
`gcloud storage` (or `gsutil`) CLI in the step, so run that step in an image
that has it.

**Sharding (shared cache).** Cloud Build has no job matrix, so the shards are
separate builds that whatever starts them launches with the same `_SNAP` (a
run id), `_SHARD` and `_SHARDS` substitutions. Before launching them, that
starter copies the live cache once, and after they finish it runs the gate:

```console
$ gcloud storage cp -r gs://$PROJECT_ID-ci-cache/rstest gs://$PROJECT_ID-ci-cache/rstest-snap/$RUN
$ # ... launch one build per shard with _SNAP=$RUN, wait for all of them ...
$ gcloud storage cp "gs://$PROJECT_ID-ci-cache/rstest-reports/$RUN/*" .
$ rstest shard-verify shard.*.json
```

Each shard build reads the snapshot, runs, saves its report, and (for
default-branch runs, `_PUSH=1`) publishes only the segment it wrote. `$$`
escapes a shell `$` from Cloud Build's substitution:

```yaml
# cloudbuild-shard.yaml
substitutions:
  _PUSH: "0"
steps:
  - name: gcr.io/google.com/cloudsdktool/cloud-sdk:slim
    entrypoint: bash
    args:
      - -c
      - |
        mkdir -p rcache/segments
        gcloud storage rsync -r gs://$PROJECT_ID-ci-cache/rstest-snap/$_SNAP rcache || true
        ls rcache/segments > .warm-segs
  - name: python:3.13
    entrypoint: bash
    args:
      - -c
      - |
        pip install -r requirements.txt && pip install rstest
        rstest -n 4 --shard "$_SHARD/$_SHARDS" \
          --cache-remote rcache --cache-pull --cache-push \
          --report-json shard.$_SHARD.json --junitxml junit.xml || touch .failed
  - name: gcr.io/google.com/cloudsdktool/cloud-sdk:slim
    entrypoint: bash
    args:
      - -c
      - |
        gcloud storage cp shard.$_SHARD.json gs://$PROJECT_ID-ci-cache/rstest-reports/$_SNAP/
        if [ "$_PUSH" = 1 ]; then
          for f in rcache/segments/seg-*.json; do
            [ -e "$$f" ] || continue
            grep -qxF "$$(basename "$$f")" .warm-segs ||
              gcloud storage cp "$$f" gs://$PROJECT_ID-ci-cache/rstest/segments/
          done
        fi
        [ ! -e .failed ]
```

The service account needs `storage.objects.{list,get,create}` on the bucket
(`roles/storage.objectAdmin` scoped to it covers compaction's `delete` too).
Keep any `--cache-compact-threshold` low (20 to 50): a pull spawns one
`gcloud` process per loose segment, sequentially.

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
  # Persist the duration cache between runs, keyed per branch, Python
  # version and lockfile hash (GitLab appends a hash of the files to prefix).
  cache:
    key:
      files: [requirements.txt]
      prefix: rstest-py3.13-$CI_COMMIT_REF_SLUG
    paths:
      - .rstest_cache/
  before_script:
    - pip install -r requirements.txt
    - pip install rstest
  script:
    - rstest -n auto --output gitlab --junitxml junit.xml
  # Runs before the cache is saved, even when a test failed: move the replay
  # journal out of the cache and keep it as an artifact instead.
  after_script:
    - mv .rstest_cache/replay rstest-replay || true
  artifacts:
    when: always
    paths:
      - junit.xml
      - rstest-replay/latest.json
    reports:
      junit: junit.xml
```

`-n auto` uses the runner's cores; size the runner (or set `-n <k>`) to
the parallelism you want. For a monorepo root, glob `junit.*.xml` in both
`artifacts:paths` and `artifacts:reports:junit` (rstest writes one
`junit.<slug>.xml` per project, and no `junit.xml`), widen the cache to
`**/.rstest_cache/`, and move each project's `.rstest_cache/replay` out the
same way.

**Sharding (`parallel:`).** GitLab exposes `CI_NODE_INDEX` (1-based) and
`CI_NODE_TOTAL` when you set `parallel:`, which map straight onto
[`--shard K/N`](sharding.md). With GitLab's own cache, the shards restore it
read-only so they partition identically, one later job saves it, and another
checks the shards covered the suite:

```yaml
stages: [test, post]

test:
  stage: test
  image: python:3.13
  parallel: 4
  cache:
    # The cache the default branch's `durations` job writes (no branch in the
    # key). An unprotected MR branch can't read it by default (see below).
    key:
      files: [requirements.txt]
      prefix: rstest-durations-py3.13
    paths: [.rstest_cache]
    policy: pull        # shards restore only; don't race to save
  script:
    - pip install -r requirements.txt && pip install rstest
    - rstest -n 4 --shard ${CI_NODE_INDEX}/${CI_NODE_TOTAL} --output gitlab --report-json shard.${CI_NODE_INDEX}.json --junitxml junit.xml
  artifacts:
    when: always
    paths: [shard.*.json]
    reports:
      junit: junit.xml   # GitLab merges per-job JUnit natively

# The one writer: a full run in a LATER stage, so it saves only after every
# shard has restored (a save mid-matrix would give later shards a different
# cache and a different partition). Default branch only (see below).
durations:
  stage: post
  needs: [test]
  rules:
    - if: $CI_COMMIT_BRANCH == $CI_DEFAULT_BRANCH
      when: always      # refresh timings even when a shard failed
  image: python:3.13
  cache:
    key:
      files: [requirements.txt]
      prefix: rstest-durations-py3.13
    paths: [.rstest_cache]
    policy: pull-push
    when: always        # save even when this run has a failing test
  script:
    - pip install -r requirements.txt && pip install rstest
    - rstest -n auto -q
  after_script:
    - rm -rf .rstest_cache/replay   # runs before the cache is saved

# Fails unless the shards ran every collected test exactly once.
shard-verify:
  stage: post
  needs: [test]
  when: always
  image: python:3.13
  script:
    - pip install rstest
    - rstest shard-verify shard.*.json
```

`durations` runs only on the default branch: a full unsharded run in every
pipeline would cost as much as the matrix it exists to speed up, and only
trusted pipelines should write the cache the shards read. Shards on other
branches restore the default branch's cache; a branch that changes
`requirements.txt` gets a new key and partitions by count until that change
lands. GitLab also keeps separate caches for protected and unprotected
branches by default, so shards on an unprotected merge-request branch can't
read the protected default branch's cache and partition by count (still
correct, and `shard-verify` guards it). Leave that setting on: it is the
[trust boundary](../concepts/caching.md#trust-boundary) that stops
unprotected branches writing the cache the default branch reads. For warm
merge-request shards, use the shared cache below, which also removes the need
for the `durations` job.

**Shared cache (parallel matrix).** GitLab's `cache:` is one blob per key. It
can't merge segments across `parallel:` jobs. For a duration-balanced matrix,
use the [shared-cache backend](ci-shared-cache.md#object-store-s3gcsr2-oidc-no-secrets)
against an object store the runner is authed to (S3 here; `gs://` works the
same with `gcloud storage`). One job snapshots the prefix into an artifact,
every shard reads that artifact, and default-branch shards upload only the
segment they wrote:

```yaml
stages: [snapshot, test, post]

cache-snapshot:
  stage: snapshot
  image: { name: amazon/aws-cli, entrypoint: [""] }
  script:
    - mkdir -p rcache/segments
    - aws s3 sync s3://ci-cache/rstest rcache
  artifacts:
    paths: [rcache/]

test:
  stage: test
  image: python:3.13
  parallel: 4
  needs: [cache-snapshot]
  before_script:
    - pip install -r requirements.txt && pip install rstest awscli
  script:
    - mkdir -p rcache/segments && ls rcache/segments > .warm-segs
    - rstest -n 4 --shard "$CI_NODE_INDEX/$CI_NODE_TOTAL" --cache-remote rcache --cache-pull --cache-push --output gitlab --report-json "shard.$CI_NODE_INDEX.json" --junitxml junit.xml
  after_script:
    # Default branch only: publish the segment this shard wrote.
    - |
      [ "$CI_COMMIT_BRANCH" = "$CI_DEFAULT_BRANCH" ] || exit 0
      for f in rcache/segments/seg-*.json; do
        [ -e "$f" ] || continue
        grep -qxF "$(basename "$f")" .warm-segs || aws s3 cp "$f" s3://ci-cache/rstest/segments/
      done
  artifacts:
    when: always
    paths: [shard.*.json]
    reports: { junit: junit.xml }

shard-verify:
  stage: post
  needs: [test]
  when: always
  image: python:3.13
  script:
    - pip install rstest
    - rstest shard-verify shard.*.json
```

A shared runner mount works the same way: snapshot it with `cp -R` instead of
`aws s3 sync`. Give only default-branch pipelines credentials that can write
the prefix; merge-request pipelines need read access for the snapshot only.

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

  # Persist the duration cache between runs, keyed per OS, Python version and
  # lockfile hash (the requirements.txt segment hashes the file). The key is
  # unique per build: Azure never overwrites an existing cache entry, so a
  # branch-only key would freeze the durations at the branch's first run.
  # Azure scopes caches by branch, so restoreKeys prefix-match the newest entry
  # this branch saved, then the newest the default branch saved.
  - task: Cache@2
    inputs:
      key: 'rstest | "$(Agent.OS)" | "py3.13" | requirements.txt | "$(Build.SourceBranch)" | "$(Build.BuildId)"'
      restoreKeys: |
        rstest | "$(Agent.OS)" | "py3.13" | requirements.txt | "$(Build.SourceBranch)"
        rstest | "$(Agent.OS)" | "py3.13" | requirements.txt
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

  # Cache@2 saves in a post-job step, after this one.
  - script: rm -rf .rstest_cache/replay
    displayName: keep replay journals out of the cache
    condition: always()
```

**Shared cache (sharding).** rstest has no native Azure Blob transport, so an
`azblob://` remote is rejected loudly rather than silently written to a junk
dir. Two supported paths:

- **Materialize a dir** (works with any store): one job downloads the segments
  once and publishes them as a pipeline artifact, every shard runs against
  that copy and stages only the segment it wrote, and on the default branch a
  separate step uploads it. The immutable, uniquely-named segments make the
  uploads safe across shards.

  ```yaml
  variables:
    CACHE_ACCOUNT: cicache   # the storage account that holds the ci-cache container

  jobs:
  - job: snapshot
    steps:
    - task: AzureCLI@2
      displayName: snapshot the shared cache
      inputs:
        azureSubscription: ci-cache-read   # Storage Blob Data Reader
        scriptType: bash
        scriptLocation: inlineScript
        inlineScript: |
          # download-batch keeps blob names, so rstest/segments/seg-*.json lands
          # in ./rcache/rstest/segments/; point --cache-remote at ./rcache/rstest.
          az storage blob download-batch -d ./rcache -s ci-cache --pattern 'rstest/*' \
            --account-name "$(CACHE_ACCOUNT)" --auth-mode login || true
          mkdir -p ./rcache/rstest/segments
    - publish: ./rcache
      artifact: rstest-snapshot

  - job: test
    dependsOn: snapshot
    strategy:
      parallel: 4       # sets System.JobPositionInPhase (1-based) / TotalJobsInPhase
    steps:
    - task: UsePythonVersion@0
      inputs:
        versionSpec: "3.13"
    - script: pip install -r requirements.txt && pip install rstest
      displayName: install
    - task: DownloadPipelineArtifact@2
      inputs:
        artifact: rstest-snapshot
        targetPath: $(System.DefaultWorkingDirectory)/rcache
    # No cloud credentials here: test code never holds the write identity.
    - script: |
        mkdir -p ./rcache/rstest/segments ./push
        ls ./rcache/rstest/segments > .warm-segs
        # Azure runs `script:` without errexit, so the step's exit status is the
        # LAST command's. Keep rstest's code, stage, then exit with it.
        code=0
        rstest -n 4 --shard "$(System.JobPositionInPhase)/$(System.TotalJobsInPhase)" \
          --cache-remote ./rcache/rstest --cache-pull --cache-push \
          --report-json "shard.$(System.JobPositionInPhase).json" --junitxml junit.xml || code=$?
        # Stage only this run's new segment(s) for the upload step below.
        for f in ./rcache/rstest/segments/seg-*.json; do
          [ -e "$f" ] || continue
          grep -qxF "$(basename "$f")" .warm-segs || cp "$f" ./push/
        done
        exit $code
      displayName: test (shared cache)
    # Default branch only, and even when a test failed.
    - task: AzureCLI@2
      displayName: publish this shard's new segment
      condition: and(succeededOrFailed(), eq(variables['Build.SourceBranch'], 'refs/heads/main'))
      inputs:
        azureSubscription: ci-cache-write   # Storage Blob Data Contributor
        scriptType: bash
        scriptLocation: inlineScript
        inlineScript: |
          set -- ./push/seg-*.json
          [ -e "$1" ] || exit 0
          az storage blob upload-batch -d ci-cache --destination-path rstest/segments -s ./push \
            --account-name "$(CACHE_ACCOUNT)" --auth-mode login
    - publish: shard.$(System.JobPositionInPhase).json
      artifact: shard-report-$(System.JobPositionInPhase)
      condition: always()

  - job: verify
    dependsOn: test
    condition: succeededOrFailed()
    steps:
    - task: UsePythonVersion@0
      inputs:
        versionSpec: "3.13"
    - download: current
      patterns: '**/shard.*.json'
    - script: |
        pip install rstest
        rstest shard-verify $(Pipeline.Workspace)/shard-report-*/shard.*.json
      displayName: shard-verify
  ```

  `--auth-mode login` makes `az` use the service connection's Microsoft Entra
  identity instead of an account key. Split it in two: `ci-cache-read` needs
  **Storage Blob Data Reader** on the container, and `ci-cache-write` needs
  **Storage Blob Data Contributor** (nothing here deletes, since the recipe
  never compacts). The `condition:` keeps pull-request builds off the write
  step, but a pull request can edit the pipeline YAML, so also add a branch
  control check on `ci-cache-write` that allows only `refs/heads/main`.

- **Authenticated `https://` endpoint**: front the store with a static file
  server honoring the [listing contract](../concepts/caching.md#http-listing-contract) and
  use `--cache-remote https://…` with `RSTEST_CACHE_REMOTE_TOKEN`. That is a
  live remote, so use it for a single job; a shard matrix needs the snapshot
  layout above.

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
      # Persist the duration cache between runs, keyed per OS/arch, Python
      # version and lockfile hash. Newest cache for this branch, else the
      # newest from main. No branchless fallback: that would let main restore
      # any branch's cache.
      - restore_cache:
          keys:
            - rstest-{{ arch }}-py3.13-{{ checksum "requirements.txt" }}-{{ .Branch }}-
            - rstest-{{ arch }}-py3.13-{{ checksum "requirements.txt" }}-main-
      - run: pip install -r requirements.txt
      - run: pip install rstest
      - run: rstest -n auto --junitxml test-results/junit.xml
      - store_test_results:
          path: test-results
      - run: rm -rf .rstest_cache/replay   # keep replay journals out of the cache
      - save_cache:
          key: rstest-{{ arch }}-py3.13-{{ checksum "requirements.txt" }}-{{ .Branch }}-{{ .Revision }}
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
[`--shard K/N`](sharding.md). The shards restore the cache read-only, a
separate non-parallel job on `main` runs the full suite to write it (without
that job, every run partitions cold: an even split with no wall-time
balancing), and a `verify` job checks the shards covered the suite:

```yaml
jobs:
  test:
    docker:
      - image: cimg/python:3.13
    parallelism: 4
    steps:
      - checkout
      # The cache the `durations` job writes on main (no branch in the key).
      - restore_cache:
          keys:
            - rstest-durations-{{ arch }}-py3.13-{{ checksum "requirements.txt" }}-
      - run: pip install -r requirements.txt && pip install rstest
      - run: |
          K=$((CIRCLE_NODE_INDEX + 1))
          mkdir -p reports
          rstest -n 4 --shard "$K/$CIRCLE_NODE_TOTAL" \
            --report-json "reports/shard.$K.json" --junitxml test-results/junit.xml
      - store_test_results: { path: test-results }   # a directory, not a file
      - persist_to_workspace: { root: ., paths: [reports] }
  durations:
    docker:
      - image: cimg/python:3.13
    steps:
      - checkout
      - restore_cache:
          keys:
            - rstest-durations-{{ arch }}-py3.13-{{ checksum "requirements.txt" }}-
      - run: pip install -r requirements.txt && pip install rstest
      - run: rstest -n auto -q
      - run:
          name: keep replay journals out of the cache
          command: rm -rf .rstest_cache/replay
          when: always
      - save_cache:
          key: rstest-durations-{{ arch }}-py3.13-{{ checksum "requirements.txt" }}-{{ .Revision }}
          paths: [".rstest_cache"]
          when: always   # save even when this run has a failing test
  verify:
    docker:
      - image: cimg/python:3.13
    steps:
      - attach_workspace: { at: . }
      - run: pip install rstest && rstest shard-verify reports/shard.*.json
workflows:
  test-and-cache:
    jobs:
      - test
      # After the shards, so a save can't land mid-matrix and hand later
      # containers a different cache.
      # [success, failed]: still refresh timings when a shard failed.
      # main only: a full run in every workflow would cost as much as the
      # matrix, and only trusted builds should write the cache shards read.
      - durations:
          requires:
            - test: [success, failed]
          filters:
            branches:
              only: main
      - verify:
          requires: [test]
```

CircleCI keys are immutable once written, so the `{{ .Revision }}` suffix
makes each `main` run save a fresh key that every branch's shards pick up by
prefix on their next push. A branch that changes `requirements.txt` gets a new
prefix and partitions by count until that change reaches `main`. A failing shard skips its
`persist_to_workspace`, so `verify` runs only when every shard passed: it
guards a green build against dropped tests. The Tests tab aggregates
per-container results; for one merged `junit.xml` artifact, add a downstream
collect-and-merge step.

**Shared cache (parallelism).** `save_cache`/`restore_cache` is one blob per key.
It can't merge across `parallelism: N` containers. Point the shards at an
object store the job is authed to (S3/GCS via a context or OIDC): one job
snapshots the prefix into the workspace, every container reads that copy, and
default-branch containers upload only the segment they wrote:

```yaml
jobs:
  cache-snapshot:
    docker:
      - image: cimg/python:3.13
    steps:
      - run: mkdir -p rcache/segments && aws s3 sync s3://ci-cache/rstest rcache
      - persist_to_workspace: { root: ., paths: [rcache] }
  test:
    docker:
      - image: cimg/python:3.13
    parallelism: 4
    steps:
      - checkout
      - attach_workspace: { at: . }
      - run: pip install -r requirements.txt && pip install rstest
      - run: |
          K=$((CIRCLE_NODE_INDEX + 1))
          mkdir -p rcache/segments reports && ls rcache/segments > .warm-segs
          rstest -n 4 --shard "$K/$CIRCLE_NODE_TOTAL" \
            --cache-remote rcache --cache-pull --cache-push \
            --report-json "reports/shard.$K.json" --junitxml test-results/junit.xml
      - run:
          name: publish this shard's new segment (main only)
          when: always
          command: |
            [ "$CIRCLE_BRANCH" = main ] || exit 0
            for f in rcache/segments/seg-*.json; do
              [ -e "$f" ] || continue
              grep -qxF "$(basename "$f")" .warm-segs || aws s3 cp "$f" s3://ci-cache/rstest/segments/
            done
      - store_test_results: { path: test-results }
      - persist_to_workspace: { root: ., paths: [reports] }
  verify:
    docker:
      - image: cimg/python:3.13
    steps:
      - attach_workspace: { at: . }
      - run: pip install rstest && rstest shard-verify reports/shard.*.json
workflows:
  test:
    jobs:
      - cache-snapshot
      - test:
          requires: [cache-snapshot]
      - verify:
          requires: [test]
```

Both `cache-snapshot` and `test` need the `aws` CLI (the `aws-cli` orb or
your own image). Give the write-capable context or OIDC role only to
default-branch builds; other branches need read access for the snapshot only.

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
it (with `excludes: '.rstest_cache/replay/**'`), or use a shared
workspace/volume on the agent. If you run a TAP harness
instead, `--output tap` makes stdout a pure TAP 13 stream for the [TAP
plugin](https://plugins.jenkins.io/tap/).

**Shared cache (agents, sharding).** Jenkins agents usually share an
NFS/volume mount, which can be the [shared-cache
remote](ci-shared-cache.md#self-hosted-shared-mount-zero-glue) with no
stash/unstash. For a single job, `--cache-remote /mnt/ci-cache/rstest
--cache-pull --cache-push` is the whole setup. For parallel shards, copy the
mount once per build, run every shard against that copy, and gate on
`shard-verify`:

```groovy
// Jenkinsfile (multibranch: BRANCH_NAME is set)
pipeline {
  agent none
  stages {
    stage('snapshot') {
      agent any
      steps {
        sh '''
          snap="/mnt/ci-cache/snap/$BUILD_TAG"
          mkdir -p "$snap/segments"
          cp -R /mnt/ci-cache/rstest/. "$snap/" 2>/dev/null || true
        '''
      }
    }
    stage('test') {
      matrix {
        agent { docker { image 'python:3.13'; args '-v /mnt/ci-cache:/mnt/ci-cache' } }
        axes { axis { name 'SHARD'; values '1', '2', '3', '4' } }
        stages {
          stage('shard') {
            steps {
              sh '''
                rm -rf rcache && cp -R "/mnt/ci-cache/snap/$BUILD_TAG" rcache
                ls rcache/segments > .warm-segs
                pip install -r requirements.txt && pip install rstest
                code=0
                rstest -n 4 --shard "$SHARD/4" \
                  --cache-remote rcache --cache-pull --cache-push \
                  --report-json "shard.$SHARD.json" --junitxml "junit.$SHARD.xml" || code=$?
                # Default branch only: publish the segment this shard wrote.
                if [ "$BRANCH_NAME" = main ]; then
                  for f in rcache/segments/seg-*.json; do
                    [ -e "$f" ] || continue
                    grep -qxF "$(basename "$f")" .warm-segs || cp "$f" /mnt/ci-cache/rstest/segments/
                  done
                fi
                exit $code
              '''
            }
            post {
              always {
                junit "junit.${SHARD}.xml"
                stash name: "shard-${SHARD}", includes: "shard.${SHARD}.json", allowEmpty: true
              }
            }
          }
        }
      }
    }
    stage('verify') {
      agent { docker { image 'python:3.13' } }
      steps {
        script { ['1', '2', '3', '4'].each { unstash "shard-${it}" } }
        sh 'pip install rstest && rstest shard-verify shard.*.json'
      }
    }
  }
}
```

Each matrix cell gets its own agent and workspace, so the shards never share a
local `rcache`. `verify` runs only when every shard passed. Prune
`/mnt/ci-cache/snap/` with a periodic job. No mount? Snapshot an `s3://…` /
`gs://…` prefix with `aws s3 sync` / `gcloud storage rsync` instead. Only
builds of your default branch should be able to write the mount or bucket
([trust boundary](../concepts/caching.md#trust-boundary)).

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
      pip install rstest
      rstest -n auto --output buildkite --junitxml junit.xml
    artifact_paths:
      - junit.xml
```

Feed `junit.xml` to the [Test Engine
collector](https://buildkite.com/docs/test-engine) or the JUnit annotate plugin
for a test report. Buildkite agents usually don't persist `.rstest_cache`
between builds, so without a cache every run schedules cold. For a single
step, `--cache-remote s3://ci-cache/rstest --cache-pull --cache-push` (an
`s3://` prefix works well on AWS-hosted agents) keeps scheduling warm.

**Sharding (`parallelism:`, shared cache).** Buildkite provides
`BUILDKITE_PARALLEL_JOB` (**0-based**) and `BUILDKITE_PARALLEL_JOB_COUNT`, so
add 1 to the index for [`--shard K/N`](sharding.md). One step snapshots the
cache into a build artifact, every parallel job runs against that copy,
default-branch jobs upload only the segment they wrote, and a final step
checks the shards covered the suite. `$$` escapes a shell `$` from
pipeline-upload interpolation:

```yaml
# .buildkite/pipeline.yml
steps:
  - label: ":package: cache snapshot"
    key: snapshot
    command: |
      mkdir -p rcache/segments
      aws s3 sync s3://ci-cache/rstest rcache
      tar czf rcache.tgz rcache
    artifact_paths: ["rcache.tgz"]

  - label: ":pytest: shard %n"
    key: test
    depends_on: snapshot
    parallelism: 4
    command: |
      pip install -r requirements.txt && pip install rstest
      buildkite-agent artifact download rcache.tgz . && tar xzf rcache.tgz
      ls rcache/segments > .warm-segs
      K=$$((BUILDKITE_PARALLEL_JOB + 1))
      code=0
      rstest -n 4 --shard "$$K/$$BUILDKITE_PARALLEL_JOB_COUNT" \
        --cache-remote rcache --cache-pull --cache-push --output buildkite \
        --report-json "shard.$$K.json" --junitxml "junit.$$K.xml" || code=$$?
      # Default branch only: publish the segment this shard wrote.
      if [ "$$BUILDKITE_BRANCH" = "$$BUILDKITE_PIPELINE_DEFAULT_BRANCH" ]; then
        for f in rcache/segments/seg-*.json; do
          [ -e "$$f" ] || continue
          grep -qxF "$$(basename "$$f")" .warm-segs || aws s3 cp "$$f" s3://ci-cache/rstest/segments/
        done
      fi
      exit $$code
    artifact_paths: ["junit.*.xml", "shard.*.json"]

  - label: ":white_check_mark: shard-verify"
    depends_on:
      - step: test
        allow_failure: true
    command: |
      pip install rstest
      buildkite-agent artifact download "shard.*.json" .
      rstest shard-verify shard.*.json
```

Give the write-capable IAM role only to agents that build your default branch;
other agents need read access for the snapshot only.

## Pre-commit

rstest ships [pre-commit](https://pre-commit.com) hooks so a suite runs
before code lands. Add to your project's `.pre-commit-config.yaml`:

```yaml
repos:
  - repo: https://github.com/KovantAI/rstest
    rev: v0.8.0             # pin a released tag
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
it would select zero tests and pass silently. On CI, `--changed` diffs
against the PR base instead: GitHub Actions sets `GITHUB_BASE_REF` and GitLab
sets `CI_MERGE_REQUEST_*` on pull/merge-request pipelines themselves (the full
list is in [Selecting changed tests](changed.md#ci-usage)).

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

### Point the hook at your project's interpreter

pre-commit installs these hooks into an isolated environment of their own and
sets `$VIRTUAL_ENV` to it while the hook runs. That environment has rstest but
none of your project's dependencies, and `$VIRTUAL_ENV` comes first in
[interpreter discovery](../getting-started/installation.md#which-python-does-rstest-use),
so without help the workers would run there and fail with
`ModuleNotFoundError` for your project's packages. When that happens rstest
prints a hint on stderr naming the interpreter it ran and the project `.venv`
it passed over. Tell rstest which interpreter to use:

```yaml
      - id: rstest
        args: ["--python", ".venv/bin/python"]   # Windows: .venv/Scripts/python.exe
```

`--python .venv` (the venv directory) works too. Or skip the hook environment
entirely with a local hook that runs the rstest already installed in your
project (rstest must be in that venv, which it already is if you followed
[Installation](../getting-started/installation.md)):

```yaml
repos:
  - repo: local
    hooks:
      - id: rstest
        name: rstest
        entry: rstest
        language: system
        pass_filenames: false
        types: [python]
        require_serial: true
        stages: [pre-push]
```

With `language: system` pre-commit doesn't set `$VIRTUAL_ENV`, so rstest
finds your `.venv` on its own (run `git push` from an activated venv, or
use `entry: .venv/bin/rstest`).

## Go deeper

- [CI quickstart](ci-quickstart.md): GitHub Actions and the Django and
  monorepo worked examples.
- [Shared cache across CI jobs](ci-shared-cache.md): the segment model and
  every transport, for a shard matrix.
- [Sharding across CI jobs](sharding.md): how partitions are computed.
- [Exit codes](../reference/exit-codes.md) · [Report JSON](../reference/report-json.md):
  the machine-readable surfaces to build gates on.
