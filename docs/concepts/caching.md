# Caching

## `.rstest_cache/` (rstest's own)

- `durations.json` — per-test call durations, merged over runs (a filtered
  run updates only the tests it ran). Drives
  [long-pole-first scheduling](scheduling.md#dispatch-order) and the
  suite-size heuristic behind `-n auto`. Safe to delete at any time; the
  next run rebuilds it (and is scheduled in collection order).

Persist it in CI ([example](../guides/ci-quickstart.md)) to get
duration-aware scheduling from the second run onward. In the repository,
add it to `.gitignore` alongside `.pytest_cache/`:

```gitignore
.pytest_cache/
.rstest_cache/
```

## `.pytest_cache/` (pytest's, shared)

Workers read it normally (`--lf`/`--ff` deselection happens inside the
vendored core). Writes to the run-level keys (`lastfailed`, `nodeids`,
`stepwise`) are blocked in workers — each worker sees only its own slice —
and the orchestrator writes the merged truth after the run. Other plugins'
cache writes pass through untouched.

## Worker temp directories

Each worker gets a disjoint `tmp_path` root under
`$TMPDIR/rstest-<pid>/gwN/` (pytest-xdist's `popen-gwN` pattern), preventing
numbered-directory cleanup races between sibling workers. A user-provided
`--basetemp` is honored and left alone.
