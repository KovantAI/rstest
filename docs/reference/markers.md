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
rstest workers to prevent double reruns. Registered automatically.

## `@pytest.mark.xdist_group`

```python
@pytest.mark.xdist_group("dbpool")
def test_uses_shared_pool():
    ...
```

Under [`--dist loadgroup`](cli.md#-dist-loadloadfileloadscopeloadgroupeach),
all tests sharing a group name run on the same worker — across files.
pytest-xdist-compatible; registered automatically.
