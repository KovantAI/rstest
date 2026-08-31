# Caching

## `.rstest_cache/` (rstest's own)

- `durations.json` — per-test call durations, merged over runs (a filtered
  run updates only the tests it ran). Drives
  [long-pole-first scheduling](scheduling.md#dispatch-order) and the
  suite-size heuristic behind `-n auto`. Safe to delete at any time; the
  next run rebuilds it (and is scheduled in collection order).
- `flakes.json` — sparse record of tests that have passed only on rerun
  ([`--reruns`](../guides/flaky-tests.md) / `@pytest.mark.flaky`), used to
  surface repeat offenders. Auto-written, safe to delete, persisted the same
  way as `durations.json`.
- `coverage_index.json` — line→test index (which tests' coverage executed
  each source line), written by any [`--cov-context=test`](../guides/coverage.md#per-test-contexts-cov-contexttest)
  run. Lets [`--changed`](../guides/changed.md) select only the tests hitting
  the changed lines. Safe to delete — `--changed` falls back to the import
  graph without it; rebuild by re-running coverage with `--cov-context=test`.
  Merges through the shared cache like the others, so sharded coverage runs
  union into a full index (see the shared-cache section below).

Persist it in CI ([example](../guides/ci-quickstart.md)) to get
duration-aware scheduling from the second run onward. In the repository,
add it to `.gitignore` alongside `.pytest_cache/`:

```gitignore
.pytest_cache/
.rstest_cache/
```

The location is CWD-relative by default; set `RSTEST_CACHE` to relocate it
(distinct from `RSTEST_CACHE_DIR`, which steers the machine-global
interpreter-probe cache). Writes are atomic (tmp + rename), so a concurrent
reader never sees a half-written file.

## Shared cache backend

Instead of hand-wiring `actions/cache` (with its per-key immutability dance and
a dedicated refresh job), rstest can publish and warm `.rstest_cache` to a
**shared remote** directly — see [`--cache-remote`](../reference/cli.md#-cache-remote-urldir--cache-pull--cache-push).

It is **segmented, merge-on-read**: each run pushes its own immutable segment
rather than overwriting one shared blob, so concurrent shards and PRs never
clobber each other.

```
<remote>/
  base.json                       # compacted merged state
  segments/seg-<id>.json          # one immutable segment per run/shard
```

- **Pull** merges `base.json` + every segment into the local cache
  (durations = newest value per test; flake counts = summed per-run events,
  deduped by segment id so a re-pull never double-counts; the coverage index
  unions per file — segments that agree on a file's content hash merge their
  line→test maps, a changed hash keeps the newest, so **sharded partial
  indexes fuse into a full one**).
- **Push** writes just this run's segment (`--cache-push`).
- **Compact** (`--cache-compact`) folds segments into a new base and prunes
  them; a segment already folded is recorded in the base's absorbed-id set, so
  compaction is safe against concurrent pushes.

The remote is a plain directory — a local path, an NFS/EFS mount, or a dir a CI
step materializes (GitHub `download-artifact`, `aws s3 sync`). Recipes:
[CI quickstart → Shared cache](../guides/ci-quickstart.md#shared-cache).

## `.pytest_cache/` (pytest's, shared)

Workers read it normally (`--lf`/`--ff` deselection happens inside the
vendored core). Writes to the run-level keys (`lastfailed`, `nodeids`,
`stepwise`) are blocked in workers — each worker sees only its own slice —
and the orchestrator writes the merged truth after the run. Other plugins'
cache writes pass through untouched.

## Worker temp directories

Each worker gets a disjoint `tmp_path` root under `$TMPDIR/rstest-<pid>/gwN/`
(one subdirectory per worker id — the same per-worker isolation xdist gets
from its `popen-gwN` roots), preventing numbered-directory cleanup races
between sibling workers. A user-provided `--basetemp` is honored and left
alone.
