# Onboarding to pytest 9.1.1

rstest runs a **vendored pytest 9.1.1** core, so adopting rstest adopts
pytest 9's behavior — whatever pytest version is installed elsewhere in
your environment. This page is the small, concrete checklist for getting a
suite that runs on **pytest 8.x** or **pytest 9.0.x** clean on 9.1.1
*before* you switch the runner, so the switch itself stays a one-line
change.

It is deliberately short. pytest 9 is a **cleanup major**, not a redesign:
it removes APIs that already emitted `DeprecationWarning` throughout 8.x and
keeps the collection model, fixture engine, `_pytest.*` import paths, and
the `pluggy` hook contract. If your suite is warning-clean today, you are
almost certainly already done — jump to [Verify](#3-verify).

## The method

You don't need to guess which *removed APIs* apply. Turn deprecations into
errors on **your current pytest**, and the suite names each one — with your
installed pytest untouched. A short list of warning-free **behavioral**
changes (below) you check by hand; then `rstest -n 0` is the backstop that
runs the suite under the real 9.1.1 core.

### 1. Surface the warnings

Run your existing suite with deprecation warnings promoted to errors:

```console
$ pytest -W error::DeprecationWarning -W error::PendingDeprecationWarning
```

Every failure is one thing to fix. A clean run here means **nothing below
applies to you** — go to step 3.

!!! tip "No config change needed"
    `-W` is a command-line flag; it overrides your `filterwarnings` ini for
    this one run. Don't commit it yet — it's a probe, not the fix.

### 2. Fix what fired

Match each error to the table. These are the pytest-8→9 removals that
actually bite real suites (grounded in the upstream
[deprecations list](https://docs.pytest.org/en/stable/deprecations.html)):

| Removed in | What broke | Fix |
|---|---|---|
| **9.0** | A **sync test depends on an async fixture** | Make the test `async`, or wrap the async fixture in a sync one. A sync test can no longer pull an un-awaited coroutine from an async fixture. |
| **9.0** | A **mark applied to a fixture function** (`@pytest.mark.* ` above `@pytest.fixture`) | Move the mark to the *test* functions that use the fixture. Marks on fixture defs were silently ignored and are now an error. |
| **9.0** | A **hook takes `py.path.local`** args | Rename the parameter to its `pathlib.Path` twin (below). |
| **9.1** | `importorskip("x")` **swallowed a real `ImportError`** | It now only skips on `ModuleNotFoundError`. If you relied on catching a deeper `ImportError`, pass `exc_type=ImportError` explicitly. |
| **9.1** | `SomeCollector.from_parent(..., fspath=...)` | Pass `path=<pathlib.Path>` instead of `fspath=<py.path.local>`. |

Hook-argument renames (9.0) — old name → new name, same value as a
`pathlib.Path`:

| Hook | Old arg | New arg |
|---|---|---|
| `pytest_ignore_collect` | `path` | `collection_path` |
| `pytest_collect_file` | `path` | `file_path` |
| `pytest_pycollect_makemodule` | `path` | `module_path` |
| `pytest_report_header` | `startdir` | `start_path` |
| `pytest_report_collectionfinish` | `startdir` | `start_path` |

These live in `conftest.py` and plugins, not test files. Grep to find them
fast:

```console
$ grep -rn "def pytest_\(ignore_collect\|collect_file\|pycollect_makemodule\|report_header\|report_collectionfinish\)" .
```

!!! note "Most suites hit zero of these"
    The common case is **no matches**. These removals target plugin authors
    and old conftest hooks, not everyday test code. If your grep is empty
    and step 1 was clean, you have nothing to do.

#### Behavioral changes the `-W` probe won't catch

Step 1 promotes *deprecation warnings* to errors, so it finds every removed
API. But pytest 9.0 also made a few **behavioral** changes that emit **no
warning** — the probe stays green and they bite at runtime instead. Check
these by hand:

| 9.0 change | Who it bites | Fix / restore |
|---|---|---|
| **Duplicate path args are de-duplicated.** `pytest x.py x.py` (or `pytest a/b a/`) now runs the overlap **once**, not twice. | Scripts/CI that pass repeated or nested paths and count on re-runs. | Pass `--keep-duplicates` to restore the old behavior, or stop passing the duplicates. |
| **CI detection requires a non-empty value.** `$CI` / `$BUILD_NUMBER` must now be set to something non-empty; an empty string no longer triggers CI mode. | Pipelines that export `CI=` empty and rely on CI-mode output. | Set `CI=1` (or any non-empty value) in the job. |
| **`config.args` holds strings only** (no longer `pathlib.Path`). | conftest/plugins that read `config.args` and expect path objects. | Wrap in `pathlib.Path(...)` at the read site. |
| **Python 3.9 support dropped.** | Suites still running on 3.9. | The vendored core needs CPython **3.10+** — the floor rstest already requires. Upgrade the interpreter. |
| **`PytestRemovedIn9Warning` is now an error by default.** | Anything using an API slated for removal in 9.1. | Fix it (that's the point), or stopgap `filterwarnings = ignore::pytest.PytestRemovedIn9Warning` in your ini while you work through them. |

### 3. Verify

Point rstest at the suite in single-session mode — one worker, one pytest
session, **byte-exact** pytest 9.1.1 behavior:

```console
$ rstest -n 0
```

Green here means your suite is pytest-9.1.1-clean. Now drop `-n 0` to go
parallel — that's a *different* migration
([parallel safety](parallel-safety.md)), not a pytest-version one.

## Already on pytest 9.0.x?

Then you're nearly done — the 9.0 → 9.1 delta is only **two** items, both in
the table above:

1. `importorskip` catches only `ModuleNotFoundError` by default (pass
   `exc_type=ImportError` to restore the old catch-all).
2. `from_parent(..., fspath=...)` → `path=`.

Run step 1's `-W error` probe once to confirm, and you're on 9.1.1.

## Done criteria

- [ ] `pytest -W error::DeprecationWarning` runs clean on your current pytest
- [ ] `grep` for the renamed hooks is empty (or all renamed)
- [ ] the warning-free behavioral changes above checked (dup args, CI var, `config.args`, Python ≥ 3.10)
- [ ] `rstest -n 0` is green

Three greens and the pytest-version step is finished. What's left — running
in parallel — is covered by [Migrating from pytest](migrate-from-pytest.md)
and [Parallel safety](parallel-safety.md).

## Why this is a separate step

See [Compatibility → Why 9, not 8](../concepts/compatibility.md#why-9-not-8)
for the reasoning: vendoring 9 keeps rstest current with upstream at
near-zero adoption cost, because the removed APIs are exactly the ones 8.x
was already warning you about. There is one vendored core, tracked forward;
no older-core build.
