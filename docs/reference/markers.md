# Markers

The pytest markers rstest gives special meaning to, and how each behaves at different worker counts.

## `@pytest.mark.serial`

```python
import pytest


@pytest.mark.serial
def test_binds_port_8080(): ...
```

Excludes the test from the parallel phase. Serial tests run exclusively:
on a single designated worker, only after every other worker's session has
fully finished (fixtures torn down, ports and databases released), in
collection order.

The marker is registered by rstest automatically: no `markers` ini entry
is needed, and `--strict-markers` does not complain under rstest.

Under plain pytest (no rstest) the marker is **unregistered**. It changes no
behavior, but pytest warns `PytestUnknownMarkWarning`, so a run with
`--strict-markers` (or `-W error`) fails at collection. The same applies to
`flaky` and `timeout` when pytest-rerunfailures / pytest-timeout are not
installed. To keep such a run working, add the names to your `markers` ini
entry. See [What ties you to rstest](../guides/migrate-from-pytest.md#what-ties-you-to-rstest).

Semantics details in [Scheduling](../concepts/scheduling.md#the-serial-phase);
when to use it in [Parallel safety](../guides/parallel-safety.md).

## `@pytest.mark.flaky`

```python
@pytest.mark.flaky(reruns=3)
def test_talks_to_flaky_service(): ...
```

Per-test rerun budget: the mark overrides a global
[`--reruns`](cli.md#-reruns-n) for that test. A pass-after-retry reports as
flaky exactly like global reruns. The plugin itself is neutralized inside
rstest pool workers to prevent double reruns (at `-n 0/1` without a global
`--reruns` there is no pool, so it stays active; see below). Registered automatically.

The marker name matches pytest-rerunfailures, and rstest reads the budget
the same way: the `reruns=` keyword, else the first positional argument
(`flaky(3)`), else 1. `condition=` is honored too: a false condition (a bool,
or a string evaluated like the plugin does) means no reruns. A string
condition that fails to evaluate keeps the reruns. The plugin's other options
(`reruns_delay`, `only_rerun`, `rerun_except`) are ignored by rstest's own
retry; use the global [`--only-rerun`](cli.md#-only-rerun-regex) to filter by
error.

The mark works **with or without** a global `--reruns`, at any worker count:

- **At `-n ≥ 2`** (and in the one-worker rerun pool that `--reruns` starts at
  `-n 0/1`, see [`--reruns`](cli.md#-reruns-n)) the orchestrator retries the
  test, possibly on another worker.
- **At `-n 0/1` without `--reruns`** the run stays the byte-exact single
  session, and the session retries a marked test in place, the way
  pytest-rerunfailures does: a failed attempt shows as `R` (`RERUN` with
  `-v`) and counts as `N rerun` in pytest's summary line, and a test that
  then passes is reported flaky. An installed pytest-rerunfailures handles
  the mark itself there instead, as under plain pytest. Tests without the
  mark run exactly as before.

A failed attempt that a rerun may still rescue never counts toward `-x` /
`--maxfail`; only a test that fails its last attempt does.

## `@pytest.mark.xdist_group`

```python
@pytest.mark.xdist_group("dbpool")
def test_uses_shared_pool(): ...
```

Under [`--dist loadgroup`](cli.md#-dist-loadloadfileloadscopeloadgroupeach),
all tests sharing a group name run on the same worker, across files.
pytest-xdist-compatible.

rstest registers the marker automatically, so `--strict-markers` never
complains about it under rstest, even when pytest-xdist is not installed.
Portability caveat: under **plain pytest**, `xdist_group` is xdist's own
marker, registered only when pytest-xdist is installed; a plain-pytest run
with `--strict-markers` and no xdist installed will reject it. Migrating off
xdist, you keep the marker either way: rstest honors it, and plain pytest
treats it as inert (a no-op) as long as `--strict-markers` isn't forcing the
issue.

## `@pytest.mark.timeout`

```python
@pytest.mark.timeout(5)
def test_slow_path(): ...
```

Per-test deadline in seconds, overriding the global
[`--timeout`](cli.md#-timeout-secs). The test is interrupted in-process at the
deadline and fails with a traceback at the stuck line. pytest-timeout-compatible
marker name; no plugin needed.

`timeout(0)` or a negative value disables the timeout for that test, even
when a global `--timeout` is set.

The marker arms rstest's own timer at every worker count, including `-n 0`,
even when you pass no `--timeout` and even with pytest-timeout installed
(`-p no:timeout` does not turn it off). If pytest-timeout is also installed,
both honor the marker, so uninstall it rather than run two timers.

## A note on `@pytest.mark.parametrize` IDs

Not a marker rstest owns, but the one that most often blocks parallelism:
parametrize **IDs must be stable across collections**. Under full
collection (every cold-cache run, and suites below the
[lazy](../concepts/lazy-collection.md) auto threshold) rstest collects on
each worker and refuses to dispatch if the id sets disagree, so an id built
from a memory address (`repr()` fallback), a uuid, or a sub-second timestamp
stops a parallel run (`workers collected different test sets`) until you fix
it or run `-n 0`. A second-resolution `now()` id usually matches across
workers and runs, but can fail intermittently when collection straddles a
second. Give such a parametrize an explicit stable `ids=` (e.g.
`ids=[c.name for c in cases]`). [`rstest migrate-check`](cli-commands.md#migrate-check)
finds these before your first run and names the exact site.
