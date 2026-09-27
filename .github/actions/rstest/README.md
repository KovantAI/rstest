# `rstest` GitHub Action

Run the [rstest](https://github.com/KovantAI/rstest) test runner in CI with the
boilerplate baked in: uv-aware install, a persistent (correctly-keyed)
`.rstest_cache`, `--changed` base-ref handling, and an optional fail-ratio gate
for nondeterministic (real-LLM) suites.

This action is a thin wrapper. It does **not** re-implement what the rstest CLI
already does natively, it just wires it into GitHub Actions:

| Concern | Handled by |
|---|---|
| `::error` per failed test, `::warning` for flaky reruns | rstest `--output github` (this action defaults to it) |
| Doctor diagnostics → job summary | rstest `--doctor` (auto-publishes to `$GITHUB_STEP_SUMMARY`) |
| Machine-readable diagnostics | rstest `--doctor-json` / `--doctor-md` (pass via `args`) |
| Fail CI on a doctor metric threshold | rstest `--doctor-fail-on` (this action's `doctor-fail-on` forwards to it) |
| No silent skip when `--changed` finds nothing | rstest `--changed-strict` (`changed: strict`) |
| Merge durations/flakes/coverage across shards & PRs (no clobber) | rstest shared-cache backend (`--cache-remote`/`--cache-pull`/`--cache-push`); this action's `cache-backend` wires it turnkey |
| Persist durations/flakes across runs (single job) | **this action** (`actions/cache`, the default `cache-backend`) |
| Tolerate N% failures (real-LLM) | **this action** (`fail-under-ratio`) |
| uv-native install/run | **this action** (`runner: auto`) |

## Usage

```yaml
- uses: KovantAI/rstest/.github/actions/rstest@v0.7.0
  with:
    python-version: "3.13"
    args: "-n auto"
```

Pin the action to a release tag (as above) or, for supply-chain hardening,
to a full commit SHA. Set `version:` to pin the rstest wheel too; without
it the action installs the latest rstest from PyPI.

### PR change-based selection (strict gate)

```yaml
- uses: actions/checkout@v7
  with:
    fetch-depth: 0            # --changed needs history to diff the base
- uses: KovantAI/rstest/.github/actions/rstest@v0.7.0
  with:
    changed: strict          # full run on unconnectable files; exit 5 on nothing-affected
    base-ref: origin/main
    durations-regress: "2.0" # cold cache warns + seeds; require-baseline:true to enforce
```

### Real-LLM / nondeterministic suite (fail-ratio gate)

```yaml
- uses: KovantAI/rstest/.github/actions/rstest@v0.7.0
  with:
    args: "-m acceptance -n 2"
    reruns: "2"
    rerun-on: "http-5xx,timeouts"   # -> --only-rerun regex
    fail-under-ratio: "0.20"        # tolerate <=20% assertion failures
    hard-fail-on: "AssertionError: config"   # ...but never tolerate these
    junit: junit.xml
```

### Gate on suite health (doctor metrics)

```yaml
- uses: KovantAI/rstest/.github/actions/rstest@v0.7.0
  with:
    args: "-n auto"
    doctor-fail-on: "parallel_efficiency<25, imbalance_pct>70"
```

Fails the job if realized parallel efficiency drops below 25% or worker
imbalance exceeds 70%. Metrics absent from the report (e.g. a single-worker run
has no `parallel_efficiency`) are skipped, not failed.

### Sharding

```yaml
strategy:
  matrix:
    shard: [1, 2, 3, 4]
steps:
  - uses: KovantAI/rstest/.github/actions/rstest@v0.7.0
    with:
      args: "-n 4"            # explicit: -n auto can resolve to 1 worker, which --shard rejects
      cache-backend: artifact # the default actions-cache backend is for single unsharded jobs
      shard: ${{ matrix.shard }}
      shard-total: 4
      upload-junit: true
```

> Cross-shard JUnit merge (one gate over the whole suite) is a **workflow-level**
> concern: each shard uploads its own JUnit; merge them in a downstream job.
> Shards balance by the duration cache, so use the `artifact` or `remote`
> [cache backend](#warm-cache-as-a-service), not the default `actions-cache`:
> with `actions-cache` every shard tries to save the same key (only the first
> wins) and shards can restore different entries. Even with a shared backend,
> shards pull at different times; see
> [Keep one cache snapshot across the matrix](https://python-rstest.readthedocs.io/en/stable/guides/sharding/#keep-one-cache-snapshot-across-the-matrix)
> for gating pipelines.

## Warm cache as a service

Duration-aware scheduling and sharding need warm timing data. In ephemeral CI
the cache is cold every run unless persisted, so you pay cold-run scheduling
forever. `cache-backend` productizes the [shared-cache
backend](https://github.com/KovantAI/rstest/blob/main/docs/concepts/caching.md#shared-cache-backend):
pull the authoritative baseline before the run, push this run's immutable
segment after, **merge-on-read**, so concurrent shards and PRs never clobber and
PR runs read newest-main.

| `cache-backend` | Backend | When |
|---|---|---|
| `actions-cache` (default) | one blob per key via `actions/cache` | single unsharded job; no cross-shard merge |
| `artifact` | GitHub artifacts, no external cloud, no secrets | the turnkey default for sharded / PR suites |
| `remote` | object store or HTTP endpoint (`cache-remote`) | teams already on S3/GCS/R2 or a shared mount |

### GitHub-native (no cloud, no secrets)

Warms from the latest successful run on `warm-from-branch` (default `main`) and
publishes each job's segment as an artifact. `actions: read` is what lets the
warm step reach a **prior** run's artifacts (a plain download sees only the
current run); `contents: read` is the checkout. No secrets, no external store:

```yaml
permissions: { contents: read, actions: read }
strategy: { matrix: { shard: [1, 2, 3, 4] } }
steps:
  - uses: actions/checkout@v7
  - uses: KovantAI/rstest/.github/actions/rstest@v0.7.0
    with:
      python-version: "3.13"
      cache-backend: artifact
      shard: ${{ matrix.shard }}
      shard-total: 4
      # optional: --changed reads the unioned coverage index
      args: "-n 4 --cov=<your_package> --cov-context=test --cov-report="
```

Run it on pushes to your default branch too, so those runs publish the segments
PR jobs warm from. First run is cold (nothing to union) and seeds the cache.
Keep those default-branch runs **full** (not `changed: true`): the artifact
backend warms from exactly one prior run, so a `--changed` main run leaves a
warm cache that only covers the tests it selected.

### Object store (S3 / GCS / HTTP): OIDC, no secrets in the URL

`cache-remote` drives the `aws` / `gcloud` CLI already on the runner (creds from
the OIDC role); `http(s)://` uses `cache-remote-token`:

```yaml
permissions: { id-token: write, contents: read }
steps:
  - uses: aws-actions/configure-aws-credentials@v4
    with: { role-to-assume: arn:aws:iam::…:role/ci, aws-region: us-east-1 }
  - uses: KovantAI/rstest/.github/actions/rstest@v0.7.0
    with:
      python-version: "3.13"
      cache-remote: s3://ci-cache/rstest        # gs://… or https://… too
      cache-compact-threshold: "500"            # fold segments inline past N
      shard: ${{ matrix.shard }}
      shard-total: 4
```

Setting `cache-remote` selects the `remote` backend automatically. A shared
mount (`cache-remote: /mnt/ci-cache/rstest`) needs no pull/push bookends beyond
the flags. Add `durations-regress` + `require-baseline: true` to make a cold or
failed pull a hard error instead of a silent green.

`id-token: write` is for the OIDC role assumption; the assumed role needs
`s3:ListBucket` + `s3:{Get,Put,Delete}Object` on the prefix (`Delete` only if
`cache-compact-threshold` is set or you run a `cache-compact` job): the
[full permission table](https://github.com/KovantAI/rstest/blob/main/docs/concepts/caching.md#transports)
covers GCS / Azure / HTTP.

## Inputs

| input | default | purpose |
|---|---|---|
| `args` | `-n auto` | extra rstest flags / paths, appended after the other flags. Split with shell quoting rules (`-k "a and b"` stays one argument), never glob-expanded or evaluated (Unreleased, 0.8.0; `@v0.7.0` word-splits it unquoted) |
| `python-version` | `""` | run `setup-python` at this version; else assume Python is set up |
| `runner` | `auto` | `uv` / `plain` / `auto` (uv when `uv.lock` or `[tool.uv]` present) |
| `install` | `""` | install override; empty = infer from `runner` |
| `version` | `""` | pin `rstest==X` (plain runner; uv uses the lockfile) |
| `working-directory` | `.` | project root (monorepo) |
| `cache` | `true` | restore/save `.rstest_cache`. Global kill switch: `false` disables caching for **every** backend (`actions-cache`, `artifact`, `remote`) |
| `cache-key-prefix` | `rstest-cache` | bump to invalidate all cached baselines |
| `cache-backend` | `actions-cache` | `actions-cache` / `artifact` / `remote`: see [Warm cache as a service](#warm-cache-as-a-service) |
| `cache-remote` | `""` | dir / `file://` / `s3://` / `gs://` / `http(s)://` remote; non-empty ⇒ `remote` backend |
| `cache-remote-token` | `""` | bearer for an `http(s)://` remote → `RSTEST_CACHE_REMOTE_TOKEN` |
| `cache-compact-threshold` | `""` | `--cache-compact-threshold N`: fold loose segments inline on push past N (best-effort) |
| `warm-from-branch` | `main` | artifact backend: branch whose latest successful run seeds the warm cache |
| `warm-from-event` | `push` | **Unreleased (0.8.0), not in `@v0.7.0`.** Artifact backend: only warm from a run triggered by this event (empty = any); keeps PR runs from becoming the warm source |
| `artifact-suffix` | derived | **Unreleased (0.8.0), not in `@v0.7.0`.** Scopes artifact names per matrix leg; default is `<os>-py<version>[-<working-directory>]` |
| `artifact-cache-dir` | `.rstest-rcache` | artifact backend: workspace dir segments materialize into |
| `github-token` | job token | artifact backend: token for the cross-run resolve + download (needs `actions: read`) |
| `output` | `github` | `--output` style: `github` (annotations), `gitlab`, `buildkite`, `teamcity`, `azure`, `tap`, `json`, `dots`, `verbose`, `bar`. An unknown value only warns and falls back to `dots` |
| `junit` | `junit.xml` | `--junitxml` path; empty = skip (required for the gate) |
| `changed` | `false` | `false` / `true` / `strict` |
| `base-ref` | `""` | base ref for `--changed`; fetched if shallow. Empty on a PR = inferred from `$GITHUB_BASE_REF` (`origin/<base>`) |
| `reruns` | `""` | `--reruns N` |
| `rerun-on` | `""` | preset(s) → `--only-rerun` (`http-5xx`, `timeouts`, or raw regex) |
| `worker-timeout` | `""` | `--worker-timeout SECS` (hang / container-boot backstop) |
| `durations-regress` | `""` | `--durations-regress RATIO` (cold cache warns; see `require-baseline`) |
| `require-baseline` | `false` | strict: fail if no baseline. Default only warns: the first run legitimately has none and seeds it |
| `doctor` | `false` | add `--doctor` |
| `doctor-fail-on` | `""` | fail on doctor metrics, e.g. `parallel_efficiency<30, imbalance_pct>60` (each forwarded to native `--doctor-fail-on`; breach fails via exit code, report auto-published to job summary; inapplicable metrics skipped) |
| `quarantine` | `""` | `--quarantine FILE` |
| `shard` / `shard-total` | `""` | `--shard K/N` |
| `fail-under-ratio` | `""` | max tolerated assertion-failure fraction (0–1); non-test exit codes still fail (Unreleased, 0.8.0; see [Security and matrix behavior](#security-and-matrix-behavior)) |
| `hard-fail-on` | `""` | regex; matching failures fail immediately, bypassing the ratio |
| `upload-junit` | `false` | upload JUnit as an artifact |

## Outputs

| output | meaning |
|---|---|
| `exit-code` | rstest exit code (before the fail-ratio gate) |
| `junit-path` | JUnit path written (empty if none) |
| `passed` / `failed` | test counts parsed from JUnit (when the gate ran) |

## Cache design

The cache key is `${prefix}-${os}-py${version}-${hash(lockfile)}-${run_id}` with
a `${...}-` restore-key. Two deliberate choices:

- **`run_id` suffix + prefix restore-key**: `actions/cache` never re-saves an
  existing key, so a stable key would freeze the cache at a branch's first run.
  A unique key that always misses, falling through `restore-keys` to the newest
  match, is the standard "newest-wins" pattern.
- **`os` + `python-version` + lockfile-hash segmentation**: durations and flake
  history are interpreter-specific. Without this, a 3.13 run would seed a 3.12
  shard's baseline. Segmenting keeps each matrix leg's baseline separate.

To seed a shared baseline for PR shards, run the full unsharded suite on your
default branch (a normal run of this action on `push` writes the cache); PR runs
restore the newest matching entry read-only.

## Security and matrix behavior

> **Unreleased.** Everything in this section describes the action on `main`,
> which ships with rstest 0.8.0. The `@v0.7.0` action pastes inputs into its
> scripts, has no `artifact-suffix` or `warm-from-event` input (artifact names
> are unscoped, and any successful run on `warm-from-branch` can seed the warm
> cache), and its fail-ratio gate judges only the JUnit ratio.

- **Inputs never reach the shell as code.** Every input is passed to the
  action's scripts through `env:` and quoted, so a value such as a branch name
  can't inject commands. The one deliberate exception is `install`, which is a
  shell command you write yourself and is evaluated as such.
- **`args` uses shell quoting rules.** It is split like a shell would split it
  (`-k "a and b"` stays one argument), but never glob-expanded or evaluated, so
  `$(...)` and backticks are passed literally. Unbalanced quotes fail the step.
- **The fail-ratio gate only tolerates test failures.** It checks rstest's exit
  code first: an interrupt (2), internal error or lost worker (3), pytest usage
  error (4), or no tests collected (5) fails the job whatever the ratio, and so
  does exit 1 with no failing test in the JUnit (a gating flag such as
  `doctor-fail-on` fired, or rstest refused the run). When tests failed *and* a
  gating flag fired, the gate can't tell them apart and judges by the ratio.
- **Each matrix leg gets its own artifact names.** Segments are named
  `rstest-seg-<suffix>--<run_id>-<shard>` and JUnit artifacts
  `rstest-junit-<suffix>[-shard-K]`, where `<suffix>` is `artifact-suffix`
  (default: runner OS, Python version and `working-directory`, e.g.
  `Linux-py3.13-libs-core`). The warm step pulls only its own leg's segments, so
  interpreters and projects never mix. Set `artifact-suffix` yourself when legs
  differ in something else (a dependency matrix, say).
- **Only trusted runs seed the artifact cache.** The warm lookup takes the
  latest successful run on `warm-from-branch` triggered by `warm-from-event`
  (default `push`), so a pull_request run, including one from a fork with a
  branch named `main`, never becomes the warm source. See the
  [trust boundary](https://python-rstest.readthedocs.io/en/stable/concepts/caching/#trust-boundary)
  guidance.
- **`changed` with nothing affected writes no JUnit.** rstest prints
  `rstest: no tests affected …` and exits before running anything, so no
  `--junitxml` or `--report-json` is written. The JUnit upload ignores the
  missing file, and the fail-ratio gate passes an exit-0 run with no report.
  With `changed: strict` the exit code is 5 and the job fails, by design. Use
  `if-no-files-found: ignore` on your own report uploads.

### Upgrading from an earlier version of this action

Artifact names now carry the leg suffix. Two consequences:

- The first artifact-backend run after upgrading starts cold, because earlier
  segments were named `rstest-seg-<run_id>-<shard>` and don't match the new
  pattern. It re-seeds itself.
- Workflows that download JUnit by exact name (`rstest-junit` or
  `rstest-junit-shard-K`) need the new names. A `pattern: rstest-junit-*` with
  `merge-multiple: true` works across all legs and shards.

## Notes

- The fail-ratio gate parses JUnit with DTDs rejected (blocks XXE /
  billion-laughs); it uses `defusedxml` if installed, else a hardened stdlib
  parser.
- rstest is on PyPI, so the default install works with no wheel URL.
- This is a **subdir composite action**, so it is not on the Marketplace and the
  `uses:` path is long.
