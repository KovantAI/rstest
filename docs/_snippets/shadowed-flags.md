| Flag | Also defined by | What rstest does instead |
|---|---|---|
| `-n`, `--numprocesses`, `--dist` | pytest-xdist | runs its own worker pool ([`-n`](../reference/cli.md#-n-numprocesses-nauto), [`--dist`](../reference/cli.md#-dist-loadloadfileloadscopeloadgroupeach)); xdist stays inert |
| `--junitxml` / `--junit-xml` | pytest core | writes one merged JUnit file itself, at every worker count ([`--junitxml`](../reference/cli.md#-junitxml-path)) |
| `--html` | pytest-html | writes rstest's own merged HTML report, at every worker count ([`--html`](../reference/cli.md#-html-path)) |
| `--timeout` | pytest-timeout | rstest's native per-test timeout ([`--timeout`](../reference/cli.md#-timeout-secs)) |
| `--reruns`, `--only-rerun` | pytest-rerunfailures | rstest's native, crash-aware, orchestrator-side reruns ([`--reruns`](../reference/cli.md#-reruns-n)) |
| `--output` | pytest-playwright (`--output DIR`, its artifacts directory) | takes the value only when it is one of rstest's styles (`dots`, `verbose`, `bar`, `github`, `gitlab`, `buildkite`, `teamcity`, `azure`, `tap`, `json`) and renders that style ([`--output`](../reference/cli.md#-output-dotsverbosebargithubjson)). Any other value, such as `--output artifacts`, goes to the plugin unchanged at every worker count. A playwright directory named like a style needs `-- --output json` |
| `--debug` | pytest core (`--debug` trace log) | starts debugpy and waits for an editor to attach ([`--debug`](../reference/cli.md#-debugport)) |
| `-h`, `--help`, `-V`, `--version` | pytest core | prints rstest's own help or version. `rstest -- --help` does not show pytest's help either at `-n ≥ 2` or with an explicit `--output` (in byte-exact mode with no `--output` it prints the vendored pytest's help); for pytest's and your plugins' flags, run `python -m pytest --help` in the test environment (needs pytest installed there; its version may differ from the vendored core) |
