# Local dev / inner loop

You have a small, fast suite and you run it constantly while you code. On a
suite that finishes in under ~10 seconds, adding workers barely moves the wall
clock; the wins that matter are *rerunning less* and *rerunning
automatically*. This page is a playbook: pick the row that matches what you
are doing, then follow the link for the full behaviour.

## Which flag when

| You want to... | Run | Full guide |
|---|---|---|
| rerun automatically on every save | `rstest --watch` | [Watch mode](watch-mode.md) |
| rerun only what your edits touch, by hand | `rstest --changed` | [Selecting changed tests](changed.md) |
| iterate on the failures of the last red run | `rstest --lf` (or `--ff`) | [CLI reference](../reference/cli.md) |
| get a `(Pdb)` prompt or attach an editor | `-n 0`, `-s`, `--pdb`, `--debug` | [Debugging a test](debugging.md) |
| reproduce a CI-only parallel failure | `rstest replay` with the CI job's journal | [Replaying a CI failure](replay.md) |
| tolerate or track an intermittent failure | `--reruns N`, `--quarantine` | [Flaky tests](flaky-tests.md) |
| find leaks and expensive fixtures | `rstest --doctor` | [Suite diagnostics](doctor.md) |

## Notes per row

**`--watch`.** Runs the suite once, then reruns on every save of a `.py` or
pytest config file. A test-file change reruns that file; a source change
reruns the tests the import graph says are affected. Every cycle spawns fresh
workers, so an edited module is always re-imported. Flags compose and apply to
every rerun (`rstest --watch -x -k login`), and reruns default to fail-fast
ordering. A small suite with a warm cache often reruns on one worker; pass
`-n 2` to parallelize like CI. Rerun policy, quitting, exit codes and
per-cycle cost: [Watch mode](watch-mode.md).

**`--changed`.** Diffs the working tree and untracked files against `HEAD`
and runs only the affected tests:

--8<-- "docs/_snippets/changed-engines.md"

Out of the box you get the import graph, which over-selects rather than
under-selects. Warm the coverage index once (`rstest --cov=src
--cov-context=test`) for line-level selection. Selection rules, drift, and
[`--changed-strict`](../reference/cli.md#-changed-strict):
[Selecting changed tests](changed.md).

**`--lf`.** `--lf`/`--ff` are forwarded to pytest, but rstest writes the
last-failed cache from **merged results** across workers, so a follow-up
`--lf` behaves exactly as after a serial run. Under `--watch` the last-failed
state refreshes every cycle. `--lf` still reruns
[quarantined](flaky-tests.md) failures: locally they behave like the failures
they are.

**Debugging.** Pool workers have no terminal, so a `breakpoint()` hit during
a parallel run fails that test with a hint instead of hanging. Rerun it with
`-n 0` or `-s` for the `(Pdb)` prompt, or with `--debug` to attach VS Code.
Both editors' configs and the `--reruns` gotcha:
[Debugging a test](debugging.md).

**Parallel-only failures.** Locally a fast suite often runs in
[single-worker mode](../concepts/glossary.md#single-worker-mode), so a failure
that only shows up in CI's parallel run won't reproduce until you pass
`-n 2` or more. To re-run CI's exact schedule, download the failing job's
journal and [replay it](replay.md). To find the test that pollutes it, follow
[Diagnosing a parallel-only failure](parallel-safety.md#diagnosing-a-parallel-only-failure).

**Flaky tests.** `--reruns N` retries a failure and reports a test that then
passes as `flaky`; every run records flakes in `.rstest_cache/flakes.json`;
`--quarantine` ring-fences known offenders while they get fixed. The full
lifecycle: [Flaky tests](flaky-tests.md).

**`--doctor` on a fast suite.** Skip the "where does the time go" sections and
read two others. `RESOURCE LEAKS` names tests that leave threads or fds open
after teardown, the usual source of order-dependent flakes (details:
[Resource leaks](resource-leaks.md)). `FIXTURE HOTSPOTS` ranks setup time per
fixture: a function-scoped fixture that costs real time on every test is a
candidate for a wider scope, and it taxes every inner-loop rerun. Doctor works
at any worker count and doesn't change outcomes. See
[Suite diagnostics](doctor.md).
