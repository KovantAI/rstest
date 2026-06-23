# Flag & config map — pytest / pytest-xdist → rstest

rstest forwards every unrecognized flag straight to the pytest session, so the
**entire pytest flag surface works unchanged** (`-k`, `-m`, `-x`, `-q`, `-v`,
`--lf`, plugin flags, …). Only the runner-level concerns below differ.

## Invocation

| you ran | run instead |
|---|---|
| `pytest` | `rstest` (parallel, `-n auto` by default) |
| `pytest -n 0` (sanity) | `rstest -n 0` (byte-exact single-worker) |
| `pytest -p no:cacheprovider …` | `rstest -p no:cacheprovider …` (forwarded) |

## pytest-xdist → rstest

If the suite already uses xdist, the mental model carries over directly.

| pytest-xdist | rstest | notes |
|---|---|---|
| `-n auto` / `-n 4` | `-n auto` / `-n 4` | same meaning |
| `-n 0` | `-n 0` | rstest's `-n 0` is byte-exact serial; identical to `-n 1` |
| `--dist load` | `--dist load` | default; test-granular, duration-aware |
| `--dist loadfile` | `--dist loadfile` | file affinity |
| `--dist loadscope` | `--dist loadscope` | class/module affinity |
| `--dist loadgroup` + `@pytest.mark.xdist_group` | same | marker honored |
| `workerinput` / `PYTEST_XDIST_WORKER*` | provided | plugins that read worker identity work |
| `pytest_configure_node` master hook | emulated per-worker | uuid/workerid-derived values work (e.g. SQLAlchemy `follower_ident`, pytest-django DB suffix); a single shared-server allocator is the one gap |

rstest neutralizes xdist's own session (so the two don't both try to
parallelize) but keeps `numprocesses` visible, so xdist-aware plugins still set
up correctly. Drop `pytest-xdist` from the test command; you can leave it
installed.

## Plugins that need no action

- **pytest-randomly** — rstest syncs the random seed across workers, so every
  worker collects the same shuffled order. No `-p no:randomly` needed.
- **pytest-cov** — coverage is collected per worker and merged by the
  orchestrator; works as-is.
- **pytest-reverse / pytest-ordering** — deterministic reorderers, no impact.

## `[tool.rstest]` config (pyproject.toml)

Set defaults so contributors get the right behavior without remembering flags:

```toml
[tool.rstest]
numprocesses = "auto"   # or an int; "0" forces serial
dist = "load"           # load | loadfile | loadscope | loadgroup | each
collect = "full"        # "lazy" helps narrow -k/-m on huge suites
output = "bar"           # dots | verbose | bar | github | json
```

Use a non-default only when the suite needs it (e.g. `dist = "loadfile"` for an
order-dependent suite, `numprocesses = 4` for a load-sensitive one).

## Monorepo

If there's no root pytest config but multiple sub-projects each have their own,
rstest discovers them and runs them as one parallel session from the root. Pin
the measured set if needed:

```toml
[tool.rstest]
projects = ["libs/a", "libs/b", "libs/c"]
```

## CI gate

Replace the pytest step with rstest, and add a preflight gate that fails the
build on **new** parallel-unsafe tests while tolerating ones the team has
accepted:

```yaml
- run: rstest                                   # the test run, -n auto
- run: rstest --migrate-check-json mc.json \
        --migrate-allow tests/legacy/           # gate: non-zero on new issues
```

`--migrate-allow <substring>` (repeatable) accepts a known finding by
nodeid/site substring — it's still reported (marked `(allowed)`) but doesn't
fail the gate. Use `--output github` on the test run for inline PR annotations.
