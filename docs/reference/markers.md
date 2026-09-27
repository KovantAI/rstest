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

The marker is registered by rstest automatically, no `markers` ini entry
needed, no `--strict-markers` complaints. Under plain pytest the marker is
inert (unknown markers don't change behavior), so test code stays portable.

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
rstest workers to prevent double reruns. Registered automatically.

The marker name matches pytest-rerunfailures, but rstest reads **only the
`reruns=` keyword**, defaulting to 1. The positional form (`flaky(3)`) and
the plugin's other options (`reruns_delay`, `condition`, `only_rerun`) are
ignored by rstest's own retry, so `@pytest.mark.flaky(3)` retries once. Write
`@pytest.mark.flaky(reruns=3)`, and use the global
[`--only-rerun`](cli.md#-only-rerun-regex) to filter by error.

Reruns are coordinated by the orchestrator:

- **At `-n ≥ 2`** the mark takes effect **with or without** a global
  `--reruns`.
- **At `-n 0/1`** the orchestrated retry runs only when a global `--reruns`
  is set: that flag promotes the single-worker run to a one-worker rerun
  pool (see [`--reruns`](cli.md#-reruns-n)). A flaky mark **on its own**, with
  no global `--reruns`, does **not** trigger the pool at `-n 0/1`, so the
  orchestrated retry is off; an installed pytest-rerunfailures then handles
  the mark natively (its normal single-process behavior). Pass a global
  `--reruns` to get rstest's own retry for marked tests in single-worker runs.

## `@pytest.mark.xdist_group`

```python
@pytest.mark.xdist_group("dbpool")
def test_uses_shared_pool(): ...
```

Under [`--dist loadgroup`](cli.md#-dist-loadloadfileloadscopeloadgroupeach),
all tests sharing a group name run on the same worker: across files.
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

The marker arms rstest's own timer at every worker count, including `-n 0`,
even when you pass no `--timeout` and even with pytest-timeout installed
(`-p no:timeout` does not turn it off). If pytest-timeout is also installed,
both honor the marker, so uninstall it rather than run two timers.

## A note on `@pytest.mark.parametrize` IDs

Not a marker rstest owns, but the one that most often blocks parallelism:
parametrize **IDs must be stable across collections**. rstest collects on
each worker and refuses to dispatch if the id sets disagree, so an id built
from a memory address (`repr()` fallback), a uuid, or `now()` forces the
suite to `-n 0`. Give such a parametrize an explicit stable `ids=` (e.g.
`ids=[c.name for c in cases]`). [`rstest migrate-check`](cli-commands.md#migrate-check)
finds these before your first run and names the exact site.
