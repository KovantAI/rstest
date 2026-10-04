# Flag & config map: pytest / pytest-xdist → rstest

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
| `--dist load` | `--dist load` | default; test-granular, duration-aware (per file when auto picks lazy collection on a large warm suite) |
| `--dist loadfile` | `--dist loadfile` | file affinity |
| `--dist loadscope` | `--dist loadscope` | class/module affinity |
| `--dist loadgroup` + `@pytest.mark.xdist_group` | same | marker honored |
| `workerinput` / `PYTEST_XDIST_WORKER*` | provided | plugins that read worker identity work |
| `pytest_configure_node` master hook | emulated per-worker | uuid/workerid-derived values work (e.g. SQLAlchemy `follower_ident`, pytest-django DB suffix); hooks that allocate from one controller-side counter or registry need rework (derive from `gateway.id` or a uuid) |

rstest neutralizes xdist's own session (so the two don't both try to
parallelize) but keeps `numprocesses` visible, so xdist-aware plugins still set
up correctly. Drop `pytest-xdist` from the test command; you can leave it
installed.

## Plugins that need no action

- **pytest-randomly**: rstest syncs the random seed across workers, so every
  worker collects the same shuffled order. No `-p no:randomly` needed.

## Plugins that need a small change

- **pytest-cov**: coverage is collected per worker and merged by the
  orchestrator, but only when `--cov` is on the rstest command line. From
  `addopts`, a parallel run measures coverage in every worker, never combines
  or reports it, and still exits 0. Move `--cov`/`--cov-report` to the command
  line.
- **pytest-reverse / pytest-ordering**: at `-n >= 2` the reordered list is
  only where dispatch starts. Cached slow tests (1s or more) move to the
  front and tests on different workers run concurrently, so a strict order
  doesn't hold. If the order matters, use `-n 0`,
  `--dist loadfile`, or fix the dependency.

## `[tool.rstest]` config (pyproject.toml)

Set defaults so contributors get the right behavior without remembering flags:

```toml
[tool.rstest]
numprocesses = "auto"   # or an int; "0" forces serial
dist = "load"           # load | loadfile | loadscope | loadgroup | each
# collect: leave unset; auto picks lazy for large warm-cache runs.
# Set "full" only if the suite needs every test module imported.
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
- run: rstest migrate-check --migrate-check-json mc.json \
        --migrate-allow tests/legacy/           # gate: non-zero on new issues
```

`--migrate-allow <substring>` (repeatable) accepts a known finding by
nodeid/site substring. It's still reported (marked `(allowed)`) but doesn't
fail the gate. Use `--output github` on the test run for inline PR annotations.

### GitHub Actions: the bundled action

On GitHub, prefer the bundled action over hand-written steps. It defaults
`--output github`, persists `.rstest_cache` across runs (durations and flake
history), and writes `junit.xml`:

```yaml
- uses: KovantAI/rstest/.github/actions/rstest@v0.8.0
  id: rstest
  with:
    python-version: "3.13"
    args: "-n auto"
    upload-junit: true
```

Pin a release tag (`v0.8.0` or later) or a commit SHA, and set `version:` to
pin the rstest wheel. Other inputs: `changed`, `durations-regress`,
`reruns`/`rerun-on`, `fail-under-ratio`, `shard`/`shard-total`. On other CI
systems, cache `.rstest_cache` yourself but exclude `.rstest_cache/replay`
(journals are per run).

### Keep the replay journal of a failed run

Every parallel run (except `--shard`, `--dist each`, or with
`RSTEST_NO_REPLAY_JOURNAL=1`) records its schedule to
`.rstest_cache/replay/latest.json`.
Upload it on failure so a CI-only failure can be replayed locally later
(`rstest replay --journal`, see the `rstest-triage` skill):

```yaml
- uses: actions/upload-artifact@v7
  if: failure()
  with:
    name: rstest-replay-${{ github.job }}-${{ strategy.job-index }}
    path: .rstest_cache/replay/latest.json
    if-no-files-found: ignore
```

### Sharding across jobs

```console
$ rstest -n 4 --shard "$K/$N" --report-json "shard.$K.json"
$ rstest shard-verify shard.*.json      # final job: no drops, no overlap
```

- Pin `-n` explicitly: `--shard` needs at least two workers, and `auto` can
  resolve to one, which makes the shard run exit 1.
- Buckets balance by the duration cache, so every shard job must restore the
  **same** cache; otherwise partitions can disagree. `shard-verify` catches
  that after the fact.
- Sharded runs write no replay journal.

### Shared cache without cache-key plumbing

`--cache-remote <dir|s3://…|gs://…>` with `--cache-pull` / `--cache-push`
(or `RSTEST_CACHE_REMOTE`) warms and publishes `.rstest_cache` through a
shared directory or bucket, using the `aws`/`gcloud` CLI already on the
runner. Useful when many jobs or branches should share one duration and flake
history. `--cache-pull`/`--cache-push` are refused at a monorepo root (each
project has its own cache).

### Hangs in CI

Set `--timeout SECS` so a stuck test fails with a traceback at the stuck line
instead of timing out the whole job. It also arms a hang backstop for code
that never returns to Python; `--worker-timeout SECS` sets that backstop
explicitly.
