# CI quickstart

rstest behaves like pytest in CI: exit code discipline, JUnit XML for
your test-report integration, `--report-json` for tooling — and quiet
human output (machine consumers should parse the files, not stdout). It adds two things worth wiring up: worker
parallelism with no extra plugin, and a duration cache that makes
scheduling smarter when persisted between runs.

!!! warning "Pre-release"
    rstest is not yet on PyPI; in CI, install from a wheel you host (or
    build with maturin in a setup job). The recipe below shows the
    intended shape once published — substitute your wheel URL for
    `pip install rstest` until then.

## GitHub Actions

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
        run: rstest -n auto --junitxml junit.xml

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

[`--migrate-check`](../reference/cli.md#-migrate-check) exits non-zero when a
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
- **Worker count**: `-n auto` uses the runner's logical cores. CI runners
  are small (2–4 cores) and not oversubscribed, so `auto` is the right
  default there.
- **Colors** are disabled automatically when output is not a terminal;
  force with `--color=yes` if your CI renders ANSI.
