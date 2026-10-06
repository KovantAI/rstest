# CI output formats

Machine-readable `--output` styles that turn a run's failures into the native
annotations, log sections or streams of a CI system or test harness. Each one
is selected with [`--output STYLE`](../reference/cli.md#output) or
`[tool.rstest] output = "STYLE"`; the human styles (`dots`, `verbose`, `bar`)
are described with the flag. Whatever the style, the process exit code
follows [Exit codes](../reference/exit-codes.md).

## GitHub Actions (`github`)

Prints the `dots` log plus a [GitHub Actions](https://docs.github.com/actions)
`::error` workflow command per failing test, shown as inline annotations on
the PR diff:

```text
::error file=<path>,title=<nodeid>,line=<n>::<traceback>
```

- `file` is repo-relative: when the pytest rootdir sits below the repo root (a
  `working-directory:` step, a monorepo project), its path from the repo root
  is prepended (`proj/tests/test_a.py`). The repo root is `$GITHUB_WORKSPACE`
  (or Azure's `$BUILD_SOURCESDIRECTORY`) when it contains the rootdir, else the
  nearest ancestor with a `.git`; with neither, the path stays
  rootdir-relative.
- `line` is 1-based (`lineno + 1`, where `lineno` is the 0-based value in the
  JSON reports), omitted when no location is available. The traceback is
  escaped per the workflow-command spec.
- A collection error gets `::error file=<path>,title=<path> (collection
  error)::<traceback>`; the run still exits `2`.
- A test that passed only after reruns (`--reruns` / `@pytest.mark.flaky`)
  emits `::warning` (`flaky: passed only after N reruns`), and a
  [quarantined](../reference/cli.md#-quarantine-file) failure emits `::warning`
  (`quarantined (non-fatal): <traceback>`) instead of `::error`.

## Azure Pipelines (`azure`)

Prints the `dots` log plus an [Azure Pipelines logging
command](https://learn.microsoft.com/azure/devops/pipelines/scripts/logging-commands)
per failing test, shown as an inline issue on the file in the PR:

```text
##vso[task.logissue type=error;sourcepath=<path>;linenumber=<n>]<nodeid>: <message>
```

`sourcepath` and `linenumber` follow the `github` rules for `file` and `line`.
The message is the exception line (`AssertionError: x mismatch`), the first
line of the traceback's last `E` block, since logissue is single-line. A
collection error also emits `type=error` (`<path> (collection error):
<exception>`); flaky passes and quarantined failures emit `type=warning`,
never `type=error`.

## GitLab CI (`gitlab`)

Prints the `dots` log, with each failure in the end-of-run block wrapped in
a [GitLab CI collapsible
section](https://docs.gitlab.com/ci/jobs/job_logs/#custom-collapsible-sections)
(collapsed by default). GitLab has no per-line warning command, so the
flaky-tests block folds into its own collapsed section.

## Buildkite (`buildkite`)

Prints the `dots` log, with each failure under an auto-expanded [`+++`
group header](https://buildkite.com/docs/pipelines/configure/managing-log-output).
Flaky tests are published as a `warning`
[annotation](https://buildkite.com/docs/agent/v3/cli-annotate) on the build
page (best-effort via `buildkite-agent`).

## TeamCity (`teamcity`)

Emits [TeamCity service
messages](https://www.jetbrains.com/help/teamcity/service-messages.html) as
each test finishes: `testStarted`/`testFinished` per test, plus `testFailed`
(escaped traceback as `details`) or `testIgnored` for skips and xfails,
grouped per test so parallel results never interleave. A collection error is
a failed test named after the broken module (`message='collection error'`,
traceback as `details`). Flaky tests emit a `WARNING`-status build message.
The banner and summary stay: TeamCity ignores non-service lines.

## TAP (`tap`)

Stdout is a pure [Test Anything Protocol](https://testanything.org)
version 13 stream: `ok N - nodeid` / `not ok N - nodeid` per test as it
finishes, failure text as `#` lines, skips as `# SKIP <reason>`, xfail/xpass as
`# TODO`, closed by the `1..N` plan. A collection error is a
`not ok N - <path> # collection error` point (traceback as `#` lines) counted
in the plan, so a suite that can't import never reads as an empty green
`1..0`. No banner or human summary. For TAP harnesses (`prove`, the Jenkins
TAP plugin).

## Newline-delimited JSON (`json`)

Stdout is a pure **newline-delimited JSON** stream: one `testreport`
object per phase as each test finishes, closed by a `sessionfinish` envelope,
with no banner, footer or summary. For editors and live tooling; event shapes
in [Streaming JSON](../reference/report-json.md#streaming-json). Unlike
[`--report-json`](../reference/cli.md#-report-json-path), which writes one end-of-run snapshot
file.
