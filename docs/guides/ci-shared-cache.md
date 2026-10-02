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
    resolve-run → download-segments → run → upload-segment bookends natively, and
    `cache-remote: s3://…` drives the object-store path. The hand-wired YAML here
    is the reference for other CI systems (or if you want full control). The
    action resolves the warm run, or pulls and pushes the remote, inside each
    job, so in a shard matrix each shard picks its own snapshot; for a gating
    pipeline, use the upstream-resolve layouts below.

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
      # Warm from the latest successful push run on your default branch; its
      # shard segments union into a full index.
      - name: resolve warm-cache run
        id: warm
        env:
          GH_TOKEN: ${{ github.token }}
        run: |
          # Match this workflow by file name, not display name (two workflows
          # can share a `name:`); GITHUB_WORKFLOW_REF is owner/repo/.github/workflows/<file>@ref.
          wf="${GITHUB_WORKFLOW_REF##*/.github/workflows/}"; wf="${wf%%@*}"
          rid=$(gh run list --repo "$GITHUB_REPOSITORY" \
                  --workflow "$wf" --branch main --event push \
                  --status success --limit 1 \
                  --json databaseId --jq '.[0].databaseId // ""')
          echo "run-id=$rid" >> "$GITHUB_OUTPUT"
        continue-on-error: true

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
      # --changed. Replace <your_package> with your importable package/source dir.
      - run: rstest -n 4 --shard ${{ matrix.shard }}/4
               --cov=<your_package> --cov-context=test --cov-report=
               --cache-remote ./rcache --cache-pull --cache-push
               --junitxml junit.${{ matrix.shard }}.xml

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
```

No refresh job, no `run_id`/`restore-keys` dance, no single writer: each shard
contributes its segment (durations, flake events, **and** its share of the
coverage index). The `resolve` job and pull step above warm from the latest
successful default-branch run, whose shard segments union into a whole
`--changed` index (no dedicated unsharded job). **Run this workflow on pushes to
your default branch too**, so those runs publish the segments PR jobs warm from
(a scheduled run works as well). The first run, or any cold pull, has nothing to
union and falls back to the import graph (correct, only coarser). Artifact
retention gives free segment eviction.

Those default-branch runs must be **full runs**, not `--changed` runs. The
artifact backend warms from exactly one prior run, so if that run only
executed the tests `--changed` picked, the warm cache holds durations and
coverage for those tests only.

The warm source is pinned to **push** runs on `main` (`--branch main --event
push` above), so segments a `pull_request` run uploads are never read back by
anyone. Keep that filter if PRs come from forks or less-trusted branches; see
[Trust boundary](../concepts/caching.md#trust-boundary).

`--status success` only ever picks a **green** run. While main is red, every
shard keeps warming from the last green run, so durations and the coverage
index stop advancing until main is fixed (and a long red streak means
scheduling from increasingly stale timings). Use `--status completed` instead
if you would rather warm from the newest finished run, red or not.

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

**A shard matrix needs two more things.** First, every shard must partition
from the **same** cache snapshot: if shards pull and push the live prefix, a
shard that finishes early pushes a segment that a later-starting shard then
pulls, and their partitions disagree (see
[Keep one cache snapshot across the matrix](sharding.md#keep-one-cache-snapshot-across-the-matrix)).
Second, only trusted runs should write: whoever can push segments can steer a
later `--changed` selection or `--reruns-only-known-flaky`
([Trust boundary](../concepts/caching.md#trust-boundary)). The layout below
handles both: one job snapshots the prefix, shards run against that local copy
and upload only their new segments, and a final job pushes them to the bucket,
only for pushes to `main`:

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
the `if:` above is not the only guard. Gate the merge on
[`rstest shard-verify shard.*.json`](sharding.md#verify-no-test-was-dropped).

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

Add `--require-baseline` to `--durations-regress` so a cold or failed pull is a
hard error, never a silent green:

```console
$ rstest -n auto --cache-remote ./rcache --cache-pull --require-baseline --durations-regress 1.5
```

(`actions/cache` is **not** recommended for this. It keeps one blob per key, so
it can't list-and-merge every segment, which is the exact limitation this design
removes.)

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
if ! rstest -n 4 --cache-remote "$REMOTE" --cache-pull 2>&1 | tee rstest.log; then
  # Test failures fall through (grep finds nothing, the step stays red).
  grep -q '^Error: pulling shared cache' rstest.log && rstest -n 4
fi
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

The remote needs **list + read + write + delete** on the cache prefix. Delete
only when a job compacts (`--cache-compact-threshold` or a `cache-compact`
step); pull/push-only jobs can drop it. Scope the credential to the prefix, not
the whole bucket. Per backend ([full table](../concepts/caching.md#transports)):

| Backend | What to grant |
|---|---|
| GitHub `artifact` | workflow `permissions: { contents: read, actions: read }` (`actions: read` reaches the prior run's segments). Object store instead? add `id-token: write` for OIDC. |
| S3 | role/keys with `s3:ListBucket` + `s3:{Get,Put,Delete}Object` on `bucket/prefix/*` (via CodeBuild role, GitHub/CircleCI OIDC, or GitLab CI vars) |
| GCS | service account with `storage.objects.{list,get,create,delete}` on the bucket/prefix (`roles/storage.objectAdmin`) |
| Azure Blob | `Storage Blob Data Contributor` on the container (dir-materialize via the `az` CLI) |
| `http(s)://` | a token in `RSTEST_CACHE_REMOTE_TOKEN`; the endpoint enforces authz |
| dir / shared mount | filesystem read+write+delete on the directory |

## Per-CI-system shared-cache recipes

Each provider's `--cache-remote` wiring (object store, blob, mount) lives with
its recipe in [More CI systems](ci-recipes.md): [AWS CodeBuild](ci-recipes.md#aws-codebuild),
[Google Cloud Build](ci-recipes.md#google-cloud-build), [GitLab CI](ci-recipes.md#gitlab-ci),
[Azure Pipelines](ci-recipes.md#azure-pipelines), [CircleCI](ci-recipes.md#circleci),
and [Jenkins](ci-recipes.md#jenkins).

## Go deeper

- [Sharding across CI jobs](sharding.md): how partitions are computed and the
  identical-cache-snapshot rule every shard must obey.
- [Caching](../concepts/caching.md): the segment model, transports, and compaction.
- [CI quickstart](ci-quickstart.md): the single-job starting point.
