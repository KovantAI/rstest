# Migrating from pytest

The short version: install rstest, replace `pytest` with `rstest` in your
command, done. This page is the long version — what is identical, what
differs, and how to fall back.

## What stays identical

**Your test code.** Nothing changes: fixtures, conftest hierarchies,
parametrize, marks, `pytest.raises`, assertion introspection — all of it
runs through a vendored pytest core (currently pytest 9.1.1), not a
reimplementation.

**Your plugins.** Plugins installed in the environment load through the
normal `pytest11` entry points against the vendored core. pytest-django,
pytest-asyncio, pytest-aiohttp, pytest-mock, and hypothesis are exercised
against real suites in rstest's compatibility battery. Plugin flags forward
like any other pytest flag.

**Your configuration.** `pyproject.toml [tool.pytest.ini_options]`,
`pytest.ini`, `setup.cfg`, `tox.ini` — including `addopts`, `testpaths`,
`python_files`, `markers`, `filterwarnings` — are read by the vendored core
exactly as pytest reads them.

**Your flags.** rstest keeps a handful of its own flags (`-n`, `--dist`,
`--doctor`, `--watch`, `--junitxml`, `--report-json`, `--python`) and
forwards everything else to the test session verbatim. `-k`, `-m`, `-x`,
`--maxfail`, `--lf`, `--ff`, `-W`, `-p`, `--tb`, plugin options — no
translation table needed.

## What changes

**Parallel by default.** This is the headline difference. pytest runs your
tests one at a time; rstest runs them on `auto` workers and says so in its
header line. The implications:

- *Session/module-scoped fixtures instantiate once per worker*, not once
  per run — the same semantics as pytest-xdist. A session-scoped database
  or server fixture must tolerate N concurrent instances. (`rstest
  --doctor` flags session fixtures that ran more than once.)
- *Tests run in a different order*, interleaved across workers. Tests that
  depend on a previous test's side effects need [`--dist
  loadfile`](parallel-safety.md#file-affinity) or a fix.
- *Output interleaves across workers* under `-v`, in completion order.

**Test output rendering.** rstest renders progress, failures, and summaries
itself (from the same data pytest would use). Failure tracebacks, captured
sections, warnings summaries, and counts match pytest's content, but
plugins that *draw on the terminal* (pytest-sugar, pytest-rich) won't paint
their UI — rstest owns the terminal.

**Some files land elsewhere.** Each worker gets a disjoint `tmp_path` root
(like xdist's `popen-gwN`). `--junitxml` is written by rstest itself from
merged results, since per-worker sessions would clobber a shared file. The
`--lf` cache is likewise written merged.

**Pinned to an older pytest?** Price that step separately. rstest's
vendored core is pytest 9; if your suite and plugin set are pinned to
pytest 8 (or older), adopting rstest *includes* a pytest-9 migration —
deprecation warnings, plugin version bumps, the usual. `rstest -n 0` is
the cheap probe: it surfaces exactly what a pytest upgrade would, with
your installed pytest untouched. Budget the runner switch as
"pytest upgrade first, then a one-line command change," not one step.

## The escape hatch

```console
$ rstest -n 0
```

One worker, one pytest session, byte-exact pytest behavior. If something
behaves differently under rstest's parallel mode, this is the first
diagnostic: if it also fails at `-n 0`, it's not parallelism.

Flags that need pytest's own terminal switch to this mode automatically:
`--co`, `-s` / `--capture=no`, `--pdb`, `--trace`.

## If your suite already uses pytest-xdist

rstest neutralizes xdist inside its workers automatically — an `addopts =
-n 4` in your ini will not spawn nested workers. Remove the `-n` from
`addopts` when convenient and pass it to rstest instead. See
[Migrating from pytest-xdist](migrate-from-xdist.md).

## Just want to know if it's worth it?

```console
$ rstest --try
```

runs your suite under plain pytest and under `rstest -n auto` and tells you, in
one shot, whether the results are identical and how much faster rstest is — the
30-second answer before you commit to anything. If it flags differences, it
points you at `--migrate-check` (below).

## A migration checklist

0. `rstest --migrate-check` — **the preflight that does the triage for you.**
   It is the front door of the migration: run it first, fix what it names,
   and steps 2–3 below usually become a formality. See
   [The migrate-check preflight](#the-migrate-check-preflight) just below for
   what it reports.
1. `rstest -n 0` — confirm identical results to pytest (this is the
   contract; report a bug if not).
2. `rstest` — run parallel. Green? You're done.
3. A few tests fail only in parallel? `--migrate-check` already classified
   each one and named its fix; [Parallel safety](parallel-safety.md) is the
   reference for the remedies (`@pytest.mark.serial`, `--dist loadfile`, or
   fixing the shared state).
4. Run `rstest --doctor` once — it usually pays for the migration by
   itself.

## Driving it with Claude (the migrate-to-rstest skill)

rstest ships a Claude Code skill that runs this whole checklist for you:
`.claude/skills/migrate-to-rstest/`. Open the rstest repo (or copy that
directory into your own project's `.claude/skills/`) and ask Claude to
"migrate my suite to rstest" or "parallelize my tests". It has two lanes:
**readiness** drives `--migrate-check`, applies the right fix per verdict, and
wires up `[tool.rstest]` config + a CI gate; **speed** drives `--doctor` to find
the slowest tests, wait-bound (sleep/timeout) tests, the parallel-floor gate
test, and expensive fixtures, with the action for each. It asks before editing
your tests or CI.

## The migrate-check preflight

`rstest --migrate-check` is not a test run — it is a parallel-readiness
report. It turns the manual triage of step 3 ("a few tests fail in parallel,
read the guide, classify each by hand") into one command. It works in two
stages, stopping as early as it can:

**1. Collection stability.** It collects the suite **twice** and diffs the id
sets. Any test whose id appears in only one collection has a *run-to-run
unstable* id, classified by why:

- **address / uuid** — the id embeds a per-process value (a `repr()`-fallback
  `0x…` address, or a uuid). Every worker collects a different id, so the
  workers can't agree on the test set and rstest is forced to `-n 0`. Reported
  as **WILL bail** — a hard blocker. Fix: give the `parametrize` a stable
  `ids=`.
- **time** — a timestamp/date in the id. Usually stable enough *within* one
  run (workers collect near-simultaneously), so it typically runs fine at
  `-n auto`. Reported as **may bail**.

If a WILL-bail id is found it stops here — fix the ids first, since nothing
runs in parallel until they're stable.

**2. Parallel classification.** Otherwise it runs the suite at `-n auto` and
classifies every test that fails *only* under parallelism. The discriminator
reruns (`-n 0` twice, `--dist loadfile`) are **scoped to the files containing
failures**, so a clean suite runs zero of them and cost scales with the number
of failing files, not suite size. Each failure lands in one class:

| Class | Meaning | Fix it names |
|---|---|---|
| **NOT PARALLEL-SPECIFIC** | also fails at `-n 0` | a pre-existing bug / env gap — not a migration concern |
| **INTRINSIC FLAKE** | serial repeats disagree | flaky under any runner; fix the test |
| **ORDER DEPENDENCY** | passes serial + `loadfile`, fails under `load` | `--dist loadfile`, or fix the in-file coupling |
| **WALL-CLOCK / LOAD-SENSITIVE** | passes serial, fails parallel, wait-bound (wall ≫ cpu) | mock the clock / drop the tight deadline; stopgap `-n 4` |
| **ISOLATION / CO-LOCATION** | passes serial, fails under `load` **and** `loadfile` | reset the leaked global state; stopgap `@pytest.mark.serial` |

For ORDER-DEPENDENCY and ISOLATION findings it then **bisects the polluter** —
binary-searching for the file whose tests, run serially before the victim,
reproduce the failure — and reports `POLLUTED BY: <file>`, `SAME-FILE
co-location`, or that no serial ordering reproduces (a likely concurrent-
resource race rather than state pollution).

It exits non-zero if any WILL-bail id or parallelism-specific failure is
found, so it doubles as a **CI gate** that blocks new parallel-unsafe tests:

```console
$ rstest --migrate-check-json migrate.json \
         --migrate-allow tests/legacy/    # tolerate a triaged backlog
```

`--migrate-check-json` writes the findings as a versioned JSON document
([schema reference](../reference/report-json.md#migrate-check-json)) for
tooling and trending. `--migrate-allow <substr>` accepts known findings by
nodeid/site substring — they're still reported (marked `(allowed)`) but don't
fail the build, so the gate goes red only on **new** issues while you work
through the backlog. Full flag reference:
[`--migrate-check`](../reference/cli.md#-migrate-check).
