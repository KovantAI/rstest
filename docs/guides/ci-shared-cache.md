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
    is the reference for other CI systems (or if you want full control).

## GitHub-native, no external cloud, no secrets

`download-artifact@v8`'s `pattern` + `merge-multiple` is exactly the
merge-all-segments primitive:

```yaml
permissions: { contents: read, actions: read }   # actions:read reaches prior-run artifacts
jobs:
  test:
    runs-on: ubuntu-latest
    strategy: { matrix: { shard: [1, 2, 3, 4] } }
    steps:
      - uses: actions/checkout@v7
      - uses: actions/setup-python@v7
        with: { python-version: "3.13" }
      - run: pip install -r requirements.txt && pip install rstest

      # Pull: warm from the latest successful run on your default branch; its
      # shard segments union into a full index. A plain download-artifact only
      # sees the CURRENT run; run-id + github-token reach a prior run's artifacts.
      - name: resolve warm-cache run
        id: warm
        env:
          GH_TOKEN: ${{ github.token }}
        run: |
          rid=$(gh run list --repo "$GITHUB_REPOSITORY" \
                  --workflow "${{ github.workflow }}" --branch main --event push \
                  --status success --limit 1 \
                  --json databaseId --jq '.[0].databaseId // ""')
          echo "run-id=$rid" >> "$GITHUB_OUTPUT"
        continue-on-error: true
      # Land the warmed segments in ./rcache/segments/, which is where rstest
      # reads them (--cache-remote <dir> looks in <dir>/segments/). upload-artifact
      # strips the segments/ prefix from the pushed glob, so aim the download at
      # .../segments to reconstruct the layout.
      - uses: actions/download-artifact@v8
        if: steps.warm.outputs.run-id != ''
        with:
          # Scope the prefix to this suite + interpreter (see "One prefix per
          # suite" below). The "--" stops a prefix matching a longer one.
          pattern: "rstest-seg-py3.13--*"
          merge-multiple: true
          path: ./rcache/segments
          github-token: ${{ github.token }}
          run-id: ${{ steps.warm.outputs.run-id }}
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
          name: rstest-seg-py3.13--${{ github.run_id }}-${{ matrix.shard }}
          path: ./push/seg-*.json
          if-no-files-found: ignore
```

No refresh job, no `run_id`/`restore-keys` dance, no single writer: each shard
contributes its segment (durations, flake events, **and** its share of the
coverage index). The resolve-and-pull step above warms from the latest
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

The pull and push in this example run on every event, which is fine for a
private repo with trusted contributors. If PRs come from forks or
less-trusted branches, make PR jobs pull-only; see
[Trust boundary](../concepts/caching.md#trust-boundary).

!!! note "How the cross-run pull works"
    Artifacts are run-scoped, so warming reaches back to **one** prior run by id.
    `gh run list` resolves the latest successful one above (the REST API `GET
    /repos/{owner}/{repo}/actions/artifacts` is the alternative). One complete
    sharded run is enough: its `N` shard segments union into a full index. To
    fold *many* runs instead, add a scheduled job that `cache-compact`s the
    segments into a base and uploads that base as its own artifact for PR jobs to
    pull.

## Object store (S3/GCS/R2), OIDC: no secrets

For teams already on cloud storage, point `--cache-remote` straight at the
bucket: rstest drives the `aws` / `gcloud` CLI the runner already has, with
credentials from the OIDC role (no `sync` bookends, no SDK). Immutable,
uniquely-named segments make concurrent shard pushes safe:

```yaml
permissions: { id-token: write, contents: read }
steps:
  - uses: aws-actions/configure-aws-credentials@v4
    with: { role-to-assume: arn:aws:iam::…:role/ci, aws-region: us-east-1 }
  - run: rstest -n 4 --shard ${{ matrix.shard }}/4
           --cache-remote s3://ci-cache/rstest --cache-pull --cache-push
           --cache-compact-threshold 500
```

`--cache-compact-threshold` folds the segment set inline once it grows past the
threshold, so no separate maintenance job is needed (or run `rstest cache-compact
--cache-remote s3://ci-cache/rstest --keep-last 200` on a schedule instead). A
`gs://` bucket works the same via `gcloud`; an authenticated `https://` endpoint
via `RSTEST_CACHE_REMOTE_TOKEN`. Still prefer syncing to a local dir? The
`aws s3 sync … ./rcache` / `--cache-remote ./rcache` form remains valid.

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
  `--` separator, as the example above does (`rstest-seg-py3.13--<run_id>-<shard>`,
  downloaded with `pattern: "rstest-seg-py3.13--*"`). The bundled action on
  `main` does this for you via its `artifact-suffix` input (default
  `<os>-py<version>[-<working-directory>]`). **Unreleased:** that input is not
  in the `@v0.7.0` action; it ships in 0.8.0.

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
