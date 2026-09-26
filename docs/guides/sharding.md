# Sharding across CI jobs (`--shard K/N`)

`--shard K/N` splits one suite across `N` independent CI jobs. Job `K`
runs only its `1/N` slice; the jobs never talk to each other. Each job
partitions the collected tests into `N` balanced buckets and keeps
bucket `K` (K is **1-based**: `1/4`, `2/4`, `3/4`, `4/4`).

```bash
rstest -n 4 --shard 2/4 --junitxml junit.2.xml
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
  [Verify no test was dropped](#verify-no-test-was-dropped).
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

- Resolve the warm source **once** in an upstream job (for example the
  `gh run list` step) and pass the run id to every shard as a job output.
- Or, during a sharded run, **pull only** (`--cache-pull` without
  `--cache-push`) and push segments from a single follow-up job, so no
  shard's write can change what another shard reads.

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
      selects correctly, no dedicated unsharded job. Without the shared cache,
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

!!! warning "Unreleased"
    `shard-verify` and the `meta.shard` report stamp it reads are on `main`
    but **not in rstest 0.7.0**. On 0.7.0, `rstest shard-verify …` is treated
    as a test path. Until the next release, use the jq equivalent below.

Each shard's `--report-json`, written while `--shard` was
active, carries a `meta.shard` stamp: `k`, `n`, and the sha256
`collection_hash` and size of the full collected suite. Pass the per-shard
reports and it reconciles them:

```bash
# Each shard writes a report while sharding (the report carries the stamp):
rstest -n 4 --shard "$K/$N" --report-json "shard.$K.json" --junitxml "junit.$K.xml"

# After the matrix finishes, in a job that has gathered all the shard reports:
rstest shard-verify shard.*.json
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
    If you cannot run `shard-verify` (rstest 0.7.0 or older, or a policy against extra
    tooling), reconcile by hand. Collect the full suite once with the **same**
    selection flags the shards use, union the per-shard ran-ids, and compare.
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

Use a matrix. Every job restores the **same** duration-cache key, runs
its shard, and uploads a uniquely-named JUnit. A final job merges them.

```yaml
jobs:
  test:
    runs-on: ubuntu-latest
    strategy:
      fail-fast: false
      matrix:
        shard: [1, 2, 3, 4]
    steps:
      - uses: actions/checkout@v7
      - uses: actions/setup-python@v7
        with:
          python-version: "3.13"
      - run: |
          pip install -r requirements.txt
          pip install rstest

      # Restore a SHARED cache so every shard partitions identically.
      # read-only: shards must not race to save divergent caches.
      # The `durations` job below saves `...-<ref>-<run_id>`, so this exact
      # `key` never hits; the match happens via the `restore-keys` prefix,
      # pulling the newest cache for this ref. That's intended.
      - uses: actions/cache/restore@v6
        with:
          path: .rstest_cache
          key: rstest-durations-${{ github.ref_name }}
          restore-keys: |
            rstest-durations-${{ github.ref_name }}-
            rstest-durations-

      - name: test shard ${{ matrix.shard }}
        run: rstest -n 4 --shard ${{ matrix.shard }}/4 --junitxml junit.${{ matrix.shard }}.xml

      - uses: actions/upload-artifact@v7
        if: always()
        with:
          name: junit-${{ matrix.shard }}
          path: junit.${{ matrix.shard }}.xml

  # One job runs the WHOLE suite and saves the fresh cache so the next
  # push's shards are wall-time balanced. (Shards run against a restored,
  # read-only cache; something has to write the authoritative one.)
  durations:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v7
      - uses: actions/setup-python@v7
        with: { python-version: "3.13" }
      - run: pip install -r requirements.txt && pip install rstest
      - uses: actions/cache@v6
        with:
          path: .rstest_cache
          key: rstest-durations-${{ github.ref_name }}-${{ github.run_id }}
          restore-keys: rstest-durations-${{ github.ref_name }}-
      - run: rstest -n auto -q

  merge:
    needs: test
    runs-on: ubuntu-latest
    if: always()
    steps:
      - uses: actions/download-artifact@v8
        with:
          pattern: junit-*
          merge-multiple: true
      # Feed junit.*.xml to your test-report integration; most accept a
      # glob. Or merge with junitparser: pip install junitparser &&
      # junitparser merge junit.*.xml junit.xml
      - uses: actions/upload-artifact@v7
        with:
          name: junit-all
          path: junit.*.xml
```

!!! tip "Cache split, deliberately"
    Shards **restore** a shared, stable key (`rstest-durations-<ref>`) so
    they agree; a separate full run **saves** a fresh key each run so the
    numbers stay current. Pointing shards at a per-run key would give each
    matrix job a different cache and break the partition. If you'd rather
    not run a separate full job, let shard 1 save the cache instead: but
    accept that its timings only cover 1/N of the suite.

!!! tip "Or skip the dance entirely with the shared cache"
    The restore-key/refresh-job choreography above exists to work around
    `actions/cache` immutability. The [shared-cache backend](../concepts/caching.md#shared-cache-backend)
    removes it: every shard runs `--cache-pull --cache-push`, each pushing its
    own immutable segment, and they union on the next pull, so there is no
    single-writer job and no dedicated full run. One caveat: shards pull at
    different times, so a shard that pushes early can change what a later
    shard pulls in the **same** run. For a gating pipeline, pin one snapshot
    as described in [Keep one cache snapshot across the matrix](#keep-one-cache-snapshot-across-the-matrix).
    See [Shared cache across CI jobs](ci-shared-cache.md).

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

## Any other CI (generic)

The only inputs are the 1-based shard number and the total. Wire them
from whatever your system exposes:

```bash
# N total jobs; THIS job is number K (1..N). Pin -n per job (not auto).
rstest -n 4 --shard "$K/$N" --junitxml "junit.$K.xml"
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
