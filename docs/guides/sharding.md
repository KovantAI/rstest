# Sharding across CI jobs (`--shard K/N`)

`--shard K/N` splits one suite across `N` independent CI jobs. Job `K`
runs only its `1/N` slice; the jobs never talk to each other. Each job
partitions the collected tests into `N` balanced buckets and keeps
bucket `K` (K is **1-based**: `1/4`, `2/4`, `3/4`, `4/4`).

```bash
rstest -n auto --shard 2/4 --junitxml junit.2.xml
```

This is the fan-out for the common case: **one large suite is the long
pole**, and you want it spread across a runner matrix. It is orthogonal
to `-n` — each shard still runs its slice across local workers — and to
[monorepo mode](monorepo.md), which splits *across projects on one box*.
Sharding splits *one suite across many boxes*.

## How the split stays balanced

Buckets are balanced by the **duration cache**
(`.rstest_cache/durations.json`) using longest-processing-time-first
bin-packing: the slowest tests are placed first, each into the currently
lightest bucket. So a suite with a few dominating tests still splits into
even *wall-time* slices, not even *counts*.

The split is deterministic: given the same test list and the same
duration cache, every job computes the identical partition — which is
what lets `N` jobs agree on who runs what with zero coordination.

Two consequences for CI:

- **Restore the same duration cache on every shard.** If job 2 sees a
  different cache than job 3, their partitions can overlap or drop tests.
  Restore one shared cache key across the matrix (recipes below).
- **A cold cache falls back to an even count split** (round-robin). The
  first run is balanced by count; from the second run on — once the cache
  is populated and restored — it balances by wall time.

Buckets are always **disjoint and cover the whole suite**, so merging the
per-shard JUnit reconstructs the full run.

!!! note "Requirements & limits"
    - Needs the parallel pool: `-n ≥ 2` (or `-n auto`). Single-worker /
      `-n 0` runs the session's own full suite with no dispatch filter.
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
    - **Building the coverage index under `--shard` is partial.** A sharded
      `--cov-context=test` run only measures its own shard's tests, so the
      `coverage_index.json` it writes covers a fraction of the suite — a later
      `--changed` would under-select. Warm the [`--changed` coverage
      index](changed.md#keeping-the-index-warm) from an **unsharded** coverage
      run (or merge each shard's `.coverage` before building the index), not
      from the sharded job.

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
      - uses: actions/checkout@v4
      - uses: actions/setup-python@v5
        with:
          python-version: "3.13"
      - run: |
          pip install -r requirements.txt
          pip install rstest

      # Restore a SHARED cache so every shard partitions identically.
      # read-only: shards must not race to save divergent caches.
      # The `durations` job below saves `...-<ref>-<run_id>`, so this exact
      # `key` never hits — the match happens via the `restore-keys` prefix,
      # pulling the newest cache for this ref. That's intended.
      - uses: actions/cache/restore@v4
        with:
          path: .rstest_cache
          key: rstest-durations-${{ github.ref_name }}
          restore-keys: |
            rstest-durations-${{ github.ref_name }}-
            rstest-durations-

      - name: test shard ${{ matrix.shard }}
        run: rstest -n auto --shard ${{ matrix.shard }}/4 --junitxml junit.${{ matrix.shard }}.xml

      - uses: actions/upload-artifact@v4
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
      - uses: actions/checkout@v4
      - uses: actions/setup-python@v5
        with: { python-version: "3.13" }
      - run: pip install -r requirements.txt && pip install rstest
      - uses: actions/cache@v4
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
      - uses: actions/download-artifact@v4
        with:
          pattern: junit-*
          merge-multiple: true
      # Feed junit.*.xml to your test-report integration; most accept a
      # glob. Or merge with junitparser: pip install junitparser &&
      # junitparser merge junit.*.xml junit.xml
      - uses: actions/upload-artifact@v4
        with:
          name: junit-all
          path: junit.*.xml
```

!!! tip "Cache split, deliberately"
    Shards **restore** a shared, stable key (`rstest-durations-<ref>`) so
    they agree; a separate full run **saves** a fresh key each run so the
    numbers stay current. Pointing shards at a per-run key would give each
    matrix job a different cache and break the partition. If you'd rather
    not run a separate full job, let shard 1 save the cache instead — but
    accept that its timings only cover 1/N of the suite.

!!! tip "Or skip the dance entirely with the shared cache"
    The restore-key/refresh-job choreography above exists to work around
    `actions/cache` immutability. The [shared-cache backend](../concepts/caching.md#shared-cache-backend)
    removes it: every shard runs `--cache-pull --cache-push`, each pushing its
    own immutable segment, and they union on the next pull — no single-writer
    job, no dedicated full run. See
    [CI quickstart → Shared cache](ci-quickstart.md#shared-cache).

## GitLab CI

GitLab exposes `CI_NODE_INDEX` (1-based) and `CI_NODE_TOTAL` when you set
`parallel:`. They map straight onto `K/N`:

```yaml
test:
  parallel: 4
  cache:
    key: rstest-durations-$CI_COMMIT_REF_SLUG
    paths: [.rstest_cache]
    policy: pull        # shards restore only; don't race to save
  script:
    - pip install -r requirements.txt && pip install rstest
    - rstest -n auto --shard ${CI_NODE_INDEX}/${CI_NODE_TOTAL} --junitxml junit.xml
  artifacts:
    when: always
    reports:
      junit: junit.xml   # GitLab merges per-job JUnit natively
```

Add a separate non-parallel job with `policy: pull-push` that runs the
full suite to keep the cache fresh, mirroring the GitHub `durations` job.

## CircleCI

CircleCI provides `CIRCLE_NODE_INDEX` (**0-based**) and
`CIRCLE_NODE_TOTAL`. Add 1 to the index:

```yaml
jobs:
  test:
    parallelism: 4
    steps:
      - checkout
      - restore_cache: { keys: ["rstest-durations-{{ .Branch }}"] }
      - run: pip install -r requirements.txt && pip install rstest
      - run: rstest -n auto --shard $((CIRCLE_NODE_INDEX + 1))/$CIRCLE_NODE_TOTAL --junitxml test-results/junit.xml
      - store_test_results: { path: test-results }   # a directory, not a file
```

As with GitHub and GitLab, the shards restore that cache read-only —
**something must write it**, or every run partitions cold (even split, no
wall-time balancing). Add a separate non-parallel job that runs the full
suite and saves the fresh cache:

```yaml
  durations:
    steps:
      - checkout
      - restore_cache: { keys: ["rstest-durations-{{ .Branch }}"] }
      - run: pip install -r requirements.txt && pip install rstest
      - run: rstest -n auto -q
      - save_cache:
          key: rstest-durations-{{ .Branch }}-{{ .Revision }}
          paths: [".rstest_cache"]
```

Wire both jobs into a workflow — CircleCI runs nothing without a
`workflows:` block:

```yaml
workflows:
  test-and-cache:
    jobs:
      - test
      - durations
```

CircleCI keys are immutable once written, so the `{{ .Revision }}` suffix
makes each run save a fresh key that the shards' branch-prefix
`restore_cache` then picks up on the next push. CircleCI aggregates
per-container results in its Tests tab; for a single merged `junit.xml`
artifact, add a downstream collect/merge step as in the GitHub recipe.

## Any other CI (generic)

The only inputs are the 1-based shard number and the total. Wire them
from whatever your system exposes:

```bash
# N total jobs; THIS job is number K (1..N). -n auto per job.
rstest -n auto --shard "$K/$N" --junitxml "junit.$K.xml"
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
per-shard wall times are even — if the cache is populated and restored,
they should be.
