| Flag | Also defined by | What rstest does instead |
|---|---|---|
| `-n`, `--numprocesses`, `--dist` | pytest-xdist | runs its own worker pool ([`-n`](../reference/cli.md#-n-numprocesses-nauto), [`--dist`](../reference/cli.md#-dist-loadloadfileloadscopeloadgroupeach)); xdist stays inert |
| `--junitxml` / `--junit-xml` | pytest core | writes one merged JUnit file itself, at every worker count ([`--junitxml`](../reference/cli.md#-junitxml-path)) |
| `--html` | pytest-html | writes rstest's own merged HTML report, at every worker count ([`--html`](../reference/cli.md#-html-path)) |
| `--timeout` | pytest-timeout | rstest's native per-test timeout ([`--timeout`](../reference/cli.md#-timeout-secs)) |
| `--reruns`, `--only-rerun` | pytest-rerunfailures | rstest's native, crash-aware, orchestrator-side reruns ([`--reruns`](../reference/cli.md#-reruns-n)) |
| `--output` | pytest-playwright (`--output DIR`, its artifacts directory) | renders the style when the value is one of rstest's ([`--output`](../reference/cli.md#output)); any other value, such as `--output artifacts`, goes to the plugin unchanged |
| `--quarantine` | pytest-quarantine (marks the listed tests `xfail`) | reads the file as a non-fatal failure list: listed tests still run and report, but their failures don't fail the run ([`--quarantine`](../reference/cli.md#-quarantine-file)) |
| `--debug` | pytest core (`--debug` trace log) | starts debugpy and waits for an editor to attach ([`--debug`](../reference/cli.md#-debugport)) |
| `-h`, `--help`, `-V`, `--version` | pytest core | prints rstest's own help or version (see the note below for pytest's) |

A playwright artifacts directory named like an rstest style (`json`, `tap`, ...)
needs `-- --output json` to reach the plugin. For pytest's and your plugins'
flags, run `python -m pytest --help` in the test environment (it needs pytest
installed there, and its version may differ from the vendored core);
`rstest -- --help` prints the vendored pytest's help only in single-worker mode
with no `--output`.
