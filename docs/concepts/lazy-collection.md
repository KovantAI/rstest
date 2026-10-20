# Lazy collection

Collection has two strategies. `--collect full` has every worker collect
the whole suite: identical sessions, outcomes verified by count and
hash. Lazy mode collects each test file **exactly once, on one worker, on
demand**: the orchestrator walks test files (the same `python_files`
rules pytest uses), assigns them to workers, and the collecting worker
streams back the nodeids.

## Auto-default

Setting neither `--collect` nor `[tool.rstest] collect` selects the
strategy automatically. rstest picks `lazy` when all of these hold, and
`full` otherwise:

- **Workers:** 2 or more (auto never picks `lazy` at `-n 0` or `-n 1`).
- **Size:** at least **2000** known tests and a **`tests × workers` ≥
  16 000** product. Lazy's win is dropping the `(workers − 1)` redundant
  full collections, which only pays off once the suite and the worker count
  are both large; smaller suites keep full collection's locality.
- **Dist:** `--dist load` (the default, implicit or explicit) or
  `--dist loadfile`.
- **Selection:** no nodeid, no `--pyargs`, and no path selection (explicit
  paths, or `--changed`/`--since-green` narrowing). The test count covers
  the whole cached suite, so a run narrowed to a few files keeps full
  collection's per-test spread across workers.
- **Features:** no `--shard`, `--shuffle`, `--incremental` or fail-fast
  `--order` (including the one `--watch` picks by default).
- **Doctests:** none enabled (argv, ini `addopts` or `PYTEST_ADDOPTS`). Lazy
  hands workers only `python_files` matches, so it would miss doctests in
  non-test modules, `--doctest-glob` files and plugin-collected non-`.py`
  items.
- **File walk:** every cached test's file is found by the lazy walk; a
  missing one means lazy would lose tests.
- **Balance:** no file's cached time exceeds an even per-worker share
  (`total time / workers`) by more than a second. Lazy runs each file whole
  on one worker, so such a file would hold up the run. The check assumes no
  stealing, so it applies even under an explicit `--dist load` (which could
  split the file; see [When it doesn't](#when-it-doesnt)).

The test count comes from `.rstest_cache/durations.json`, so a **cold cache
counts as zero tests** and a suite's first run stays `full`; a warm run of a
large suite flips to `lazy`. When auto picks `lazy` it prints a banner
naming the test and worker counts. Auto never *rejects* a config: when a
condition fails it just stays `full`. Force either strategy with an explicit
`--collect full` / `--collect lazy`; the latter also uses lazy collection
for a suite the balance check turned down.

Files the walk finds but pytest would never recurse into (`norecursedirs`,
`collect_ignore`, `--ignore`) are safe in any lazy run: the worker applies
pytest's own ignore checks and reports them empty.

```console
$ rstest --collect lazy
$ rstest --collect lazy -k "test_keepalive"
```

Or per project:

```toml
[tool.rstest]
collect = "lazy"
```

## When it wins

**Narrow selections on big suites.** A `-k`/`-m` run in full mode still
collects everything in every worker before deselecting; that's the
entire cost of the run when only a few tests match. Lazy mode pays one
distributed collection pass instead of N identical ones:

| run | full | lazy |
|---|---|---|
| aiohttp `-k test_keepalive` (15 of 4,469 tests) | 2.1s | 0.7s |

The same shape applies to focused iteration loops on large suites:
collection work scales with what you select, not with worker count.

## When it doesn't

**Suites with a few giant files.** Under lazy collection the unit of
dispatch is the file, not the test, even with the default `--dist load`.
A file with thousands of parametrized tests pins one worker while the rest
idle (aiohttp's full run is ~2× slower under lazy
affinity; packaging's 61k-in-30-files similar). Two options:

- stay with `--collect full` (the right call for full runs of such
  suites), or
- set `--dist load` explicitly (on the command line or as
  `[tool.rstest] dist = "load"`), which enables **stealing** however lazy
  was chosen (the implicit default does not): when the
  file queue is empty, an idle worker takes half of the busiest
  worker's undispatched items, paying one extra collection of that
  file. This restores balance (packaging matches full mode) but
  reorders execution more aggressively (see below).

Without an explicit `load`, lazy keeps strict file affinity (with the
default dist or `--dist loadfile` alike): a file's tests run on one worker,
in file order. A `load` from either source turns stealing on, so a
command-line `--dist loadfile` does not switch off a `dist = "load"` in
`[tool.rstest]`; remove it from the config for strict affinity.

## The compatibility trade

Full collection imports **every** test module in every worker before
anything runs. Some suites depend on that, usually without knowing:

- `skipif` conditions that read `sys.modules`: starlette skips
  header-encoding tests when some *other* test file has imported
  `brotli`; under lazy that import never happens on this worker, so the
  test runs instead of skipping, and fails for unrelated reasons.
- Tests that only pass *because* a sibling module's import defined or
  registered something (attrs' forward-reference and version-metadata
  tests fail under plain `pytest tests/test_forward_references.py`
  too; isolation exposes them, and lazy is just systematic isolation).
- Cross-file run-order pollution: rich's `test_table.py` mutates the
  `box.ASCII` singleton and never restores it; any scheduler that runs
  it before `test_box.py` (including plain pytest with the files
  reordered) sees the breakage. Lazy's duration-ordered file queue and
  (with `--dist load`) stealing produce orders the default scheduler
  doesn't.

Every divergence we found in the public-suite corpus reproduces under
plain pytest with the same isolation or ordering: lazy doesn't break
correct suites; it surfaces order/import dependence that full-suite
alphabetical runs mask. But that distinction doesn't make a red CI
green: if your suite has these patterns, use `--collect full` (which
also overrides the auto default) or fix the tests.

## Semantics preserved

- Session-scope fixtures: one instance per worker for the whole
  session; repeated per-file collection keeps the same `Session` node.
- Module/class fixtures tear down exactly at file boundaries (the
  cross-file `nextitem` chain is maintained).
- `-k`/`-m`/marks apply per file, exactly as pytest applies them.
- `@pytest.mark.serial`, `@pytest.mark.flaky`, `--reruns`, crash
  redistribution (but not crashed-worker `pytest_testnodedown` cleanup;
  see [Crash cleanup](xdist-hooks.md#crash-cleanup)), `-x`/`--maxfail`,
  `--worker-timeout` all work; reruns and redistribution travel by nodeid
  (a worker re-collects the file for a nodeid it has never seen).
- Collection errors abort the run with exit 2 (pytest semantics);
  `--continue-on-collection-errors` is honored. In lazy mode an error
  can surface after some tests have already run: those outcomes stay
  reported.

## Restrictions

- `--dist loadscope` / `--dist loadgroup` are rejected: they
  consolidate groups across a global nodeid list that lazy never builds.
- `--dist each` is rejected too: it runs the full suite on every worker,
  so there is nothing to collect lazily.
- Nodeid arguments (`tests/test_x.py::test_y`) and `--pyargs` fall
  back to full collection automatically.
- Collection-time side effects of *unselected* files never happen:
  the point of the mode, and the trade documented above.
