# Markers

## `@pytest.mark.serial`

```python
import pytest

@pytest.mark.serial
def test_binds_port_8080():
    ...
```

Excludes the test from the parallel phase. Serial tests run exclusively:
on a single designated worker, only after every other worker's session has
fully finished (fixtures torn down, ports and databases released), in
collection order.

The marker is registered by rstest automatically — no `markers` ini entry
needed, no `--strict-markers` complaints. Under plain pytest the marker is
inert (unknown markers don't change behavior), so test code stays portable.

Semantics details in [Scheduling](../concepts/scheduling.md#the-serial-phase);
when to use it in [Parallel safety](../guides/parallel-safety.md).

## `@pytest.mark.flaky`

```python
@pytest.mark.flaky(reruns=3)
def test_talks_to_flaky_service():
    ...
```

Per-test rerun budget — works with or without a global
[`--reruns`](cli.md#-reruns-n) (the mark overrides it for that test).
Reruns are coordinated by the orchestrator, so the mark only takes effect
at `-n ≥ 2` (like `--reruns`). A
pass-after-retry reports as flaky exactly like global reruns.
pytest-rerunfailures-compatible; the plugin itself is neutralized inside
rstest workers to prevent double reruns. At `-n 0/1` the orchestrated
reruns are off, so an installed pytest-rerunfailures handles the mark
natively (its normal single-process behavior). Registered automatically.

## `@pytest.mark.xdist_group`

```python
@pytest.mark.xdist_group("dbpool")
def test_uses_shared_pool():
    ...
```

Under [`--dist loadgroup`](cli.md#-dist-loadloadfileloadscopeloadgroupeach),
all tests sharing a group name run on the same worker — across files.
pytest-xdist-compatible.

## A note on `@pytest.mark.parametrize` IDs

Not a marker rstest owns, but the one that most often blocks parallelism:
parametrize **IDs must be stable across collections**. rstest collects on
each worker and refuses to dispatch if the id sets disagree, so an id built
from a memory address (`repr()` fallback), a uuid, or `now()` forces the
suite to `-n 0`. Give such a parametrize an explicit stable `ids=` (e.g.
`ids=[c.name for c in cases]`). [`rstest --migrate-check`](cli.md#-migrate-check)
finds these before your first run and names the exact site.

rstest registers the marker automatically, so `--strict-markers` never
complains about it under rstest — even when pytest-xdist is not installed.
Portability caveat: under **plain pytest**, `xdist_group` is xdist's own
marker, registered only when pytest-xdist is installed; a plain-pytest run
with `--strict-markers` and no xdist installed will reject it. Migrating off
xdist, you keep the marker either way — rstest honors it and plain pytest
treats it as inert (a no-op) as long as `--strict-markers` isn't forcing the
issue.
