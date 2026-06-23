# Lazy collection

`--collect lazy` is an opt-in collection strategy. The default
(`--collect full`) has every worker collect the whole suite — identical
sessions, outcomes verified by count and hash. Lazy mode collects each
test file **exactly once, on one worker, on demand**: the orchestrator
walks test files (the same `python_files` rules pytest uses), assigns
them to workers, and the collecting worker streams back the test ids.

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

**Suites with a few giant files.** Scheduling granularity defaults to
the file. A file with thousands of parametrized tests pins one worker
while the rest idle (aiohttp's full run is ~2x slower under lazy
affinity; packaging's 61k-in-30-files similar). Two options:

- stay with `--collect full` (the right call for full runs of such
  suites), or
- add an explicit `--dist load`, which enables **stealing**: when the
  file queue is empty, an idle worker takes half of the busiest
  worker's undispatched items, paying one extra collection of that
  file. This restores balance (packaging matches full mode) but
  reorders execution more aggressively — see below.

`--dist loadfile` (or just the lazy default) keeps strict file
affinity: a file's tests run on one worker, in file order.

## The compatibility trade

Full collection imports **every** test module in every worker before
anything runs. Some suites depend on that, usually without knowing:

- `skipif` conditions that read `sys.modules` — starlette skips
  header-encoding tests when some *other* test file has imported
  `brotli`; under lazy that import never happens on this worker, the
  test runs instead of skipping, and fails for unrelated reasons.
- Tests that only pass *because* a sibling module's import defined or
  registered something (attrs' forward-reference and version-metadata
  tests fail under plain `pytest tests/test_forward_references.py`
  too — isolation exposes them, lazy is just systematic isolation).
- Cross-file run-order pollution — rich's `test_table.py` mutates the
  `box.ASCII` singleton and never restores it; any scheduler that runs
  it before `test_box.py` (including plain pytest with the files
  reordered) sees the breakage. Lazy's duration-ordered file queue and
  (with `--dist load`) stealing produce orders the default scheduler
  doesn't.

Every divergence we found in the public-suite corpus reproduces under
plain pytest with the same isolation or ordering — lazy doesn't break
correct suites, it surfaces order/import dependence that full-suite
alphabetical runs mask. But that distinction doesn't make a red CI
green: if your suite has these patterns, use `--collect full` (the
default) or fix the tests.

## Semantics preserved

- Session-scope fixtures: one instance per worker for the whole
  session — repeated per-file collection keeps the same `Session` node.
- Module/class fixtures tear down exactly at file boundaries (the
  cross-file `nextitem` chain is maintained).
- `-k`/`-m`/marks apply per file, exactly as pytest applies them.
- `@pytest.mark.serial`, `@pytest.mark.flaky`, `--reruns`, crash
  redistribution, `-x`/`--maxfail`, `--worker-timeout` all work; reruns
  and redistribution travel by nodeid (a worker re-collects the file
  for an id it has never seen).
- Collection errors abort the run with exit 2 (pytest semantics);
  `--continue-on-collection-errors` is honored. In lazy mode an error
  can surface after some tests have already run — those outcomes stay
  reported.

## Restrictions

- `--dist loadscope` / `--dist loadgroup` are rejected: they
  consolidate groups across a global id list that lazy never builds.
- Nodeid arguments (`tests/test_x.py::test_y`) and `--pyargs` fall
  back to full collection automatically.
- Collection-time side effects of *unselected* files never happen —
  the point of the mode, and the trade documented above.
