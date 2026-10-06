# Shared cache across CI jobs

The plain [`actions/cache` recipe](ci-quickstart.md#github-actions) works for a
single job, but its per-key immutability forces the `run_id` key dance, and
across a shard matrix it needs a dedicated full-run job to own the cache.
rstest's [shared-cache backend](../concepts/caching.md#shared-cache-backend)
replaces both: every job pushes its own immutable **segment** and pulls the
union (no single writer, no key hacks).

This page is the reference for wiring that flow on each CI system. If you only
run one job (no shard matrix), you do not need this: the
[quickstart](ci-quickstart.md) `actions/cache` recipe is enough.

!!! tip "Turnkey via the composite action"
    On GitHub, the [`rstest` action](https://github.com/KovantAI/rstest/tree/main/.github/actions/rstest#warm-cache-as-a-service)
    wires the whole flow below for you: `cache-backend: artifact` does the
    resolve-run, download-segments, run, upload-segment bookends natively, and
    `cache-remote: s3://…` drives the object-store path. For a shard matrix,
    the ready-made recipe (one upstream job resolving the warm run, passed to
    every shard as `warm-run-id`) is in
    [Sharding: GitHub Actions](sharding.md#github-actions). The hand-wired
    YAML on this page is for other CI systems, or for full control.

Which backend fits which layout:

| Layout | Backend |
|---|---|
| One unsharded job, including its PR runs | `actions/cache` (the action's default `actions-cache` backend) is enough: a PR job restores the base branch's newest entry, and what it saves stays scoped to that PR |
| Shard matrix on GitHub | GitHub artifacts (the action's `cache-backend: artifact`, or the hand-wired recipe below). Not `actions/cache`: it keeps one blob per key, so it can't list and merge every shard's segment |
| Already on an object store or shared mount | `--cache-remote` (the action's `cache-remote`, or the [object-store layout](#object-store-s3gcsr2-oidc-no-secrets)) |

## Rules for every CI cache { #ci-cache-rules }

The recipes on this page, in [More CI systems](ci-recipes.md), and in
[Sharding](sharding.md) all follow these three rules.

### One snapshot per shard matrix { #one-snapshot-per-shard-matrix }

`--shard K/N` partitions from the duration cache each shard sees. If shards
pull and push the live cache (an object-store prefix, a shared mount), a shard
that finishes early pushes a segment that a later-starting shard then pulls,
the two compute different partitions, and tests can be dropped or run twice
while the build stays green
([why](sharding.md#keep-one-cache-snapshot-across-the-matrix)). Every sharded
recipe therefore:

1. fixes the cache **once**, before any shard starts: it copies the remote
   into a snapshot, resolves one warm run id or cache key in an upstream job,
   or restores one native-cache key read-only that only a job after the
   matrix saves;
2. runs each shard against that fixed copy (for a remote, a local copy:
   `--cache-remote rcache --cache-pull --cache-push`) with
   `--report-json shard.K.json`;
3. uploads only the segment each shard wrote, and only from trusted
   default-branch builds: whoever can push segments can steer a later
   `--changed` selection or `--reruns-only-known-flaky`
   ([trust boundary](../concepts/caching.md#trust-boundary));
4. gates on `rstest shard-verify shard.*.json` in a job after the matrix,
   which catches any divergence the steps above miss.

To keep the live segment set small, run
`rstest cache-compact --cache-remote <remote> --keep-last 50` on a schedule
from a default-branch job.

### Warm from full default-branch runs { #warm-from-full-default-branch-runs }

Run the workflow on pushes to your default branch too (a scheduled run works
as well): those runs publish what pull-request jobs and shards warm from. Keep
them **full** runs, not `--changed` runs: a backend that warms from one prior
run (the GitHub artifact backend) would otherwise hold durations and coverage
for the selected tests only. Warming from the latest **successful** run has
one side effect: while the default branch is red, every job keeps warming from
the last green run, so durations and the coverage index stop advancing until
it is fixed.

### Keep replay journals out of the cache { #keep-replay-journals-out-of-the-cache }

Every parallel run writes [replay journals](replay.md) to
`.rstest_cache/replay/` (up to 11 files, several MB each on a large suite;
`--shard` runs write none). A cache that carries them grows build after build,
and a build that recorded nothing can upload an older `latest.json` it
restored. The bundled action already leaves them out. Wherever you persist
`.rstest_cache` yourself, pick one:

- **Exclude or remove the directory** before the cache is saved
  (`!.rstest_cache/replay` for `actions/cache`), after any step that uploads
  `latest.json` as a failure artifact.
  `python -c "import shutil; shutil.rmtree('.rstest_cache/replay', True)"`
  works in bash, PowerShell and cmd.exe alike.
- **Cache only the files that matter** where the provider takes file paths:
  `durations.json`, `flakes.json`, `coverage_index.json`, `wall.json`,
  `last_green.json` and `incremental_outcomes.json` under `.rstest_cache/`.
- **Turn journaling off** with `RSTEST_NO_REPLAY_JOURNAL=1` if you won't
  replay CI failures.

## GitHub-native, no external cloud, no secrets

`download-artifact@v8`'s `pattern` + `merge-multiple` is exactly the
merge-all-segments primitive:

```yaml
permissions: { contents: read, actions: read }   # actions:read reaches prior-run artifacts
jobs:
  # Resolve the warm source ONCE, upstream of the matrix, so every shard warms
  # from the same prior run and partitions identically (see "Keep one cache
  # snapshot across the matrix" in the Sharding guide).
  resolve:
    runs-on: ubuntu-latest
    outputs:
      run-id: ${{ steps.warm.outputs.run-id }}
    steps:
      # Its shard segments union into a full index.
      --8<-- "docs/_snippets/warm-run-step.md"

  test:
    needs: resolve
    runs-on: ubuntu-latest
    strategy: { matrix: { shard: [1, 2, 3, 4] } }
    steps:
      - uses: actions/checkout@v7
      - uses: actions/setup-python@v7
        with: { python-version: "3.13" }
      - run: pip install -r requirements.txt && pip install rstest

      # Pull: a plain download-artifact only sees the CURRENT run; run-id +
      # github-token reach the prior run resolved above.
      # Land the warmed segments in ./rcache/segments/, which is where rstest
      # reads them (--cache-remote <dir> looks in <dir>/segments/). upload-artifact
      # strips the segments/ prefix from the pushed glob, so aim the download at
      # .../segments to reconstruct the layout.
      - uses: actions/download-artifact@v8
        if: needs.resolve.outputs.run-id != ''
        with:
          # Scope the prefix to this suite + interpreter (see "One prefix per
          # suite" below). The "--" stops a prefix matching a longer one.
          pattern: "rstest-seg-py3.13--*"
          merge-multiple: true
          path: ./rcache/segments
          github-token: ${{ github.token }}
          run-id: ${{ needs.resolve.outputs.run-id }}
        continue-on-error: true          # cold start: nothing to warm from yet
      # Record the warmed segment names so the push below uploads only THIS run's
      # new ones, not the whole warmed union (which would grow every run).
      - run: ls ./rcache/segments/seg-*.json 2>/dev/null | xargs -rn1 basename | sort > .warm-segs || true

      # --cov-context=test rides the segment too: each shard pushes its partial
      # coverage slice, and the next run's pull unions them into a full index
      # that --changed consumes. --cov-report= suppresses the textual report (we
      # want only the index side-effect). Drop the --cov flags if you don't use
      # --changed. Replace YOUR_PACKAGE with your importable package/source dir.
      - run: rstest -n 4 --shard ${{ matrix.shard }}/4
               --cov=YOUR_PACKAGE --cov-context=test --cov-report=
               --cache-remote ./rcache --cache-pull --cache-push
               --report-json shard.${{ matrix.shard }}.json
               --junitxml junit.${{ matrix.shard }}.xml
      - uses: actions/upload-artifact@v7
        if: always()
        with:
          name: shard-report-${{ matrix.shard }}
          overwrite: true                # a re-run of this shard replaces it
          path: shard.${{ matrix.shard }}.json

      # Push: stage only the segment(s) this run wrote (absent from .warm-segs),
      # so each shard's artifact is its own disjoint delta, no collision on the
      # next merge-multiple, no unbounded re-upload of the warmed union.
      - run: |
          mkdir -p ./push
          for f in ./rcache/segments/seg-*.json; do
            [ -e "$f" ] || continue
            grep -qxF "$(basename "$f")" .warm-segs 2>/dev/null || cp "$f" ./push/
          done
        if: always()
      - uses: actions/upload-artifact@v7
        if: always()
        with:
          # run_attempt: a re-run of a failed shard must not reuse the first
          # attempt's name (artifact names are unique within a run).
          name: rstest-seg-py3.13--${{ github.run_id }}-${{ github.run_attempt }}-${{ matrix.shard }}
          path: ./push/seg-*.json
          if-no-files-found: ignore

  # Gate: fail unless the shards ran every collected test exactly once.
  verify:
    needs: test
    if: always()
    runs-on: ubuntu-latest
    steps:
      - uses: actions/download-artifact@v8
        with:
          pattern: shard-report-*
          merge-multiple: true
      - uses: actions/setup-python@v7
        with: { python-version: "3.13" }
      - run: pip install rstest && rstest shard-verify shard.*.json
```

No refresh job, no `run_id`/`restore-keys` dance, no single writer: each shard
contributes its segment (durations, flake events, **and** its share of the
coverage index). The `resolve` job and pull step above warm from the latest
successful default-branch run, whose shard segments union into a whole
`--changed` index (no dedicated unsharded job), so run this workflow on full
pushes to your default branch too
([why](#warm-from-full-default-branch-runs)). The first run, or any cold pull,
has nothing to union and falls back to the import graph (correct, only
coarser). Artifact retention gives free segment eviction.

The warm source is pinned to **push** runs on `main` (`--branch main --event
push` above), so segments a `pull_request` run uploads are never read back by
anyone. Keep that filter if PRs come from forks or less-trusted branches; see
[Trust boundary](../concepts/caching.md#trust-boundary).

`--status success` only ever picks a **green** run, so a long red streak on
main means scheduling from increasingly stale timings. Use `--status
completed` instead if you would rather warm from the newest finished run, red
or not.

!!! note "How the cross-run pull works"
    Artifacts are run-scoped, so warming reaches back to **one** prior run by id.
    `gh run list` resolves the latest successful one above (the REST API `GET
    /repos/{owner}/{repo}/actions/artifacts` is the alternative). One complete
    sharded run is enough: its `N` shard segments union into a full index. To
    fold *many* runs instead, add a scheduled job that runs
    `rstest cache-compact --cache-remote ./rcache` over the downloaded segments
    and uploads the resulting `./rcache/base.json` as its own artifact. rstest
    reads the base only from `<remote>/base.json`, so each PR job needs an extra
    download step that puts that artifact at `./rcache/base.json` next to
    `./rcache/segments/`; the segment download above does not fetch it.

## Object store (S3/GCS/R2), OIDC: no secrets

For teams already on cloud storage, point `--cache-remote` straight at the
bucket: rstest drives the `aws` / `gcloud` CLI the runner already has, with
credentials from the OIDC role (no SDK). Immutable, uniquely-named segments
make concurrent pushes safe. For a single, unsharded job that is the whole
recipe:

```yaml
permissions: { id-token: write, contents: read }
steps:
  - uses: aws-actions/configure-aws-credentials@v4
    with: { role-to-assume: arn:aws:iam::…:role/ci, aws-region: us-east-1 }
  - run: rstest -n auto --cache-remote s3://ci-cache/rstest --cache-pull --cache-push
           --cache-compact-threshold 50
```

**A shard matrix needs the snapshot layout** from
[One snapshot per shard matrix](#one-snapshot-per-shard-matrix): one job
snapshots the prefix, shards run against that local copy and upload only their
new segments, and a final job pushes them to the bucket, only for pushes to
`main`:

```yaml
permissions: { id-token: write, contents: read }
jobs:
  # Snapshot the remote ONCE; every shard reads this exact copy. A read-only
  # role is enough here.
  snapshot:
    runs-on: ubuntu-latest
    steps:
      - uses: aws-actions/configure-aws-credentials@v4
        with: { role-to-assume: arn:aws:iam::…:role/ci-cache-read, aws-region: us-east-1 }
      - run: mkdir -p ./rcache && aws s3 sync s3://ci-cache/rstest ./rcache
      - uses: actions/upload-artifact@v7
        with:
          name: rstest-snapshot
          path: ./rcache
          if-no-files-found: ignore   # cold start: empty prefix
          overwrite: true             # a re-run of this job replaces it

  test:
    needs: snapshot
    runs-on: ubuntu-latest
    strategy: { matrix: { shard: [1, 2, 3, 4] } }
    steps:
      - uses: actions/checkout@v7
      - uses: actions/setup-python@v7
        with: { python-version: "3.13" }
      - run: pip install -r requirements.txt && pip install rstest
      # No cloud credentials in the shards: they only read the snapshot.
      - uses: actions/download-artifact@v8
        with: { name: rstest-snapshot, path: ./rcache }
        continue-on-error: true       # cold start: no snapshot yet
      - run: mkdir -p ./rcache/segments && ls ./rcache/segments > .warm-segs
      - run: rstest -n 4 --shard ${{ matrix.shard }}/4
               --cache-remote ./rcache --cache-pull --cache-push
               --report-json shard.${{ matrix.shard }}.json
               --junitxml junit.${{ matrix.shard }}.xml
      # Stage only the segment this shard wrote.
      - run: |
          mkdir -p ./push
          for f in ./rcache/segments/seg-*.json; do
            [ -e "$f" ] || continue
            grep -qxF "$(basename "$f")" .warm-segs || cp "$f" ./push/
          done
        if: always()
      - uses: actions/upload-artifact@v7
        if: always()
        with:
          name: rstest-newseg-${{ github.run_attempt }}-${{ matrix.shard }}
          path: ./push/seg-*.json
          if-no-files-found: ignore
      - uses: actions/upload-artifact@v7
        if: always()
        with:
          name: shard-report-${{ matrix.shard }}
          overwrite: true             # a re-run of this shard replaces it
          path: shard.${{ matrix.shard }}.json

  # Gate: fail unless the shards ran every collected test exactly once.
  verify:
    needs: test
    if: always()
    runs-on: ubuntu-latest
    steps:
      - uses: actions/download-artifact@v8
        with:
          pattern: shard-report-*
          merge-multiple: true
      - uses: actions/setup-python@v7
        with: { python-version: "3.13" }
      - run: pip install rstest && rstest shard-verify shard.*.json

  # The only writer: pushes to main. PR runs never get the write role.
  publish:
    needs: test
    if: always() && github.event_name == 'push' && github.ref == 'refs/heads/main'
    runs-on: ubuntu-latest
    steps:
      - uses: actions/download-artifact@v8
        with:
          pattern: rstest-newseg-*
          merge-multiple: true
          path: ./push
        continue-on-error: true
      - uses: aws-actions/configure-aws-credentials@v4
        with: { role-to-assume: arn:aws:iam::…:role/ci-cache-write, aws-region: us-east-1 }
      - run: |
          [ -d ./push ] || exit 0
          aws s3 cp ./push s3://ci-cache/rstest/segments/ --recursive
```

The shards' `--cache-push` writes into their local `./rcache` only, so no
shard's write can change what another shard reads, and a re-run of one failed
shard downloads the same snapshot and gets the same partition. Split the IAM
role in two: `ci-cache-read` (`s3:ListBucket` + `s3:GetObject` on the prefix)
for the snapshot job and any PR job, and `ci-cache-write` (adds
`s3:PutObject`, plus `s3:DeleteObject` if it compacts) that only the `main`
branch can assume. On AWS, restrict the write role's trust policy to the
`main` ref (the OIDC `sub` claim `repo:<owner>/<repo>:ref:refs/heads/main`), so
the `if:` above is not the only guard. Make the `verify` job a required check:
[`rstest shard-verify`](sharding.md#verify-no-test-was-dropped) fails when the
shards dropped or duplicated a test.

To keep the segment set small, compact from the `publish` job or a schedule:
`rstest cache-compact --cache-remote s3://ci-cache/rstest --keep-last 50`
(needs rstest installed there), or pass `--cache-compact-threshold` on the
unsharded recipe above. Keep that threshold low (20 to 50) on `s3://` and
`gs://`: a pull reads each loose segment with its own `aws` / `gcloud` process,
one after another, so a threshold of 500 can mean up to about 500 CLI
processes before the first test runs. A `gs://` bucket works the same via
`gcloud`; an authenticated `https://` endpoint via `RSTEST_CACHE_REMOTE_TOKEN`
(use `https://`, not plain `http://`: the bearer token is sent with every
request and plain HTTP carries it in cleartext).

## Self-hosted shared mount: zero glue

`--cache-remote /mnt/ci-cache/rstest` directly; the mount is the remote, with no
pull/push bookends beyond the flags.

## Reliability

A failed pull already stops the run (exit 1). Add `--require-baseline` to
`--durations-regress` so a cold remote, one that leaves no duration baseline,
is a hard error too, never a silent green:

```console
$ rstest -n auto --cache-remote ./rcache --cache-pull --require-baseline --durations-regress 1.5
```

**Pull and push fail differently.** A failed `--cache-pull` (an unreachable
endpoint, expired credentials, a listing or read error) aborts the run with
exit `1` **before any test runs**:

```text
Error: pulling shared cache from https://cache.example.com/rstest
```

A failed `--cache-push` only warns (`rstest: cache: push failed: ...`) and
keeps the run's own exit code. So a remote outage turns every pulling job red
even though no test failed. If your CI can't tolerate that, retry the step
(most CI systems have a step- or job-level retry), or rerun without
`--cache-pull` only when the pull itself was the failure:

```bash
set -o pipefail
code=0
rstest -n 4 --cache-remote "$REMOTE" --cache-pull 2>&1 | tee rstest.log || code=${PIPESTATUS[0]}
# Retry cold only when the pull itself failed; any other failure keeps its code.
if [ "$code" -ne 0 ] && grep -q '^Error: pulling shared cache' rstest.log; then
  rstest -n 4 && code=0 || code=$?
fi
exit "$code"
```

The retried run is cold (even count split), which matters for a shard matrix:
a shard that falls back partitions differently from its siblings, so gate
such a matrix with [`shard-verify`](sharding.md#verify-no-test-was-dropped). A
cold, empty remote is not a failure: the pull succeeds with nothing to merge.

## One prefix per suite, interpreter, and project

Nodeids in the cache are **project-relative** (`tests/test_x.py::test_x`)
and carry no interpreter tag. Give each distinct suite its own remote prefix
(or artifact name), or their entries collide and mix:

- a monorepo matrix with one job per package: `s3://ci-cache/rstest/libs-core`,
  `s3://ci-cache/rstest/libs-cli`, …
- a Python-version or OS matrix: add the version, e.g.
  `s3://ci-cache/rstest/py3.13`, since durations and flakiness differ per
  interpreter.
- the GitHub artifact backend: put the scope in the artifact name, before a
  `--` separator, as the example above does (`rstest-seg-py3.13--<run_id>-<attempt>-<shard>`,
  downloaded with `pattern: "rstest-seg-py3.13--*"`). The bundled action
  does this for you via its `artifact-suffix` input (default
  `<os>-py<version>[-<working-directory>]`).

## Permissions

The remote needs **list + read + write** on the cache prefix, plus **delete**
only for a job that compacts (`--cache-compact-threshold` or a
`cache-compact` step). Scope the credential to the prefix, not the whole
bucket, and give write access only to default-branch jobs. The exact grant per
backend (S3, GCS, Azure Blob, `http(s)://`, a mount, GitHub artifacts) is in
[Caching: permissions](../concepts/caching.md#cache-permissions).

## Per-CI-system shared-cache recipes

Each provider's `--cache-remote` wiring (object store, blob, mount) lives with
its recipe in [More CI systems](ci-recipes.md): [AWS CodeBuild](ci-recipes.md#aws-codebuild),
[Google Cloud Build](ci-recipes.md#google-cloud-build), [GitLab CI](ci-recipes.md#gitlab-ci),
[Azure Pipelines](ci-recipes.md#azure-pipelines), [CircleCI](ci-recipes.md#circleci),
[Jenkins](ci-recipes.md#jenkins), and [Buildkite](ci-recipes.md#buildkite). Each
sharded recipe there follows the snapshot layout above.

## Go deeper

- [Sharding across CI jobs](sharding.md): how partitions are computed and the
  identical-cache-snapshot rule every shard must obey.
- [Caching](../concepts/caching.md): the segment model, transports, and compaction.
- [CI quickstart](ci-quickstart.md): the single-job starting point.
