# `rstest` GitHub Action

Run the [rstest](https://github.com/KovantAI/rstest) test runner in CI with the
boilerplate baked in: uv-aware install, a persistent (correctly-keyed)
`.rstest_cache`, `--changed` base-ref handling, and an optional fail-ratio gate
for nondeterministic (real-LLM) suites.

This action is a thin wrapper. It does **not** re-implement what the rstest CLI
already does natively — it just wires it into GitHub Actions:

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
- uses: KovantAI/rstest/.github/actions/rstest@v1
  with:
    python-version: "3.13"
    args: "-n auto"
```

### PR change-based selection (strict gate)

```yaml
- uses: actions/checkout@v4
  with:
    fetch-depth: 0            # --changed needs history to diff the base
- uses: KovantAI/rstest/.github/actions/rstest@v1
  with:
    changed: strict          # full run on unconnectable files; exit 5 on nothing-affected
    base-ref: origin/main
    durations-regress: "2.0" # cold cache warns + seeds; require-baseline:true to enforce
```

### Real-LLM / nondeterministic suite (fail-ratio gate)

```yaml
- uses: KovantAI/rstest/.github/actions/rstest@v1
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
- uses: KovantAI/rstest/.github/actions/rstest@v1
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
  - uses: KovantAI/rstest/.github/actions/rstest@v1
    with:
      shard: ${{ matrix.shard }}
      shard-total: 4
      upload-junit: true
```

> Cross-shard JUnit merge (one gate over the whole suite) is a **workflow-level**
> concern — each shard uploads its own JUnit; merge them in a downstream job.
> All shards must restore the **same** duration cache to balance — the
> `artifact` / `remote` cache backend below does this natively (each shard pushes
> its segment; every job pulls the union), replacing the seed-from-unsharded-run
> dance.

## Warm cache as a service

Duration-aware scheduling and sharding need warm timing data. In ephemeral CI
the cache is cold every run unless persisted, so you pay cold-run scheduling
forever. `cache-backend` productizes the [shared-cache
backend](https://github.com/KovantAI/rstest/blob/main/docs/concepts/caching.md#shared-cache-backend):
pull the authoritative baseline before the run, push this run's immutable
segment after — **merge-on-read**, so concurrent shards and PRs never clobber and
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
  - uses: actions/checkout@v4
  - uses: KovantAI/rstest/.github/actions/rstest@v1
    with:
      python-version: "3.13"
      cache-backend: artifact
      shard: ${{ matrix.shard }}
      shard-total: 4
      # optional: --changed reads the unioned coverage index
      args: "-n auto --cov=<your_package> --cov-context=test --cov-report="
```

Run it on pushes to your default branch too, so those runs publish the segments
PR jobs warm from. First run is cold (nothing to union) and seeds the cache.

### Object store (S3 / GCS / HTTP) — OIDC, no secrets in the URL

`cache-remote` drives the `aws` / `gcloud` CLI already on the runner (creds from
the OIDC role); `http(s)://` uses `cache-remote-token`:

```yaml
permissions: { id-token: write, contents: read }
steps:
  - uses: aws-actions/configure-aws-credentials@v4
    with: { role-to-assume: arn:aws:iam::…:role/ci, aws-region: us-east-1 }
  - uses: KovantAI/rstest/.github/actions/rstest@v1
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
`cache-compact-threshold` is set or you run a `cache-compact` job) — the
[full permission table](https://github.com/KovantAI/rstest/blob/main/docs/concepts/caching.md#transports)
covers GCS / Azure / HTTP.

## Inputs

| input | default | purpose |
|---|---|---|
| `args` | `-n auto` | extra rstest flags / paths, appended verbatim |
| `python-version` | `""` | run `setup-python` at this version; else assume Python is set up |
| `runner` | `auto` | `uv` / `plain` / `auto` (uv when `uv.lock` or `[tool.uv]` present) |
| `install` | `""` | install override; empty = infer from `runner` |
| `version` | `""` | pin `rstest==X` (plain runner; uv uses the lockfile) |
| `working-directory` | `.` | project root (monorepo) |
| `cache` | `true` | restore/save `.rstest_cache` (`actions-cache` backend) |
| `cache-key-prefix` | `rstest-cache` | bump to invalidate all cached baselines |
| `cache-backend` | `actions-cache` | `actions-cache` / `artifact` / `remote` — see [Warm cache as a service](#warm-cache-as-a-service) |
| `cache-remote` | `""` | dir / `file://` / `s3://` / `gs://` / `http(s)://` remote; non-empty ⇒ `remote` backend |
| `cache-remote-token` | `""` | bearer for an `http(s)://` remote → `RSTEST_CACHE_REMOTE_TOKEN` |
| `cache-compact-threshold` | `""` | `--cache-compact-threshold N`: fold loose segments inline on push past N (best-effort) |
| `warm-from-branch` | `main` | artifact backend: branch whose latest successful run seeds the warm cache |
| `artifact-cache-dir` | `.rstest-rcache` | artifact backend: workspace dir segments materialize into |
| `github-token` | job token | artifact backend: token for the cross-run resolve + download (needs `actions: read`) |
| `output` | `github` | `--output` style (`github` gives annotations) |
| `junit` | `junit.xml` | `--junitxml` path; empty = skip (required for the gate) |
| `changed` | `false` | `false` / `true` / `strict` |
| `base-ref` | `""` | base ref for `--changed`; fetched if shallow. Empty on a PR = inferred from `$GITHUB_BASE_REF` (`origin/<base>`) |
| `reruns` | `""` | `--reruns N` |
| `rerun-on` | `""` | preset(s) → `--only-rerun` (`http-5xx`, `timeouts`, or raw regex) |
| `worker-timeout` | `""` | `--worker-timeout SECS` (hang / container-boot backstop) |
| `durations-regress` | `""` | `--durations-regress RATIO` (cold cache warns; see `require-baseline`) |
| `require-baseline` | `false` | strict: fail if no baseline. Default only warns — the first run legitimately has none and seeds it |
| `doctor` | `false` | add `--doctor` |
| `doctor-fail-on` | `""` | fail on doctor metrics, e.g. `parallel_efficiency<30, imbalance_pct>60` (each forwarded to native `--doctor-fail-on`; breach fails via exit code, report auto-published to job summary; inapplicable metrics skipped) |
| `quarantine` | `""` | `--quarantine FILE` |
| `shard` / `shard-total` | `""` | `--shard K/N` |
| `fail-under-ratio` | `""` | max tolerated assertion-failure fraction (0–1) |
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

- **`run_id` suffix + prefix restore-key** — `actions/cache` never re-saves an
  existing key, so a stable key would freeze the cache at a branch's first run.
  A unique key that always misses, falling through `restore-keys` to the newest
  match, is the standard "newest-wins" pattern.
- **`os` + `python-version` + lockfile-hash segmentation** — durations and flake
  history are interpreter-specific. Without this, a 3.13 run would seed a 3.12
  shard's baseline. Segmenting keeps each matrix leg's baseline separate.

To seed a shared baseline for PR shards, run the full unsharded suite on your
default branch (a normal run of this action on `push` writes the cache); PR runs
restore the newest matching entry read-only.

## Notes

- The fail-ratio gate parses JUnit with DTDs rejected (blocks XXE /
  billion-laughs); it uses `defusedxml` if installed, else a hardened stdlib
  parser.
- rstest is on PyPI, so the default install works with no wheel URL.
- This is a **subdir composite action**, so it is not on the Marketplace and the
  `uses:` path is long. For 4thbrain repos it is consumed indirectly through the
  `4thbrain/actions` reusable workflow, which hides the path.
