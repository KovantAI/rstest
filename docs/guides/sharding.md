# Sharding across CI jobs (`--shard K/N`)

`--shard K/N` splits one suite across `N` independent CI jobs. Job `K`
runs only its `1/N` slice; the jobs never talk to each other. Each job
partitions the collected tests into `N` balanced buckets and keeps
bucket `K` (K is **1-based**: `1/4`, `2/4`, `3/4`, `4/4`).

```console
$ rstest -n 4 --shard 2/4 --junitxml junit.2.xml
```

(Examples use an explicit `-n 4`, the vCPU count of a standard GitHub
runner; see the limits note below for why not `-n auto`.)

This is the fan-out for the common case: **one large suite is the long
pole**, and you want it spread across a runner matrix. It is orthogonal
to `-n` (each shard still runs its slice across local workers) and to
[monorepo mode](monorepo.md), which splits *across projects on one box*.
Sharding splits *one suite across many boxes*.

## How the split stays balanced

Buckets are balanced by the **duration cache**
(`.rstest_cache/durations.json`) using longest-processing-time-first
bin-packing: the slowest tests are placed first, each into the currently
lightest bucket. So a suite with a few dominating tests still splits into
even *wall-time* slices, not even *counts*.

The split is deterministic: given the same test list and the same
duration cache, every job computes the identical partition, which is
what lets `N` jobs agree on who runs what with zero coordination.

Two consequences for CI:

- **Restore the same duration cache on every shard.** If job 2 sees a
  different cache than job 3, their partitions can overlap or drop tests.
  Restore one shared cache key across the matrix (recipes below), and, for a
  gating pipeline, prove coverage with the check in
  [Verify no test was dropped](#verify-no-test-was-dropped). Every shard run
  also writes its timings to the local `.rstest_cache`, so running several
  shards one after another in one checkout gives each a different cache:
  restore the same snapshot before each shard.
- **A cold cache falls back to an even count split** (round-robin). The
  first run is balanced by count; from the second run on (once the cache
  is populated and restored) it balances by wall time.

When every shard sees the same test list and the same duration cache, the
buckets are **disjoint and cover the whole suite**, so merging the per-shard
JUnit reconstructs the full run. If the caches differ, that guarantee is gone;
see [Verify no test was dropped](#verify-no-test-was-dropped).

### Keep one cache snapshot across the matrix

"Restore the same cache" is harder than it sounds, because each shard pulls
at its own start time:

- **Shared-cache remote backend** (`--cache-remote … --cache-pull --cache-push`):
  a fast shard can finish and push its segment before a slow shard starts
  pulling. The slow shard then sees newer durations and computes a different
  partition.
- **Artifact backend**: each shard looks up "the latest successful main run"
  on its own. If a main run finishes while the matrix is starting, shards can
  warm from different runs.

To pin one snapshot:

- Resolve the warm source **once** in an upstream job (the `gh run list`
  step, or an `actions/cache/restore` lookup) and pass the run id or cache key
  to every shard as a job output. Recipes:
  [GitHub Actions](#github-actions) below (the action's `warm-run-id` input,
  or a resolved `actions/cache` key) and the hand-wired
  [artifact backend](ci-shared-cache.md#github-native-no-external-cloud-no-secrets).
- Or snapshot the remote once, have shards read that copy, and push their new
  segments from a single follow-up job, so no shard's write can change what
  another shard reads:
  [object-store recipe](ci-shared-cache.md#object-store-s3gcsr2-oidc-no-secrets).
- Add `--report-json shard.K.json` to every shard and gate the merge job on
  [`rstest shard-verify`](#verify-no-test-was-dropped), which catches any
  divergence these steps miss.

Resolving "the latest **successful** main run" (`gh run list --status
success`) has one side effect: while main is red, every run keeps warming from
the last green run, so the timings stop advancing until main is fixed.

!!! note "Requirements & limits"
    - Needs the parallel pool: `-n ≥ 2`. Prefer an explicit `-n 2` or higher
      over `-n auto` with `--shard`: `auto` is capped by the number of test
      files and by the cached suite time (about one worker per 2s of tests),
      so on a 1-vCPU runner, a one-file suite, or a warm cache under ~2s it
      resolves to one worker and the run fails with `--shard needs the
      parallel pool` (exit 1). A cold first run can pass and the next, warm
      run fail.
    - Not combinable with `--shuffle` (a per-run shuffle would break the
      identical-partition guarantee) or `--dist each`.
    - Works with `--collect lazy` too, where it shards at **file**
      granularity (coarser balance).
    - Under an affinity `--dist` mode (`loadfile` / `loadscope` /
      `loadgroup`) it partitions at **whole-group** granularity: a
      file / scope / `xdist_group` moves as one unit and never splits
      across shards, preserving the run-together / in-order contract
      those modes exist to provide.
    - Composes with `--changed`: selection narrows the file set first,
      then the shard partitions the survivors.
    - **A sharded coverage index is partial per job.** Each shard measures only
      its own tests. Push each shard's slice through the
      [shared cache](../concepts/caching.md#shared-cache-backend)
      (`--cache-remote … --cache-pull --cache-push`): the shards share a commit,
      so their slices **union on pull** into a full index and a later `--changed`
      selects correctly with no dedicated unsharded job. Without the shared cache,
      warm the index from an **unsharded** run (or merge each shard's
      `.coverage`). See [keeping the index warm](changed.md#keeping-the-index-warm).

## Verify no test was dropped

The disjoint-and-covers-everything guarantee holds **only** when every shard
partitions the identical `(test list, duration cache, K, N)`. The way it breaks
in practice is a divergent duration cache across jobs (above): buckets then
overlap or drop tests, and because the jobs never talk to each other, a dropped
test is simply never run. The merged report is short, yet the build can still
go green with fewer tests than the suite has. Nothing detects that at runtime.

For a merge-queue or release gate, add a step that proves the shards covered
the whole suite. The built-in [`rstest shard-verify`](../reference/cli-commands.md#shard-verify)
does exactly this.

Each shard's `--report-json`, written while `--shard` was
active, carries a `meta.shard` stamp: `k`, `n`, and the sha256
`collection_hash` and size of the full collected suite. Each shard writes its
report while sharding:

```console
$ rstest -n 4 --shard "$K/$N" --report-json "shard.$K.json" --junitxml "junit.$K.xml"
```

After the matrix finishes, a job that has gathered all the shard reports passes
them to `shard-verify`, which reconciles them:

```console
$ rstest shard-verify shard.*.json
```

It exits `0` only when the shards agree on one collection (same
`collection_hash`, `n`, and size), the shard set is exactly `1..=N` once each,
and the union of what they ran equals the collection with no test on two shards.
On any drop, overlap, missing or duplicate shard, or a divergent collection, it
prints what went wrong and exits `1`, failing the gate:

```text
FAILED shard-verify: coverage is INCOMPLETE
  - shards ran 4180 of 4200 collected tests; 20 were dropped (ran on no shard)
```

`shard-verify` needs no interpreter and reads only the JSON files, so it runs in
a lightweight final job. It covers full-collection runs; a `--collect lazy`
shard run stamps no collection hash and is not verifiable this way.

??? note "Manual equivalent with jq (no shard-verify)"
    If you cannot run `shard-verify` (a policy against extra tooling),
    reconcile by hand. Collect the full suite once with the **same** selection
    flags the shards use, union the per-shard ran-ids, and compare.
    The report-json `tests` map is keyed by every test that ran (including
    skipped and xfailed), so the union is complete.

    ```bash
    rstest --collect-only --report-json discovery.json
    jq -r '.tests[].nodeid' discovery.json | sort -u > collected.ids
    jq -r '.tests | keys[]'  shard.*.json  | sort    > ran.ids
    # Dropped or added tests:
    if ! diff <(sort -u ran.ids) collected.ids >/dev/null; then
      echo "shard coverage mismatch: tests dropped or added" >&2; exit 1
    fi
    # A test that ran on two shards:
    if [ "$(wc -l < ran.ids)" -ne "$(sort -u ran.ids | wc -l)" ]; then
      echo "shard coverage overlap: a test ran on more than one shard" >&2; exit 1
    fi
    ```

## GitHub Actions

The bundled [`rstest` action][action] does the wiring: `shard` and
`shard-total` pass `--shard K/N`, and `cache-backend: artifact` gives every
shard the [shared cache](ci-shared-cache.md) over GitHub artifacts (no
external store, no secrets). Each shard pushes its own segment, and the next
run warms from the union, so there is no single-writer job and no dedicated
full run. An upstream job resolves the warm run **once** and passes it to
every shard as `warm-run-id`, so all shards partition from the same snapshot
([why](#keep-one-cache-snapshot-across-the-matrix)). A final job proves the
shards covered the suite.

```yaml
permissions: { contents: read, actions: read }   # actions:read reaches prior-run artifacts
jobs:
  # Resolve the warm source ONCE: the latest green push run on main. Every
  # shard warms from this run, so they all compute the same partition.
  warm:
    runs-on: ubuntu-latest
    outputs:
      run-id: ${{ steps.r.outputs.run-id }}
    steps:
      - id: r
        env:
          GH_TOKEN: ${{ github.token }}
          WF_REF: ${{ github.workflow_ref }}
        run: |
          wf="${WF_REF##*/.github/workflows/}"; wf="${wf%%@*}"
          rid=$(gh run list --repo "$GITHUB_REPOSITORY" --workflow "$wf" \
                  --branch main --event push --status success --limit 1 \
                  --json databaseId --jq '.[0].databaseId // ""')
          echo "run-id=$rid" >> "$GITHUB_OUTPUT"

  test:
    needs: warm
    runs-on: ubuntu-latest
    strategy:
      fail-fast: false
      matrix:
        shard: [1, 2, 3, 4]
    steps:
      - uses: actions/checkout@v7
      - uses: KovantAI/rstest/.github/actions/rstest@v0.8.0
        with:
          python-version: "3.13"
          cache-backend: artifact
          warm-run-id: ${{ needs.warm.outputs.run-id }}   # empty = cold start
          shard: ${{ matrix.shard }}
          shard-total: 4
          # Explicit -n: auto can resolve to one worker, which --shard rejects.
          args: "-n 4 --report-json shard.${{ matrix.shard }}.json"
          upload-junit: true
      - uses: actions/upload-artifact@v7
        if: always()
        with:
          name: shard-report-${{ matrix.shard }}
          overwrite: true           # a re-run of this shard replaces its report
          path: shard.${{ matrix.shard }}.json

  verify:
    needs: test
    runs-on: ubuntu-latest
    if: always()
    steps:
      - uses: actions/download-artifact@v8
        with:
          pattern: shard-report-*
          merge-multiple: true
      # Gate: fail unless the shards covered the whole suite exactly once.
      - uses: actions/setup-python@v7
        with: { python-version: "3.13" }
      - run: pip install rstest && rstest shard-verify shard.*.json
```

Run this workflow on pushes to `main` too: those runs publish the segments
that the `warm` job finds, and one complete sharded run unions into a full
cache. Keep the `main` runs full (not `changed: true`), since the artifact
backend warms from exactly one prior run. The first run is cold and every
shard uses the same even count split; from the second run on, shards balance
by wall time.

Re-running one failed shard is safe: the `warm` job isn't re-run, so the
shard warms from the same run id and recomputes the same partition, and
`overwrite: true` replaces its report, so `shard-verify` sees exactly one
report per shard. The action uploads each shard's JUnit as
`rstest-junit-<suffix>-shard-K` (each holding `junit.xml`); download them with
`pattern: rstest-junit-*` **without** `merge-multiple` and feed `*/junit.xml`
to your test-report integration.

[action]: https://github.com/KovantAI/rstest/tree/main/.github/actions/rstest

!!! note "Which cache backend for which layout"
    - **`actions-cache`** (the action's default, or a raw `actions/cache`
      step): a single unsharded job. In a shard matrix each shard would save
      and restore on its own, so shards can partition from different caches.
    - **`artifact`**: a shard matrix or PR suite on GitHub, as above.
    - **`remote`** (`cache-remote: s3://…`): teams already on an object store
      or a shared mount. For a gating matrix, use the
      [snapshot layout](ci-shared-cache.md#object-store-s3gcsr2-oidc-no-secrets)
      so no shard's push changes what another shard pulls.

### Full control: raw YAML

Without the action, there are two layouts:

- **Segment-merge shared cache, hand-wired.** The same artifact flow as the
  action, step by step, in
  [Shared cache: GitHub-native](ci-shared-cache.md#github-native-no-external-cloud-no-secrets).
- **Plain `actions/cache`**, below. `actions/cache` never re-saves an
  existing key and holds one blob per key, so shards can't each contribute
  their timings. Instead, the shards only **restore** one key resolved
  upstream, and a separate full run **saves** a fresh cache each time.

??? example "Shard matrix on plain `actions/cache`"

    ```yaml
    jobs:
      # Resolve the newest duration cache ONCE. Every shard restores this exact
      # key, so a new cache saved mid-matrix (or a re-run of one shard hours
      # later) can't change any shard's partition.
      resolve:
        runs-on: ubuntu-latest
        outputs:
          key: ${{ steps.lookup.outputs.cache-matched-key }}
        steps:
          - uses: actions/checkout@v7   # hashFiles needs the lockfile
          - id: lookup
            uses: actions/cache/restore@v6
            with:
              # The path list is part of the cache version: all three cache
              # steps must list the same paths, or the restores never match.
              path: |
                .rstest_cache
                !.rstest_cache/replay
              lookup-only: true         # find the key, don't download
              # The `durations` job saves `...-<run_id>`, so this exact key never
              # hits; the restore-keys prefix matches the newest saved cache.
              key: rstest-${{ runner.os }}-py3.13-${{ hashFiles('requirements.txt') }}-${{ github.run_id }}
              restore-keys: |
                rstest-${{ runner.os }}-py3.13-${{ hashFiles('requirements.txt') }}-

      test:
        needs: resolve
        runs-on: ubuntu-latest
        strategy:
          fail-fast: false
          matrix:
            shard: [1, 2, 3, 4]
        steps:
          - uses: actions/checkout@v7
          - uses: actions/setup-python@v7
            with: { python-version: "3.13" }
          - run: pip install -r requirements.txt && pip install rstest

          # Read-only restore of the ONE resolved key (no restore-keys: a
          # prefix match here could pick a different entry per shard).
          # Cold start: no key yet, every shard uses the same even split.
          - uses: actions/cache/restore@v6
            if: needs.resolve.outputs.key != ''
            with:
              path: |
                .rstest_cache
                !.rstest_cache/replay
              key: ${{ needs.resolve.outputs.key }}

          - run: |
              rstest -n 4 --shard ${{ matrix.shard }}/4 \
                     --report-json shard.${{ matrix.shard }}.json \
                     --junitxml junit.${{ matrix.shard }}.xml

          - uses: actions/upload-artifact@v7
            if: always()
            with:
              name: shard-${{ matrix.shard }}
              overwrite: true           # a re-run of this shard replaces its files
              path: |
                junit.${{ matrix.shard }}.xml
                shard.${{ matrix.shard }}.json

      # One job runs the WHOLE suite and saves the fresh cache, so the next
      # push's shards are wall-time balanced. (Each shard writes only partial
      # timings to its local copy and never saves it.)
      durations:
        runs-on: ubuntu-latest
        steps:
          - uses: actions/checkout@v7
          - uses: actions/setup-python@v7
            with: { python-version: "3.13" }
          - run: pip install -r requirements.txt && pip install rstest
          --8<-- "docs/_snippets/actions-cache-step.md"
          - run: rstest -n auto -q

      merge:
        needs: test
        runs-on: ubuntu-latest
        if: always()
        steps:
          - uses: actions/download-artifact@v8
            with:
              pattern: shard-*
              merge-multiple: true
          - uses: actions/setup-python@v7
            with: { python-version: "3.13" }
          - run: pip install rstest && rstest shard-verify shard.*.json
          # Feed junit.*.xml to your test-report integration (most accept a
          # glob), or merge: pip install junitparser &&
          # junitparser merge junit.*.xml junit.xml
    ```

    If you'd rather not run a separate full job, let shard 1 save the cache
    instead, but accept that its timings only cover 1/N of the suite.

## Other CI systems

Every CI system with a job matrix exposes the job's index and the total;
wire them into `--shard K/N` (K is 1-based) and keep an explicit `-n 2` or
more. Full recipes, including the read-only cache and the job that refreshes
it, live on the per-system pages:

| System | Index / total | Recipe |
|---|---|---|
| GitLab CI | `CI_NODE_INDEX` (1-based) / `CI_NODE_TOTAL`, with `parallel:` | [GitLab CI](ci-recipes.md#gitlab-ci) |
| CircleCI | `CIRCLE_NODE_INDEX` (**0-based**, add 1) / `CIRCLE_NODE_TOTAL`, with `parallelism:` | [CircleCI](ci-recipes.md#circleci) |
| Buildkite | `BUILDKITE_PARALLEL_JOB` (**0-based**, add 1) / `BUILDKITE_PARALLEL_JOB_COUNT`, with `parallelism:` | [Buildkite](ci-recipes.md#buildkite) |
| Azure Pipelines | `System.JobPositionInPhase` (1-based) / `System.TotalJobsInPhase`, with `strategy: parallel: N` | [Azure Pipelines](ci-recipes.md#azure-pipelines) |

## Any other CI (generic)

The only inputs are the 1-based shard number and the total. Wire them
from whatever your system exposes. With `N` total jobs, where this job is
number `K` (1..N), pin `-n` per job (not `auto`):

```console
$ rstest -n 4 --shard "$K/$N" --junitxml "junit.$K.xml"
```

Then collect all `junit.*.xml` artifacts and merge (e.g.
`junitparser merge junit.*.xml junit.xml`, or point a reporter at the
glob). Buckets are disjoint, so a simple concatenation of results is the
whole run.

## Choosing `N`

More shards cut wall time but each pays fixed startup (interpreter,
imports, session fixtures) and grabs a runner. Past the point where
startup dominates the slice, adding shards stops helping. Start with the
suite's total time divided by your target per-job time, then check the
per-shard wall times are even: if the cache is populated and restored,
they should be.
