# Replaying a CI failure locally

A test that fails on CI but passes on your machine is usually an ordering
problem: on CI it shared a worker with a test that leaked state, and locally
the scheduler put them apart. Every parallel run (`-n >= 2`) records which
worker ran which tests, in what order, to `.rstest_cache/replay/latest.json`.
Keep that file when a job fails and [`rstest replay`](../reference/cli-commands.md#replay)
re-runs the same schedule on your machine.

The workflow has three steps: upload the journal from the failing CI job,
download it next to a checkout of the failing commit, and replay it.

!!! warning "Sharded jobs record no journal"
    A run with `--shard` (or the action's `shard`/`shard-total` inputs) writes
    no journal, and neither do `-n 0`/`-n 1` and `--dist each`. To replay a
    failure from a sharded matrix, re-run the failing shard's tests unsharded
    with `-n` set to the CI worker count; that run records a journal you can
    replay.

## 1. Upload the journal when the tests fail

With the bundled action, give it an `id` and add one step after it:

```yaml
      - uses: KovantAI/rstest/.github/actions/rstest@v0.9.0
        id: rstest
        with:
          python-version: "3.13"
          args: "-n auto"
      - uses: actions/upload-artifact@v7
        if: always() && steps.rstest.outputs.exit-code != '0'
        with:
          name: rstest-replay-${{ github.job }}-${{ strategy.job-index }}
          path: .rstest_cache/replay/latest.json
          include-hidden-files: true
          if-no-files-found: ignore
```

The condition reads the action's `exit-code` output, not `failure()`. With
`fail-under-ratio` set, the action's step can pass while tests failed, and
`failure()` would then skip the upload. `if-no-files-found: ignore` covers a
run that recorded nothing (for example `-n auto` resolving to one worker).
`include-hidden-files: true` is required: `upload-artifact` (v4.4 and later)
skips anything under a dot-directory such as `.rstest_cache`, even a path
named in full, and without it the artifact would be empty.

The job name and matrix index in the artifact name keep the uploads apart
when a matrix (Python versions, OSes) fails in several jobs at once:
`upload-artifact` (v4 and later) refuses a second artifact with the same name
in a run. Outside a matrix, `strategy.job-index` is `0`.

With raw YAML, put the same `upload-artifact` step after your `rstest -n auto`
step, with `if: failure()`. On other CI systems, save
`.rstest_cache/replay/latest.json` as a failure artifact the same way you
save `junit.xml`.

### Where the journal lands

- **With a `working-directory`**, prefix the path with it
  (`libs/core/.rstest_cache/replay/latest.json`).
- **At a monorepo root**, each project records its own journal inside the
  project directory. Upload them all with a glob and keep the directory
  layout, so you know which project each came from. Keep
  `include-hidden-files: true` here too:

    ```yaml
          path: "**/.rstest_cache/replay/latest.json"
          include-hidden-files: true
    ```

    Replay from inside that project's directory, not from the root.

- **With `RSTEST_CACHE` set**, journals move with the cache:
  `$RSTEST_CACHE/replay/latest.json` for a single project, or
  `$RSTEST_CACHE/<slug>/replay/latest.json` per project at a monorepo root.

## 2. Check out the failing commit and download the journal

```console
$ git checkout <failing-sha>
$ pip install -r requirements.txt          # same deps as CI (use your lockfile)
$ gh run download <run-id> -n rstest-replay-tests-0 -D ci-replay   # job "tests", matrix index 0
```

Run `gh run download <run-id>` with no `-n` to fetch every artifact of the run
if you're unsure of the name.

## 3. Replay it

```console
$ rstest replay --journal ci-replay/latest.json
rstest: replay: run 18d93d9429580fb015f7d (0.9.0 recorded), 4 worker(s), 22 test(s) across 4 slot(s)
rstest: replay: args: tests -k 'not slow'
...
--- FAILED [gw0] tests/test_m2.py::test_victim ---
```

Check the `args:` line before the tests start. It shows the pytest
arguments the CI run was given on the command line (paths, `-k`, `-p` and
so on), `(none)` if there were none. rstest's own flags such as `-n` are
not part of it, and neither are `addopts` or `PYTEST_ADDOPTS`, which
replay reads from your checkout. Replay hands the recorded arguments to
pytest as they are, so only replay journals from runs you trust
(see [Security: replay journals](../reference/security.md#replay-journals)).
To pick the interpreter, pass `--python`:
`rstest replay --journal ci-replay/latest.json --python .venv/bin/python`.

Replay forces the recorded worker count (even on a laptop with fewer cores),
runs each worker's recorded tests in the recorded order, and turns off reruns
(`@pytest.mark.flaky` and `[tool.rstest] reruns` included), work stealing and
shuffling. `@pytest.mark.serial` tests still run alone, after every other
worker has finished, as they did on CI. A worker that crashes is replaced and
the replacement picks up where it died. The test args (paths, `-k`, `-m`) come
from the journal, so pass none. Recorded `--lf`/`--sw` are ignored with a note:
they would select by your local pytest cache, not CI's.

The failing test's `[gwN]` tag tells you which worker to look at: the tests
that ran before it on that worker hold the likely polluter. To narrow them
down to the exact polluting test, run
[`rstest bisect <nodeid>`](../reference/cli-commands.md#bisect-nodeid). Once
you have a fix, run the same replay command again. Green means the fix holds
for the schedule that broke CI.

## Keeping a journal portable

- **Run from the same directory as CI.** Nodeids are relative to the rootdir,
  so in a monorepo replay from the package directory the CI job ran in.
- **Keep test paths inside the project.** Absolute paths under the directory
  rstest ran in (`$GITHUB_WORKSPACE/tests`) are stored relative to it, so they
  resolve on your checkout. A path outside it is stored as given; replay
  warns when such a path doesn't exist on your machine.
- **Match the code and dependencies.** If the suite changed since the
  recording, replay says so, runs the tests that still match, and reports how
  many recorded tests no longer collect. The reproduction may then be lost.
- **Only parallel runs record.** `-n 0`/`-n 1`, `--dist each` and
  `--shard` write no journal. A `--collect lazy` run records too, and replay
  re-runs its schedule with full collection.
- **Keep journals out of the CI cache.** If you cache `.rstest_cache`
  yourself, exclude `.rstest_cache/replay`; see
  [Keep replay journals out of the cache](ci-shared-cache.md#keep-replay-journals-out-of-the-cache).

## What replay does not reproduce

Replay reproduces what each worker ran and in what order, which is what
state-leak and ordering failures depend on. How the workers' timing lines up
with each other is not reproduced, so a true timing race (two workers touching
the same file or port at the same moment) may need several replays or may
not show up at all.

For an order-dependent failure you found with `--shuffle` rather than on CI,
the run prints its seed: rerun with `--shuffle=SEED` to get the same order
back. For the wider triage workflow (known-flaky tests, quarantine, reruns),
see [Flaky tests](flaky-tests.md).
