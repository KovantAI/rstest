# Plugins

## How loading works

rstest workers run a vendored pytest core, and plugins load against it
through the standard `pytest11` entry points — the same mechanism pytest
uses. Class identity holds: rstest depends on the real
[pluggy](https://github.com/pytest-dev/pluggy), and the vendored core's
classes live at their usual `_pytest.*` import paths, so plugins that
`isinstance`-check or import internals find what they expect. No plugin
re-installation, configuration, or porting.

Plugin command-line flags forward like any pytest flag; plugin ini options
are read normally.

## Exercised continuously

These load and pass per-test outcome parity against pytest baselines in
rstest's compatibility battery, on real suites:

- pytest-django (incl. per-worker test databases under parallelism)
- pytest-asyncio
- pytest-aiohttp
- hypothesis
- pytest-mock
- pytest-cov (see [Coverage](coverage.md))

## Special-cased

- **pytest-xdist**: neutralized inside workers (rstest owns parallelism);
  its hookspecs stay importable for plugins that implement them. Of the
  master-side hooks, `pytest_configure_node`, `pytest_testnodeready`,
  and `pytest_testnodedown` are emulated (called); the rest —
  `pytest_xdist_make_scheduler`, `pytest_xdist_auto_num_workers`,
  `pytest_handlecrashitem`, `pytest_xdist_node_collection_finished` —
  are silent no-ops: scheduling and crash handling are the Rust
  orchestrator's job and are not extensible from Python.
- **pytest-rerunfailures**: inside pool workers rstest unregisters the
  plugin (before `pytest_configure`, so its xdist `sock_port` client branch
  never fires) and handles reruns itself — crash-aware, honoring
  `@mark.flaky`. rstest's `--reruns` fire at every worker count, including
  `-n 0/1`. At `-n 0` with no `--reruns` the plugin keeps its own behavior.

## Tested compatibility

Beyond the battery above, these common ecosystem plugins were run under both
`-n 0` and the parallel pool and tiered by result. **Works** = correct in
parallel; **caveat** = works with a stated limitation; **parallel-unsafe** =
run it at `-n 0` (or use rstest's native equivalent).

| Plugin | Tier | Note |
|---|---|---|
| pytest-timeout | Works | per-test timeout fires in both modes — though rstest has a built-in [`--timeout`](../reference/cli.md#-timeout-secs) (+ `@pytest.mark.timeout`) that needs no plugin; don't run both (two SIGALRM handlers). See also `--worker-timeout` for C-extension deadlocks |
| pytest-env | Works | env vars set on every worker |
| pytest-socket | Works | `--disable-socket` blocks identically in parallel |
| pytest-repeat | Works | `@mark.repeat(N)` items distribute across workers |
| freezegun / pytest-freezer | Works | in-process time freezing is per-worker; **but** don't put `now()` in parametrize IDs (see [known gaps](../concepts/compatibility.md#known-gaps)) |
| pytest-benchmark | Caveat | auto-disables at `-n ≥ 2` (sees the pool as xdist); run benchmarks at `-n 0` and read numbers from `--benchmark-json` (the stats table isn't painted — rstest owns the terminal) |
| pytest-order | Caveat | ordering only holds within a worker at `-n ≥ 2`; use `-n 0`, or `--dist loadfile`/`loadscope` to keep an ordered group on one worker |
| pytest-randomly | Works | rstest synthesizes the `randomly_seed` key xdist's master would inject, derived from the run uid so every worker agrees on one reproducible seed. An explicit `--randomly-seed=<n>` still wins. (rstest's native [`--shuffle`](../reference/cli.md#-shuffleseed) remains available and is also parallel-safe.) |
| pytest-rerunfailures | Works | inside pool workers rstest unregisters it *before* `pytest_configure`, so its xdist `sock_port` client branch never fires (the old `KeyError: 'sock_port'` at `-n ≥ 2` with pytest-xdist installed), and rstest owns reruns natively — crash-aware, honoring `@mark.flaky` and [`--reruns`](../reference/cli.md#-reruns-n) / `--only-rerun`. At `-n 0` the plugin keeps its own behavior. |
| pytest-html | Parallel-unsafe | at `-n ≥ 2` **no report is written** — a silent no-op, not a crash. pytest-html registers its report writer only on a node *without* `workerinput` (its xdist "am I the master?" check); every rstest pool worker has a `workerinput`, so nothing ever owns report generation. Merging all workers' results into one file needs a single master process, which rstest doesn't run (the Rust orchestrator owns the merge, and workers are isolated sessions). Generate the report at `-n 0`/`-n 1`, or keep the parallel run and emit from merged artifacts — see [HTML & aggregated reporting](#html-aggregated-reporting-under-parallelism). |

The recurring fault line: plugins that read xdist-**master**-injected
`workerinput` keys used to crash under the pool when rstest had no master to
supply them. rstest now closes these per plugin: **derivable** keys are
synthesized worker-side (`randomly_seed` — one run-level value every worker
agrees on); **controller-service** keys are handled by each worker playing
master for itself — pytest-retry's branch self-provisions its own report
server per worker (so its `server_port` is set locally, no central
controller needed), and pytest-rerunfailures is unregistered before it can
read `sock_port` (rstest owns reruns instead). The one still-unsupported
case in this table is pytest-html (see its row). Where a plugin is risky,
rstest also ships a native equivalent: `--shuffle` (≈pytest-randomly),
`--reruns`/`--only-rerun` (≈pytest-rerunfailures), `--worker-timeout`
(complements pytest-timeout).

## HTML & aggregated reporting under parallelism

pytest-html is the one report plugin that goes dark at `-n ≥ 2` (silent
no-op — see its row above). You do **not** have to choose between parallel
speed and a contributor-facing report: run the suite once in parallel, then
emit the report from the merged artifacts rstest writes. Nothing re-runs.

The reason this works: `--junitxml` and `--report-json` are **intercepted by
rstest and rendered from merged results**, not forwarded to per-worker
sessions (which would clobber a shared file). Both are whole-suite documents
at any worker count, `-n 0` through `-n auto`.

### JUnit XML — for CI dashboards (native, parallel)

Most CI report surfaces (GitHub Actions test summaries, GitLab, Jenkins,
Buildkite) consume JUnit XML. `--junitxml` gives you one merged file
straight from the parallel run:

```console
$ rstest -n auto --junitxml results.xml
```

One file, all workers merged, pytest classname conventions. Flaky passes
(green after `--reruns`) carry `<property name="flaky" value="true"/>` so
dashboards track them without parsing anything else. This is the preferred
path when your goal is CI-visible results, not a standalone HTML page.

### HTML — from `--report-json` (parallel run, no rerun)

When you specifically want an HTML artifact for contributors, generate it
from the parallel run's [`--report-json`](../reference/report-json.md)
snapshot — the full merged per-test outcome document:

```console
$ rstest -n auto --report-json results.json      # fast parallel run
$ python render_report.py results.json report.html   # cheap, no tests run
```

The report generator is a few lines over the stable schema — the
`meta.counts` block is the terminal summary line verbatim (never re-derive
it by walking `tests`), and each test carries its phase outcomes,
`duration`, and `longrepr` on failure:

```python
# render_report.py
import html, json, sys

doc = json.load(open(sys.argv[1]))
c = doc["meta"]["counts"]
rows = []
for nodeid, t in sorted(doc["tests"].items()):
    outcome = t.get("call", t.get("setup", "skipped"))  # skipped tests have no call phase
    detail = html.escape(t.get("longrepr", t.get("skip_reason", "")))[:2000]
    rows.append(
        f"<tr class={outcome!r}><td>{html.escape(nodeid)}</td>"
        f"<td>{outcome}</td><td>{t.get('duration', 0):.4f}s</td>"
        f"<td><pre>{detail}</pre></td></tr>"
    )
open(sys.argv[2], "w").write(
    f"<h1>{c['passed']} passed, {c['failed']} failed, {c['skipped']} skipped "
    f"({doc['meta']['duration_seconds']:.1f}s, {doc['meta']['workers']} workers)</h1>"
    "<table><tr><th>test</th><th>outcome</th><th>time</th><th>detail</th></tr>"
    + "".join(rows)
    + "</table>"
)
```

This keeps the parallel wall-time win — the only extra cost is parsing a
JSON file. Style it however your contributors expect; the schema is
versioned (`meta.schema`) and increment-only, so the script won't silently
break on upgrade.

### pytest-html's exact format — `-n 0` reporting pass

If a workflow depends on pytest-html's *specific* HTML output and nothing
else will do, run a dedicated single-session pass for the report only:

```console
$ rstest -n auto                    # gate on this — the fast run
$ rstest -n 0 --html report.html    # report only; single session, no workerinput
```

This re-runs the suite serially, so use it only when the exact pytest-html
layout is a hard requirement — the JUnit or `--report-json` paths above
avoid the second run entirely.

## Checking an unlisted plugin

The lists above aren't exhaustive — your suite likely runs plugins not named
here. Probe one yourself in a minute:

1. Run a slice of your suite at **`-n 0`** — establishes the plugin works at
   all under rstest's vendored core.
2. Run the same slice at **`-n 2`**. If it now crashes (commonly a
   `KeyError` on a `workerinput` key at `pytest_configure`), the plugin
   depends on an xdist-master-injected value rstest doesn't set — run it at
   `-n 0` or find a native equivalent.
3. If it runs but results look wrong (ordering, benchmark timings, a report
   file not written), it's likely order- or terminal-sensitive — see the
   caveat tiers above for the pattern.

`rstest try` is a fast first pass: it runs your suite under pytest and
under `rstest -n auto` and flags outcome differences, plugins included.

## Hook coverage

rstest runs a real [pluggy](https://github.com/pytest-dev/pluggy) inside each
worker, so **every conftest/plugin hook runs per-worker** exactly as in pytest.
The exceptions are hooks whose result depends on there being a single
coordinating process — because rstest's coordinator is the Rust orchestrator,
not a Python master. This table is the precise contract at `-n ≥ 2`:

| Hook | Behavior at `-n ≥ 2` | Why |
|---|---|---|
| `pytest_configure` / `pytest_unconfigure` | Runs per worker | Standard per-session hook |
| `pytest_collection_modifyitems` | Deselection honored; **reordering ignored** | Dispatch is index-into-verified-collection, duration-first — use `-n 0` or an affinity [`--dist`](../reference/cli.md) mode to preserve order ([xdist hooks](../concepts/xdist-hooks.md)) |
| `pytest_terminal_summary` and other terminal-painting hooks | Runs per worker, but **custom terminal output is not shown** — rstest owns the terminal | The orchestrator renders one merged terminal; use `-n 0` when you want a plugin's own rendering |
| `pytest_runtest_protocol` (custom replacements) | Runs, but interplay with dispatch only exercised for the plugins listed above | Dispatch owns `nextitem`/phase streaming — report surprises |
| `pytest_configure_node` | Emulated (called) per worker | xdist master-side hook — [xdist hooks](../concepts/xdist-hooks.md) |
| `pytest_testnodeready` / `pytest_testnodedown` | Emulated; `testnodedown` for a crashed worker runs on a *survivor* | Best-effort, weaker than xdist |
| `pytest_xdist_make_scheduler` | Silent no-op | Scheduling lives in Rust, not extensible from Python |
| `pytest_xdist_auto_num_workers` | Silent no-op | Worker count is rstest's decision |
| `pytest_handlecrashitem` | Silent no-op | Crash handling lives in Rust |
| `pytest_xdist_node_collection_finished` | Silent no-op | No master collection phase |

### The silent-no-op class

One failure mode is worth generalizing, because it hits a whole *category* of
plugins, not just the pytest-html row that documents it above. A plugin that
decides "am I the xdist master?" with `not hasattr(config, "workerinput")` will
**silently do nothing** under the rstest pool — every rstest worker carries a
`workerinput`, so the master-only branch never fires, and nothing crashes to
tell you.

This is why **pytest-html** writes no report at `-n ≥ 2`, and why
report-aggregator plugins in general — anything that merges all workers'
results into one artifact from a central process — go dark under parallelism.

Rule of thumb: if a plugin's job is to *aggregate across workers from the
master*, assume it needs `-n 0` until proven otherwise. rstest ships native,
merged-from-the-orchestrator equivalents for the common ones — `--junitxml`,
`--report-json`, `--cov`, native `--html` — which are whole-suite documents at
any worker count.

### Self-audit: catch a silent no-op in your own plugins

The danger of this class is that nothing errors — a home-grown reporter or
aggregator just stops producing its artifact at `-n ≥ 2`. You can flush it
out mechanically: run the **same slice** at `-n 0` and at `-n 2`, each into
its own empty directory, then diff the *files each run produced*. Anything a
plugin writes at `-n 0` but not at `-n 2` (or writes empty) is a silent
no-op.

```bash
#!/usr/bin/env bash
# silent-noop-audit.sh — flag plugin artifacts that vanish under the pool.
# Usage: ./silent-noop-audit.sh tests/some_slice
set -euo pipefail
slice="${1:-tests}"

audit() {                      # $1 = worker count, $2 = output dir
  rm -rf "$2"; mkdir -p "$2"
  # -p no:cacheprovider keeps rstest's own .rstest_cache / .pytest_cache
  # out of the diff; add your plugin's output flags here if it needs one
  # (e.g. --html "$2/report.html").
  ( cd "$2" && rstest -n "$1" -p no:cacheprovider "$OLDPWD/$slice" >stdout.log 2>&1 ) || true
  ( cd "$2" && find . -type f ! -empty | sort ) >"$2.files"
}

audit 0 audit-n0
audit 2 audit-n2

echo "== files present at -n 0 but MISSING or EMPTY at -n 2 (silent no-op suspects) =="
comm -23 <(sed 's#^audit-n0/##' audit-n0.files) <(sed 's#^audit-n2/##' audit-n2.files)

echo "== plugin lines in -n 0 stdout absent from -n 2 stdout (terminal-owned output) =="
diff <(grep -v '^$' audit-n0/stdout.log) <(grep -v '^$' audit-n2/stdout.log) | grep '^<' || true
```

Read the two lists:

- **A file in the first list** — a plugin wrote it serially and not under the
  pool. That's the silent no-op (pytest-html's `report.html` shows up here).
  Move that report to a `-n 0`/`-n 1` pass, or switch to a native
  merged-from-orchestrator artifact (`--junitxml`, `--report-json`).
- **Lines in the second list** are usually just terminal-painting plugins
  (rstest owns the terminal) — expected, not a bug — *unless* a line
  represents a data side effect the plugin only performs on the master. If
  so, treat it like the first list.

One caveat the diff can't see: a plugin that writes the *same path* at both
worker counts but with **less** in it at `-n 2` (e.g. only one worker's
share). For those, compare sizes or contents of the shared artifact, not
just its presence. When in doubt, the honest fallback is unchanged:
generate that artifact at `-n 0`.

## Known limits

- Plugins that *render the terminal* (pytest-sugar, pytest-rich and
  similar progress UIs) don't paint at `-n ≥ 2` — rstest owns the
  terminal. Their non-visual behavior is unaffected; use `-n 0` when you
  specifically want their rendering.
- Plugins registering custom `pytest_runtest_protocol` replacements run,
  but interplay with rstest's item dispatch is only exercised for the
  plugins listed above. Report surprises.
