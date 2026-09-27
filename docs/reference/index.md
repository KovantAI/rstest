# Reference

Exact behavior of flags, files, and outputs.

- [CLI flags](cli.md): every rstest flag, grouped by topic, and the rule for forwarding pytest flags
- [CLI subcommands](cli-commands.md): `try`, `migrate-check`, `audit`, `bisect`, `shard-verify`, `cache-compact`, `explain`, `verify-vendor`
- [Markers](markers.md): `serial`, `flaky`, `xdist_group`
- [Environment variables](environment.md): the `RSTEST_*` contract
- [Exit codes](exit-codes.md): what each code means and which flags gate on it
- [Report JSON](report-json.md): walkthrough and examples for `--report-json`, discovery, streaming, doctor, and migrate-check output
- [Output schemas](output-schemas.md): field reference generated from the Rust types
- [Benchmarks](benchmarks.md): suite timings against pytest and pytest-xdist
- [Parity divergences & upstream fixes](parity-divergences.md): every reason a public suite isn't byte-exact, and the upstream change that removes it
- [Plugin compatibility (top 100)](top-100-plugins.md): how the 100 most-downloaded pytest plugins behave under the pool, with a verified/inferred column
- [Plugins exercised by the corpus](corpus-plugins.md): the plugins real corpus suites actually load and pass under rstest
- [xdist support matrix](xdist-support.md): every pytest-xdist flag and hook, and what rstest does with it
- [Troubleshooting](troubleshooting.md): first-run errors and common fixes
- [Security & supply chain](security.md): vulnerability reporting, release integrity, the vendored pytest, what rstest runs and when it uses the network
- [License](license.md)
