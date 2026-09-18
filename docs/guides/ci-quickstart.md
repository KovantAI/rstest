# CI quickstart

rstest behaves like pytest in CI: exit-code discipline, JUnit XML for your
test-report integration, and `--report-json` for tooling, with quiet human
output (machine consumers should parse the files, not stdout).

On top of that, rstest adds two things worth wiring up in CI: worker
parallelism with no extra plugin, and a duration cache that makes scheduling
smarter when persisted between runs.

This page gets you running on **GitHub Actions** and walks two worked examples
(Django, monorepo). For other CI systems — AWS CodeBuild, Google Cloud Build,
GitLab, Azure, CircleCI, Jenkins, pre-commit — see [More CI
systems](ci-recipes.md). For a shard matrix that needs a cache no native CI
cache can merge, see [Shared cache across CI jobs](ci-shared-cache.md).

!!! tip "Pin for reproducible CI"
    The recipes use a bare `pip install rstest`. For reproducible builds,
    pin a version (`pip install rstest==0.7.0` or `rstest~=0.3`) or install
    from your lockfile.

## Which layout do I want?

| Your situation | Layout | Where |
|---|---|---|
| Single project | one job, `rstest -n auto`, cache `.rstest_cache` | [GitHub Actions](#github-actions) below |
| Monorepo, **few** packages (≤ runner cores) | one root job, `cd repo && rstest`, merged report | [Monorepos guide](monorepo.md) |
| Monorepo, **many** packages | one job **per package** via a matrix | [Monorepo worked example](#worked-example-monorepo-on-ephemeral-ci) |
| One long suite you split across CI nodes | `--shard K/N` matrix + [shared cache](ci-shared-cache.md) | [Sharding](sharding.md) + [Shared cache](ci-shared-cache.md) |

The rule of thumb: **the unit of CI parallelism should be the project, not the
root** once you have more packages than runner cores — one job per package
gives each the full runner and its own cache. Reach for `--shard` only when a
*single* project's suite is itself the long pole.

## GitHub Actions

The quickest path is the bundled composite action, which wraps install, the
duration cache (correctly keyed), `--changed` base-ref handling, and an
optional fail-ratio gate:

```yaml
jobs:
  tests:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: KovantAI/rstest/.github/actions/rstest@v1
        with:
          python-version: "3.13"
          args: "-n auto"
          upload-junit: true
```

That defaults `--output github` (so failures show as `::error` annotations and
flaky reruns as `::warning`), persists `.rstest_cache` across runs, and writes
`junit.xml`. See the [action README][action] for all inputs (`changed`,
`durations-regress`, `reruns`/`rerun-on`, `fail-under-ratio`, `shard`, …).

[action]: https://github.com/KovantAI/rstest/tree/main/.github/actions/rstest

### Under the hood

The action is a thin wrapper. If you prefer raw YAML — or need something the
action does not expose — the equivalent steps are:

```yaml
jobs:
  tests:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-python@v5
        with:
          python-version: "3.13"

      - name: install
        run: |
          pip install -r requirements.txt
          pip install rstest

      # Persist the duration cache: from the second run on, the scheduler
      # starts the slowest tests first.
      - uses: actions/cache@v4
        with:
          path: .rstest_cache
          # Unique key per run: actions/cache never RE-saves an
          # existing key, so a ref-only key freezes the cache at the
          # branch's first run. restore-keys picks the newest match.
          key: rstest-durations-${{ github.ref_name }}-${{ github.run_id }}
          restore-keys: |
            rstest-durations-${{ github.ref_name }}-
            rstest-durations-

      - name: test
        # --output github emits ::error per failure and ::warning for flaky
        # reruns; --doctor auto-publishes diagnostics to the job summary.
        run: rstest -n auto --output github --junitxml junit.xml

      # Long pole? Fan the suite across a runner matrix with --shard K/N —
      # see the Sharding guide.

      # Monorepo roots: caches live in EACH project (.rstest_cache per
      # package — widen the cache path to **/.rstest_cache), and junit
      # files are written per project as junit.<slug>.xml — glob them
      # in the artifact step.

      - uses: actions/upload-artifact@v4
        if: always()
        with:
          name: junit
          path: junit.xml
```

For a shard matrix, swap the `actions/cache` step for the segment-merge
[shared cache](ci-shared-cache.md) — it sidesteps the `run_id` key dance and
lets every shard write without a single-writer job.

## Worked example: Django on ephemeral CI

A Django suite is the common case: pytest-django, a real database, ephemeral
GitHub runners where nothing survives between runs unless you persist it. The
two things people get wrong are the **cold-vs-warm cache** and **per-worker
databases** — both are handled below.

pytest-django is exercised continuously in rstest's battery *including
per-worker test databases under parallelism* (see [Plugins](plugins.md)):
rstest supplies each worker the xdist-style worker identity pytest-django keys
off, so every worker gets its own isolated test DB (`test_app_gw0`,
`test_app_gw1`, …) automatically — no extra flags, same as under xdist.

```yaml
# .github/workflows/tests.yml
jobs:
  tests:
    runs-on: ubuntu-latest
    services:
      postgres:
        image: postgres:16
        env: { POSTGRES_USER: ci, POSTGRES_PASSWORD: ci, POSTGRES_DB: app }
        ports: ["5432:5432"]
        # createdb privilege matters: each worker CREATEs its own test DB
        options: >-
          --health-cmd "pg_isready -U ci" --health-interval 5s
          --health-timeout 5s --health-retries 5
    env:
      DJANGO_SETTINGS_MODULE: myapp.settings.test
      DATABASE_URL: postgres://ci:ci@localhost:5432/app
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-python@v5
        with: { python-version: "3.13" }
      - run: pip install -r requirements.txt && pip install rstest==0.7.0

      # The cache is what makes run two fast. Unique key per run (actions/cache
      # never re-saves an existing key); restore-keys picks the newest match.
      - uses: actions/cache@v4
        with:
          path: .rstest_cache
          key: rstest-${{ github.ref_name }}-${{ github.run_id }}
          restore-keys: |
            rstest-${{ github.ref_name }}-
            rstest-

      # --reuse-db keeps the migrated test DB across runs on a warm workspace;
      # on ephemeral runners the DB is fresh each time, so it's a no-op there —
      # harmless to leave in, useful on self-hosted runners.
      - run: rstest -n auto --reuse-db --output github --junitxml junit.xml

      - uses: actions/upload-artifact@v4
        if: always()
        with: { name: junit, path: junit.xml }
```

**What the two runs look like.** Duration-aware scheduling needs one run of
timing data, so the first run on a fresh cache key is *cold* — the scheduler
has no per-test durations and falls back to an even split. The second run
(and every run after, as long as the cache restores) is *warm*: it starts the
slowest tests first and packs workers tightly.

Concretely, on the runnable
[`examples/ci-bench`](https://github.com/KovantAI/rstest/tree/main/examples/ci-bench)
suite (136 wait-bound tests with duration skew) — **measured**, `-n 4`, best of
3, Apple Silicon / CPython 3.13:

<!-- SOURCE OF TRUTH: examples/ci-bench/README.md — keep numbers in sync -->
| config | wall | vs pytest |
|---|---|---|
| pytest (serial) | 12.1s | 1.0× |
| rstest cold (`-n 4`, no cache) | 5.3s | 2.3× |
| rstest warm (`-n 4`, cached durations) | 3.6s | 3.3× |

Cold already wins from parallelism; warm adds ~1.5× on top by scheduling the
long pole first. That is a **synthetic** wait-bound example, not a Django app —
rstest ships no canonical Django timing, and a suite's win depends on its own
shape (see the self-check table in the
[README](https://github.com/KovantAI/rstest#will-rstest-speed-up-your-suite)).
To get *your* real numbers before committing, run [`rstest try`](migrate-from-pytest.md)
locally — it runs your suite under plain pytest and under `rstest -n auto`,
diffs outcomes, and reports the speedup, with no migration.

!!! warning "Ephemeral runners: warm the cache from your default branch"
    If PR jobs start from a cold cache every time, you only ever pay cold-run
    cost. Run this workflow on pushes to your default branch too (GitHub lets
    PR jobs restore the base branch's cache entries), so PRs restore a warm
    `.rstest_cache` instead of rebuilding timing data from scratch. For a
    matrix/shard layout, prefer the [shared-cache backend](ci-shared-cache.md)
    — it sidesteps the `run_id` key dance entirely.

## Worked example: monorepo on ephemeral CI

Two monorepo constraints collide in ephemeral CI, and the fix resolves both
at once:

1. **Concurrency.** At the root, every project launches concurrently with at
   least one worker each ([Monorepo mode](../concepts/monorepo.md#worker-budget-and-scheduling)).
   Many packages on a small (2–4 core) runner oversubscribes — 20 packages on
   a 2-core runner is 20 concurrent single-worker children fighting for 2 cores.
2. **Shared cache.** `--cache-remote`/`--cache-pull`/`--cache-push` are **not
   supported at a monorepo root** ([CLI](../reference/cli.md#-cache-remote-urldir--cache-pull--cache-push)) —
   each project keeps its own `.rstest_cache`, so the segment-merge shared
   cache is a per-project feature.

**Both dissolve if you make the project the unit of CI parallelism** — one
job per package via a matrix, instead of one root job running everything
concurrently. Each job runs a single project (`rstest libs/core` opts out of
monorepo mode and runs that package alone, with the runner's *full* core count
— no oversubscription), and because it's a single-project run it can use the
[shared cache](ci-shared-cache.md) normally:

```yaml
# .github/workflows/tests.yml
permissions: { contents: read, actions: read }
jobs:
  discover:
    runs-on: ubuntu-latest
    outputs: { projects: ${{ steps.list.outputs.projects }} }
    steps:
      - uses: actions/checkout@v4
      # Emit the matrix from your project layout. Keep this list in sync with
      # [tool.rstest] projects in the root pyproject.toml (single source of truth).
      - id: list
        run: |
          echo 'projects=["libs/core","libs/cli","services/api"]' >> "$GITHUB_OUTPUT"

  test:
    needs: discover
    runs-on: ubuntu-latest
    strategy:
      fail-fast: false
      matrix: { project: ${{ fromJSON(needs.discover.outputs.projects) }} }
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-python@v5
        with: { python-version: "3.13" }
      - run: pip install -r requirements.txt && pip install rstest==0.7.0

      # Warm this project's shared cache from the latest successful main run.
      - name: resolve warm-cache run
        id: warm
        env: { GH_TOKEN: ${{ github.token }} }
        run: |
          rid=$(gh run list --repo "$GITHUB_REPOSITORY" \
                  --workflow "${{ github.workflow }}" --branch main \
                  --status success --limit 1 \
                  --json databaseId --jq '.[0].databaseId // ""')
          echo "run-id=$rid" >> "$GITHUB_OUTPUT"
        continue-on-error: true
      # Warm segments land in ./rcache/segments/ (where rstest reads them);
      # upload-artifact strips that prefix on push, so aim the download at it.
      - uses: actions/download-artifact@v4
        if: steps.warm.outputs.run-id != ''
        with:
          pattern: "rstest-seg-${{ matrix.project }}-*"
          merge-multiple: true
          path: ./rcache/segments
          github-token: ${{ github.token }}
          run-id: ${{ steps.warm.outputs.run-id }}
        continue-on-error: true
      - run: ls ./rcache/segments/seg-*.json 2>/dev/null | xargs -rn1 basename | sort > .warm-segs || true

      # Run ONE project → full runner cores, no oversubscription, shared cache OK.
      # The junit slug keeps per-package files distinct across matrix legs.
      - name: test
        run: |
          slug=$(echo "${{ matrix.project }}" | tr '/' '-')
          rstest ${{ matrix.project }} -n auto --output github \
                 --cache-remote ./rcache --cache-pull --cache-push \
                 --junitxml "junit.${slug}.xml"

      # Upload only this run's new segment(s), not the warmed union.
      - run: |
          mkdir -p ./push
          for f in ./rcache/segments/seg-*.json; do
            [ -e "$f" ] || continue
            grep -qxF "$(basename "$f")" .warm-segs 2>/dev/null || cp "$f" ./push/
          done
        if: always()
      - uses: actions/upload-artifact@v4
        if: always()
        with:
          name: rstest-seg-${{ matrix.project }}-${{ github.run_id }}
          path: ./push/seg-*.json
          if-no-files-found: ignore
      - uses: actions/upload-artifact@v4
        if: always()
        with: { name: junit-${{ matrix.project }}, path: "junit.*.xml" }
```

Each package is its own job: it gets the whole runner, warms its own cache
segment from the last green main run, and pushes a fresh segment — cold on run
one, warm from run two, exactly like the single-suite case. Isolation is free
(matrix jobs don't share a runner), and a slow package no longer steals
workers from a fast one. The segment-merge mechanics are in
[Shared cache across CI jobs](ci-shared-cache.md).

!!! note "When to keep the root run instead"
    If your packages are **few** (roughly ≤ the runner's core count) the root
    `cd repo && rstest` from the [Monorepos guide](monorepo.md) is simpler:
    one job, one merged `--report-json`, per-project `junit.<slug>.xml` (glob
    `**/junit.*.xml`), and `.rstest_cache` persisted via `actions/cache` on
    `**/.rstest_cache`. It also keeps `--changed`'s cross-package skip logic in
    one place. Reach for the matrix above when project count outgrows the
    runner, or when you want the segment-merge shared cache per package. A
    project-level concurrency cap for the root case is on the roadmap
    ([Monorepo mode](../concepts/monorepo.md#worker-budget-and-scheduling)).

## Suite-health trending with doctor

`--doctor-json` writes the doctor analysis as a versioned JSON document
(see [Suite diagnostics](doctor.md)). Archive it per run and compare a
PR's report against the main branch's — no extra tooling required, the
document already contains totals, wait-bound tests, parallel-floor gate
tests, and fixture costs by name.

Any doctor run also publishes the report as markdown to the CI job
summary automatically — appended to `$GITHUB_STEP_SUMMARY` on GitHub
Actions, piped to `buildkite-agent annotate` on Buildkite — so the
current run's analysis is on the run page with no post-processing step.
(GitLab and TeamCity have no native markdown summary; use `--doctor-md`
and publish the file as an artifact.)

The baseline travels via the actions cache: pushes to main save it, PR
jobs restore it (GitHub lets PRs read the base branch's cache entries):

```yaml
      - name: test (with doctor)
        run: rstest -n auto --junitxml junit.xml --doctor-json doctor.json

      # Save the baseline on main; restore the latest one on PRs.
      - uses: actions/cache@v4
        with:
          path: doctor-baseline.json
          key: doctor-baseline-${{ github.sha }}
          restore-keys: doctor-baseline-

      - name: compare against main
        if: github.event_name == 'pull_request'
        run: |
          [ -f doctor-baseline.json ] || { echo "no baseline yet"; exit 0; }
          {
            echo "## Suite health vs main"
            jq -rn --slurpfile a doctor-baseline.json --slurpfile b doctor.json '
              def d(f): ($b[0][f] - $a[0][f]);
              "tests: \($a[0].tests) -> \($b[0].tests)",
              "test time: \($a[0].test_time_seconds|round)s -> \($b[0].test_time_seconds|round)s (\(d("test_time_seconds")|round)s)",
              "wait-bound: \($a[0].wait_bound.wait_pct // 0|round)% -> \($b[0].wait_bound.wait_pct // 0|round)%"
            '
            echo "new wait-bound tests:"
            comm -13 \
              <(jq -r '.wait_bound.tests[]?.nodeid' doctor-baseline.json | sort) \
              <(jq -r '.wait_bound.tests[]?.nodeid' doctor.json | sort) \
              | sed 's/^/- /' || true
          } >> "$GITHUB_STEP_SUMMARY"

      - name: refresh baseline
        if: github.ref == 'refs/heads/main'
        run: cp doctor.json doctor-baseline.json
```

Two practical notes:

- **Don't fail the job on timing deltas.** CI runners are noisy;
  single-digit-percent changes in `test_time_seconds` are jitter. Treat
  the summary as a review aid; alert only on structural signals (new
  wait-bound tests, a fixture's `count` doubling, a new parallel-floor
  gate test) or on large sustained moves.
- **Compare like with like.** `wall_seconds` depends on the worker
  count; if runner sizes vary, compare `test_time_seconds` (summed test
  time) and per-test signals instead.

## Gating new parallel-unsafe tests with migrate-check

[`migrate-check`](../reference/cli.md#migrate-check) exits non-zero when a
test has a run-to-run unstable id or fails only under parallelism, so a
dedicated job keeps a migrating suite from regressing — no new co-location
leak, order dependency, or unstable-id site sneaks in green. Use
`--migrate-allow` to tolerate a triaged backlog so the gate fires only on
**new** issues, and `--migrate-check-json` to archive the findings
([schema](../reference/report-json.md#migrate-check-json)):

```yaml
      - name: migrate-check gate
        run: |
          rstest --migrate-check-json migrate.json \
                 --migrate-allow tests/legacy/   # known-unsafe backlog, tolerated
      - uses: actions/upload-artifact@v4
        if: always()
        with:
          name: migrate-check
          path: migrate.json
```

This is heavier than a normal run (it collects twice and reruns the failing
files under discriminators), so run it on its own job or a schedule rather than
every push if the suite is large. Once the suite reports `ready`, drop the gate
and just run `rstest`.

## Notes

- **Exit codes** are pytest's (0 pass, 1 failures, 2 interrupted, 3
  internal, 4 usage error, 5 nothing collected) with sensible merging across workers —
  see [Exit codes](../reference/exit-codes.md).
- **`--junitxml`** is rendered by rstest from merged results; point your
  CI's test-report integration at it as you would pytest's.
- **`--report-json`** emits a per-test outcome snapshot (stable schema) if
  you build tooling on top of results.
- **`--output github`** keeps the normal log and additionally emits
  `::error` annotations for each failure, so failures appear inline on the
  PR diff — see [`--output`](../reference/cli.md#-output-dotsverbosebargithubjson).
- **Crash safety matters most in CI**: a segfaulting test costs one FAILED
  entry instead of an aborted job with partial results.
- **Worker count**: `-n auto` uses the runner's available logical cores —
  on Linux it honors the CPU affinity mask and cgroup CPU quota, so a
  CPU-limited container gets its allocation, not the host's core count. CI
  runners are small (2–4 cores) and not oversubscribed, so `auto` is the
  right default there; pin `-n <k>` only if you need a fixed count.
- **Colors** are disabled automatically when output is not a terminal;
  force with `--color=yes` if your CI renders ANSI.

## Go deeper

- [More CI systems](ci-recipes.md) — AWS CodeBuild, Google Cloud Build,
  GitLab, Azure, CircleCI, Jenkins, and pre-commit.
- [Shared cache across CI jobs](ci-shared-cache.md) — the segment-merge cache
  for a shard matrix.
- [Sharding across CI jobs](sharding.md) — how partitions are computed and the
  identical-cache-snapshot rule.
