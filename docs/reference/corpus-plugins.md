# Plugins exercised by the corpus

This maps every pytest plugin that **actually loads** during a
compatibility-corpus (`corpus/` in the repo) run to the suite(s) that load it.
It is the runtime-evidence half of the
[top-100 plugin matrix](top-100-plugins.md): where that table classifies plugins
(55 verified `V` or `V*`, 45 inferred `i`), this table records the ones a real suite
loads *and still meets the corpus parity floor under rstest*.

## What "exercised" means here, and doesn't

Each row means: the plugin registers a `pytest11` entry point in that suite's
venv, so it **loads into the vendored pytest core** during the run, and the suite
meets its parity floor (baseline `pytest` vs `rstest`, per-test outcome diff).
That is genuine runtime evidence the plugin **coexists** with rstest's pool.

It is **not** proof every plugin feature is verified:

- A suite may load a plugin without exercising its feature (e.g.
  `pytest-codspeed` / `pytest-benchmark` **auto-disable at `-n ≥ 2`**: loaded,
  but their measurement path is inert under the pool; benchmark at `-n 0`).
- Some suites run pinned below `-n auto` (`-n 0`, `-n 4`, `--collect lazy`); a
  plugin whose *only* corpus evidence is an `-n 0` suite carries **no parallel
  evidence**: flagged below.

Promoting a row to `V` in the [top-100 matrix](top-100-plugins.md) still needs a
targeted micro-suite that exercises the plugin's *feature* and diffs its
observable side effect. This table tells us **where that evidence already
exists** so the verification work starts from the strongest-covered plugins.

## Plugins by corpus coverage

Sorted by number of suites that load the plugin. All listed suites meet the
corpus parity floor under rstest.

| Plugin | # suites | Suites | Parallel evidence |
|---|---:|---|---|
| anyio | 7 | anyio, fastapi, httpx, langchain, langgraph, starlette, urllib3 | ✅ parallel (fastapi/langchain/langgraph/starlette `-n auto`; httpx `-n 0`) |
| hypothesis | 6 | anyio, attrs, packaging, pandas, pydantic, python-dateutil | ✅ parallel |
| pytest-cov | 6 | aiohttp, arrow, fastapi, langchain, python-dateutil, requests | ✅ parallel |
| pytest-mock | 6 | aiohttp, anyio, arrow, langchain, langgraph, pydantic | ✅ parallel |
| pytest-timeout | 6 | aiohttp, anyio, fastapi, jinja2, urllib3, werkzeug | ✅ parallel |
| pytest-xdist | 6 | aiohttp, attrs, fastapi, langchain, pandas, sqlalchemy | ✅ parallel (neutralized inside workers) |
| pytest-asyncio | 5 | aiohttp, django-allauth, langchain, langgraph, structlog | ✅ parallel |
| pytest-codspeed | 4 | aiohttp, fastapi, langchain, pydantic | ⚠️ loaded only; auto-disables at `-n ≥ 2` |
| inline-snapshot | 2 | fastapi, pydantic | ✅ parallel (fastapi `-n auto`) |
| langsmith | 2 | langchain, langgraph | ✅ parallel |
| pytest-benchmark | 2 | langchain, pydantic | ⚠️ loaded only; auto-disables at `-n ≥ 2` |
| pytest-run-parallel | 2 | markupsafe, pydantic | ✅ parallel (markupsafe `-n auto`) |
| pytest-socket | 2 | langchain, urllib3 | ✅ parallel |
| syrupy | 2 | langchain, langgraph | ✅ parallel |
| Faker | 1 | pydantic | 🔶 `-n 0` only |
| langchain-tests | 1 | langchain | ✅ parallel |
| pytest-aiohttp | 1 | aiohttp | ✅ parallel |
| pytest-django | 1 | django-allauth | ✅ parallel (`-n 4`; per-worker test DB naming, but on SQLite `:memory:` only) |
| pytest-examples | 1 | pydantic | 🔶 `-n 0` only |
| pytest-httpbin | 1 | requests | ✅ parallel |
| pytest-memray | 1 | urllib3 | ✅ parallel (`-n 4`) |
| pytest-pretty | 1 | pydantic | 🔶 `-n 0` only |
| pytest-randomly | 1 | structlog | ✅ parallel (seed synced across workers) |
| pytest-recording | 1 | langchain | ✅ parallel |
| pytest-retry | 1 | langgraph | ✅ parallel (no xdist in the venv, so rstest seeds `server_port`) |
| pytest-sugar | 1 | fastapi | ⚠️ loaded only; terminal plugin not painted at `-n ≥ 2` |
| time-machine | 1 | structlog | ✅ parallel |
| typeguard | 1 | tenacity | ✅ parallel |

**28 distinct plugin distributions across 22 suites.**

> `pytest-xdist`, `pytest-cov`, `pytest-timeout` register `pytest11` entry points
> but rstest **supersedes** them (native `-n`, `--cov`, `--timeout`); here they
> confirm *coexistence* (they load without breaking the run), not that you should
> run both: see the [top-100 matrix](top-100-plugins.md) "🟦 Native" rows.

## Weak / no evidence: needs a dedicated micro-suite

- **`-n 0`-only evidence** (no parallel data at all): `Faker`, `pytest-examples`,
  `pytest-pretty`, their sole corpus carrier (pydantic) runs at `-n 0`.
- **Loaded but feature inert under the pool**: `pytest-codspeed`,
  `pytest-benchmark` (auto-disable at `-n ≥ 2`), `pytest-sugar` (terminal
  plugin, not painted). Coexistence is proven; the *feature* is `-n 0`-only by
  the plugin's own design.

## Suites with no plugins (core pytest only)

These 11 suites load no third-party `pytest11` plugin, so they exercise the
vendored core + fixtures/parametrize/marks only, not plugin compat: **click,
flask, freezegun, itsdangerous, jsonschema, marshmallow, more-itertools, pluggy,
rich, trio, typer.**

## Promoted from `i` to `V`

These three were inferred until the corpus evidence below was confirmed; the
[top-100 matrix](top-100-plugins.md) now marks them `V`:

- **syrupy**: snapshot asserts under langchain + langgraph at `-n auto`.
- **pytest-recording**: VCR cassettes under langchain at `-n auto`.
- **pytest-retry**: rerun channel under langgraph at `-n auto`. The venv has
  no pytest-xdist, so rstest seeds the `server_port` the plugin's worker branch
  reads. With xdist installed the plugin instead self-provisions through its
  controller branch, covered by an e2e gate; see
  [parity divergences §8](parity-divergences.md#8-plugin-master-hook-gating-rstest-side-fixed).

The reverse also applies: `pytest-examples` and `pytest-pretty` are marked `i`
in the matrix because their only corpus evidence is the `-n 0` pydantic run.

### What pytest-django's evidence covers

django-allauth, the only corpus suite that loads pytest-django, configures
SQLite `:memory:`. An in-memory database is private to each process anyway, so
this run proves the plugin loads and passes under the pool, but it does **not**
exercise the `test_<name>_gwN` database naming a server-backed database
(Postgres, MySQL) relies on. Verify that on your own suite, or with
`rstest migrate-check`.

## How this table was produced

For each prepared suite venv under `corpus/work/<suite>/venv`, enumerate the
`pytest11` entry-point group and record the owning distribution. Regenerate after
a `corpus/run.py --prepare` refresh; the inventory tracks whatever the pinned
suites currently install.
