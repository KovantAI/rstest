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
- uses: KovantAI/rstest/.github/actions/rstest@v0.8.0
  with:
    python-version: "3.13"
    args: "-n auto"
```

Pin the action to a release tag (as above) or, for supply-chain hardening,
to a full commit SHA. Set `version:` to pin the rstest wheel too; without
it the action installs the latest rstest from PyPI. With `runner: uv` (the
default when the project has a `uv.lock` or `[tool.uv]`) the action runs
`uv sync --dev` and ignores `version:`: pin rstest in your lockfile instead.

### PR change-based selection (strict gate)

```yaml
- uses: actions/checkout@v7
  with:
    fetch-depth: 0            # --changed needs history to diff the base
- uses: KovantAI/rstest/.github/actions/rstest@v0.8.0
  with:
    changed: strict          # full run on unconnectable files; exit 5 on nothing-affected
    base-ref: origin/main
    durations-regress: "2.0" # cold cache warns + seeds; require-baseline:true to enforce
```

On a push event the action picks a base that can
actually differ: `base-ref` if it is not the pushed commit itself, else the
commit before the push (`github.event.before`). With no usable base (a new
branch, a `schedule` or `workflow_dispatch` run, or a PR whose base equals
`HEAD`) it warns and runs the full suite, rather than diffing against `HEAD`,
which in a clean checkout selects nothing and passes green (or exits 5 under
`strict`). So the example above is safe on both `pull_request` and pushes to
`main`.

### Real-LLM / nondeterministic suite (fail-ratio gate)

```yaml
- uses: KovantAI/rstest/.github/actions/rstest@v0.8.0
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
- uses: KovantAI/rstest/.github/actions/rstest@v0.8.0
  with:
    args: "-n auto"
    doctor-fail-on: "parallel_efficiency<25, imbalance_pct>70"
```

Fails the job if realized parallel efficiency drops below 25% or worker
imbalance exceeds 70%. Metrics absent from the report (e.g. a single-worker run
has no `parallel_efficiency`) are skipped, not failed.

### Sharding

Shards balance by the duration cache, so every shard must partition from the
same cache snapshot. Use the `artifact` (or `remote`)
[cache backend](#warm-cache-as-a-service), not the default `actions-cache`:
with `actions-cache` each shard saves and restores on its own (every shard
tries to save the same key and only the first wins), so shards can restore
different entries. Resolve the warm run **once** upstream and pass it to every
shard as `warm-run-id`; otherwise each shard resolves its own and can warm from
a different run:

```yaml
permissions: { contents: read, actions: read }
jobs:
  warm:
    runs-on: ubuntu-latest
    outputs: { run-id: "${{ steps.warm.outputs.run-id }}" }
    steps:
      # Same step as docs/_snippets/warm-run-step.md (kept identical by a test).
      - id: warm
        env:
          GH_TOKEN: ${{ github.token }}
        shell: bash  # bash syntax: Windows runners default to PowerShell
        run: |
          wf="${GITHUB_WORKFLOW_REF##*/.github/workflows/}"; wf="${wf%%@*}"
          rid=$(gh run list --repo "$GITHUB_REPOSITORY" --workflow "$wf" \
                  --branch main --event push --status success --limit 1 \
                  --json databaseId --jq '.[0].databaseId // ""')
          echo "run-id=$rid" >> "$GITHUB_OUTPUT"
        continue-on-error: true
  test:
    needs: warm
    strategy: { fail-fast: false, matrix: { shard: [1, 2, 3, 4] } }
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v7
      - uses: KovantAI/rstest/.github/actions/rstest@v0.8.0
        with:
          args: "-n 4"            # explicit: -n auto can resolve to 1 worker, which --shard rejects
          cache-backend: artifact
          warm-run-id: ${{ needs.warm.outputs.run-id }}
          shard: ${{ matrix.shard }}
          shard-total: 4
          upload-junit: true
```

Cross-shard JUnit merge and the `rstest shard-verify` coverage gate are
**workflow-level** concerns: each shard uploads its own JUnit; merge and verify
in a downstream job. The full recipe, including the verify job, is in
[Sharding: GitHub Actions](https://python-rstest.readthedocs.io/en/stable/guides/sharding/#github-actions);
the reasoning is in
[Keep one cache snapshot across the matrix](https://python-rstest.readthedocs.io/en/stable/guides/sharding/#keep-one-cache-snapshot-across-the-matrix).

### Monorepos

Run one matrix job per package, with `working-directory` set to the package:

```yaml
jobs:
  test:
    runs-on: ubuntu-latest
    strategy:
      fail-fast: false
      matrix:
        project: [libs/core, libs/cli, services/api]
    steps:
      - uses: actions/checkout@v7
      - uses: KovantAI/rstest/.github/actions/rstest@v0.8.0
        with:
          python-version: "3.13"
          working-directory: ${{ matrix.project }}
          cache-backend: artifact
          upload-junit: true
```

Each job is a plain single-project run: the package's own `[tool.rstest]`,
`.venv` or lockfile, and `.rstest_cache` apply, and artifact names carry the
package (`artifact-suffix` defaults to `<os>-py<version>-<working-directory>`).
For a package in a root uv workspace, set `runner: uv`.

The action refuses a monorepo root (a `working-directory` with no pytest
config of its own and several subprojects, or `[tool.rstest] projects`),
using the same rule rstest uses to enter monorepo mode. A root run keeps a
cache and a `junit.<slug>.xml` per project and refuses
`--cache-pull`/`--cache-push`, none of which fit this action's cache path,
JUnit upload or fail-ratio gate. Full recipe, including the single-job root alternative:
[monorepo on ephemeral CI](https://python-rstest.readthedocs.io/en/stable/guides/ci-quickstart/#worked-example-monorepo-on-ephemeral-ci).

## Warm cache as a service

Duration-aware scheduling and sharding need warm timing data. In ephemeral CI
the cache is cold every run unless persisted, so you pay cold-run scheduling
forever. `cache-backend` productizes the [shared-cache
backend](https://python-rstest.readthedocs.io/en/stable/concepts/caching/#shared-cache-backend):
pull the authoritative baseline before the run, push this run's immutable
segment after, **merge-on-read**, so concurrent shards and PRs never clobber and
PR runs read newest-main.

| `cache-backend` | Backend | When |
|---|---|---|
| `actions-cache` (default) | one blob per key via `actions/cache` | a single unsharded job; no cross-shard merge, so not for a shard matrix |
| `artifact` | GitHub artifacts, no external cloud, no secrets | shard matrices (with `warm-run-id` for a gating matrix, see [Sharding](#sharding)) |
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
  - uses: KovantAI/rstest/.github/actions/rstest@v0.8.0
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
  - uses: KovantAI/rstest/.github/actions/rstest@v0.8.0
    with:
      python-version: "3.13"
      cache-remote: s3://ci-cache/rstest        # gs://… or https://… too
      cache-compact-threshold: "50"             # fold segments inline past N (keep 20-50)
      shard: ${{ matrix.shard }}
      shard-total: 4
```

Setting `cache-remote` selects the `remote` backend automatically. A shared
mount (`cache-remote: /mnt/ci-cache/rstest`) needs no pull/push bookends beyond
the flags. A failed pull is always a hard error (exit 1). Add
`durations-regress` + `require-baseline: true` so a pull that succeeds but
brings no baseline (a cold remote) is one too, instead of a silent green.

**Only trusted runs write.** With the default `cache-push: auto`, the
`remote` backend pushes only on `push`, `schedule` and `workflow_dispatch`
runs of `warm-from-branch`, and on `merge_group`. Every other job (pull
requests, feature-branch pushes, `issue_comment` or `workflow_run` jobs)
reads the baseline but writes no segment. Set `cache-push: true` to push
anyway, or `false` for a pull-only job on any event. With the
`actions-cache` backend, `auto` saves on `push`, `pull_request` (whose cache
only that PR can restore), `schedule`, `workflow_dispatch` and
`merge_group`, but not on `pull_request_target`, `issue_comment` or
`workflow_run`, which save into the base branch's scope.

`cache-push` only decides whether rstest pushes. The job's credentials are
the real boundary, because test code can read whatever the job holds: the
action passes `cache-remote-token` only to the rstest command, and rstest
keeps it out of the test processes' environment, but a test running as the
same user can still read the rstest process's environment. Give PR jobs a
read-only role or token, for example by choosing the role per event:

```yaml
  - uses: aws-actions/configure-aws-credentials@v4
    with:
      role-to-assume: ${{ github.event_name == 'push' && github.ref == 'refs/heads/main' && 'arn:aws:iam::…:role/ci-cache-write' || 'arn:aws:iam::…:role/ci-cache-read' }}
      aws-region: us-east-1
```

`id-token: write` is for the OIDC role assumption; the assumed role needs
`s3:ListBucket` + `s3:{Get,Put,Delete}Object` on the prefix (`Delete` only if
`cache-compact-threshold` is set or you run a `cache-compact` job): the
[full permission table](https://python-rstest.readthedocs.io/en/stable/concepts/caching/#transports)
covers GCS / Azure / HTTP.

## Inputs

| input | default | purpose |
|---|---|---|
| `args` | `-n auto` | extra rstest flags / paths, appended after the other flags. Split with shell quoting rules (`-k "a and b"` stays one argument), never glob-expanded or evaluated |
| `python-version` | `""` | run `setup-python` at this version; else assume Python is set up |
| `runner` | `auto` | `uv` / `plain` / `auto` (uv when `uv.lock` or `[tool.uv]` present) |
| `install` | `""` | install override; empty = infer from `runner` |
| `version` | `""` | pin `rstest==X` (plain runner; uv uses the lockfile) |
| `working-directory` | `.` | one project's directory (in a monorepo, a package); a monorepo root is refused, see [Monorepos](#monorepos) |
| `cache` | `true` | restore/save `.rstest_cache`. Global kill switch: `false` disables caching for **every** backend (`actions-cache`, `artifact`, `remote`) |
| `cache-key-prefix` | `rstest-cache` | bump to invalidate all cached baselines |
| `cache-backend` | `actions-cache` | `actions-cache` / `artifact` / `remote`: see [Warm cache as a service](#warm-cache-as-a-service) |
| `cache-remote` | `""` | dir / `file://` / `s3://` / `gs://` / `http(s)://` remote; non-empty ⇒ `remote` backend |
| `cache-remote-token` | `""` | bearer for an `http(s)://` remote → `RSTEST_CACHE_REMOTE_TOKEN` |
| `cache-push` | `auto` | whether this job writes the cache: `auto` = `remote` pushes only from `push`/`schedule`/`workflow_dispatch` on `warm-from-branch` and `merge_group`; `actions-cache` saves only on `push`/`pull_request`/`schedule`/`workflow_dispatch`/`merge_group` (any other event, e.g. `pull_request_target` or `release`, never saves). `true` always, `false` never |
| `cache-compact-threshold` | `""` | `--cache-compact-threshold N`: fold loose segments inline on push past N (best-effort) |
| `warm-from-branch` | `main` | your trusted branch. Artifact backend: its latest successful run seeds the warm cache. `remote` backend with `cache-push: auto`: the only branch whose runs push |
| `warm-run-id` | `""` | Artifact backend: warm from this run id instead of resolving one per job; pass one id resolved upstream to every shard so they share a snapshot |
| `warm-from-event` | `push` | Artifact backend: only warm from a run triggered by this event (empty = any); keeps PR runs from becoming the warm source |
| `artifact-suffix` | derived | Scopes artifact names per matrix leg; default is `<os>-py<version>[-<working-directory>]` |
| `artifact-cache-dir` | `.rstest-rcache` | artifact backend: workspace dir segments materialize into |
| `github-token` | job token | artifact backend: token for the cross-run resolve + download (needs `actions: read`) |
| `output` | `github` | `--output` style: `github` (annotations), `gitlab`, `buildkite`, `teamcity`, `azure`, `tap`, `json`, `dots`, `verbose`, `bar`. An unknown value only warns and falls back to `dots` |
| `junit` | `junit.xml` | `--junitxml` path; empty = skip (required for the gate) |
| `changed` | `false` | `false` / `true` / `strict`. With no usable base the full suite runs with a warning (see [PR change-based selection](#pr-change-based-selection-strict-gate)) |
| `base-ref` | `""` | base ref for `--changed`; fetched if shallow. Empty on a PR = inferred from `$GITHUB_BASE_REF` (`origin/<base>`); empty on a push = the commit before the push |
| `reruns` | `""` | `--reruns N` |
| `rerun-on` | `""` | comma-separated preset(s) → `--only-rerun` (`http-5xx`, `timeouts`, or raw regex, passed verbatim after trimming surrounding whitespace) |
| `worker-timeout` | `""` | `--worker-timeout SECS` (hang / container-boot backstop) |
| `durations-regress` | `""` | `--durations-regress RATIO` (cold cache warns; see `require-baseline`) |
| `require-baseline` | `false` | strict: fail if no baseline. Default only warns: the first run legitimately has none and seeds it |
| `doctor` | `false` | add `--doctor` |
| `doctor-fail-on` | `""` | fail on doctor metrics, e.g. `parallel_efficiency<30, imbalance_pct>60` (each forwarded to native `--doctor-fail-on`; breach fails via exit code, report auto-published to job summary; inapplicable metrics skipped) |
| `quarantine` | `""` | `--quarantine FILE` |
| `shard` / `shard-total` | `""` | `--shard K/N`; set both or neither (one alone fails the step) |
| `fail-under-ratio` | `""` | max tolerated assertion-failure fraction (0–1); non-test exit codes still fail (see [Security and matrix behavior](#security-and-matrix-behavior)) |
| `hard-fail-on` | `""` | regex; matching failures fail immediately, bypassing the ratio |
| `upload-junit` | `false` | upload JUnit as an artifact |

## Outputs

| output | meaning |
|---|---|
| `exit-code` | rstest exit code (before the fail-ratio gate) |
| `junit-path` | JUnit path written (empty if none) |
| `passed` / `failed` | test counts parsed from JUnit; set only when the fail-ratio gate ran (`fail-under-ratio` set) |

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

To seed the baseline PR runs restore, run the suite on your default branch (a
normal run of this action on `push` writes the cache). PR runs restore the
newest matching entry; anything a PR run saves is scoped to that PR, so it never
replaces the default branch's baseline. This design is for a single unsharded
job; for a shard matrix use the `artifact` backend (see [Sharding](#sharding)).

## Security and matrix behavior

> Tags before `v0.8.0` lack these fixes: they paste inputs into their
> scripts and have no `artifact-suffix` or `warm-from-event` input.

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
  `rstest-seg-<suffix>--<run_id>-<attempt>-<shard>` and JUnit artifacts
  `rstest-junit-<suffix>[-shard-K]`, where `<suffix>` is `artifact-suffix`
  (default: runner OS, Python version and `working-directory`, e.g.
  `Linux-py3.13-libs-core`). The warm step pulls only its own leg's segments, so
  interpreters and projects never mix. Set `artifact-suffix` yourself when legs
  differ in something else (a dependency matrix, say).
- **"Re-run failed jobs" works.** Artifact names are unique per run across
  attempts, so segment names carry `github.run_attempt`, and the next warm
  merges the segments of every attempt. The JUnit artifact keeps its stable
  name and is overwritten, so it always holds the latest attempt's result.
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
  `rstest-junit-shard-K`) need the new names. Download them with
  `pattern: rstest-junit-*` and **without** `merge-multiple`: every artifact
  holds one file named after the `junit` input (`junit.xml` by default), so
  merging them into one directory keeps only one leg's file. Each artifact
  lands in its own subdirectory instead; read `*/junit.xml`.

## Notes

- The fail-ratio gate parses JUnit with DTDs rejected (blocks XXE /
  billion-laughs); it uses `defusedxml` if installed, else a hardened stdlib
  parser.
- rstest is on PyPI, so the default install works with no wheel URL.
- This is a **subdir composite action**, so it is not on the Marketplace and the
  `uses:` path is long.
