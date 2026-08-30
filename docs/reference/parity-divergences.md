# Parity divergences & upstream fixes

A catalogue of every reason a public suite diverges from byte-exact parity in
the [corpus](https://github.com/KovantAI/rstest/blob/main/corpus/README.md), the upstream root cause, and the concrete
change the **upstream project** could make to remove it. None of these are
rstest correctness bugs: each is either a deliberate rstest design choice
(vendoring), an upstream test that isn't deterministic, or an upstream test
that isn't parallel-safe. The corpus uses the strictest possible metric — exact
nodeid + per-phase outcome — so anything non-deterministic surfaces here rather
than being silently normalized away.

The classes, by what actually differs:

| Class | What differs | Suites | Upstream fix |
|---|---|---|---|
| Self-referential nodeid | the test id | requests, pydantic | don't parametrize on runtime paths |
| Non-deterministic nodeid | the test id | pydantic | stable parametrize ids |
| Run-dependent nodeid | the test id | (marshmallow, arrow — now resolved) | stable parametrize ids |
| Isolation defect | the outcome | typer | reset global state per test |
| Exact assert on a fuzzy value | the outcome | rich | assert the tolerant set |
| Wall-clock deadline | the outcome | urllib3, anyio, django-allauth | mock the clock |
| Real OS resource | the outcome | werkzeug, httpx | bind ephemeral / mark serial |
| Plugin master-hook gating | (was a crash) | pytest-retry, pytest-rerunfailures | — (rstest-side, fixed) |

---

## 1. Self-referential nodeids

**Outcome parity is 100%; the test passes in both runners.** What differs is
the *nodeid string*, because the test parametrizes on a value that names the
running pytest, and rstest runs a **vendored** pytest (so it can pin byte-exact
semantics independent of the installed version).

### requests — `tests/test_utils.py::TestExtractZippedPaths::test_unzipped_paths_unchanged`

```python
@pytest.mark.parametrize("path", ("/", __file__, pytest.__file__, "/etc/invalid/location"))
def test_unzipped_paths_unchanged(self, path):
    assert path == extract_zipped_paths(path)
```

`pytest.__file__` is baked into the id. Baseline →
`…/site-packages/pytest/__init__.py`; rstest →
`…/rstest_worker/_vendor/pytest/__init__.py`. One id can't pair → 1 missing +
1 extra.

### pydantic — `tests/test_docs.py` (the `sys.path` case)

Same family: an id derived from `sys.path`, whose worker-side entry points at
the vendored core.

**Upstream fix:** don't embed a runtime path in a parametrize id. Use a stable
literal id and pass the dynamic value through the body, or `ids=`:

```python
@pytest.mark.parametrize("path", [("self", __file__), ("pytest", None)], ids=lambda p: p[0])
def test_unzipped_paths_unchanged(self, path):
    name, value = path
    value = value or pytest.__file__
    assert value == extract_zipped_paths(value)
```

**rstest side:** none needed — this is intended vendoring (see
[compatibility](../concepts/compatibility.md#vendored-pytest-version)). It is a
*nodeid* difference, not a behavior difference.

---

## 2. Non-deterministic nodeids (memory addresses, reprs)

### pydantic — `tests/pydantic_core/**`

Several parametrize ids embed values that differ between **any** two Python
processes:

```
test_complex_with_special_methods[<…ComplexWithIndex object at 0x10ae4e660>-(10+0j)]
test_tuple_var_len_kwargs[…<generator object infinite_generator at 0x10cdbd840>…]
test_any_python[MyModel({… SchemaSerializer(… class: Py(0x0000000a28267810) …)})…]
```

The `0x…` addresses and object reprs change every run. Because full-collect
dispatch is **index-based** (each worker must agree on the ordered id list),
these per-process ids make workers' collection hashes diverge and rstest
**refuses to dispatch** ("workers collected different test sets") — pydantic
must run at `-n 0`.

**Upstream fix:** give these `parametrize` cases stable `ids=`. The values
under test are fine; only their *default* string id (which falls back to
`repr()`, hence the address) is unstable:

```python
@pytest.mark.parametrize("case", CASES, ids=[c.name for c in CASES])
```

**rstest side:** `-n 0`. (`--collect lazy` also fails here for an unrelated
reason, rc=4.)

---

## 3. Run-dependent nodeids — `now()` (resolved)

### marshmallow, arrow

A few parametrize ids embed `datetime.now()`. These differ between the baseline
*run* and the rstest *run*, but are **positionally stable across workers**
within a run (every worker evaluates them at the same collection position).

- **marshmallow** runs at full `-n auto`, 100% — the hashes match and the
  renamed ids pair 1:1 positionally. (`--collect lazy` *breaks* this: its
  file-affine reorder destroys the positional pairing → 99.66%.)
- **arrow** uses `--collect lazy` (one worker per file, no cross-worker hash
  compare) and is 100%.

**Upstream fix (still worthwhile):** ids derived from wall-clock are fragile —
pin them. `freeze_time` the parametrize source, or give explicit `ids=`. It
removes the dependence on collection-order stability entirely.

---

## 4. Isolation defect — leaked global state

### typer — `tests/test_callback_warning.py::test_warns_when_callback_is_not_supported`

Passes alone, passes as a file, passes serially; **fails only under `-n auto`**.
It asserts `pytest.warns(...)`; when a co-located sibling runs first in the same
worker process, leftover process warnings state makes the assertion miss. This
is a real isolation weakness **surfaced** (not caused) by parallelism — any
parallel runner co-locates differently than serial collection order.

**Upstream fix:** make the test independent of prior warnings state — reset the
registry inside the test, or use a fixture:

```python
@pytest.fixture(autouse=True)
def _clean_warnings():
    import warnings
    with warnings.catch_warnings():
        warnings.resetwarnings()
        yield
```

**rstest side:** `-n 4` lowers the co-location odds (→ ~99.93%, intermittent),
but only the upstream reset removes it. [`rstest --migrate-check`](../guides/migrate-from-pytest.md#the-migrate-check-preflight)
bisects and names the polluting sibling automatically.

---

## 5. Exact assert on a fuzzy value

### rich — `tests/test_syntax.py::test_syntax_guess_lexer`

```python
assert Syntax.guess_lexer("banana.html", "<%= @foo %>") == "rhtml"
assert Syntax.guess_lexer("banana.html", "{{something|filter:3}}") == "html+django"
```

Pygments scores several lexers equally for ambiguous `.html` content and breaks
the tie by **registry enumeration order**, which shifts when extra lexer plugins
are installed or when prior tests populate pygments' lazy cache in a different
order. Flakes ~1 in 5 *full-suite* runs under **plain sequential pytest** — not
an rstest effect. On an unlucky run the baseline and rstest disagree on this one
test → ~99.8%.

The maintainers already know: the sibling `test_traceback.py::test_guess_lexer_yaml_j2`
asserts `in ("text", "YAML+Jinja")` — tolerating both outcomes. The syntax copy
was never hardened the same way.

**Upstream fix:** assert the tolerant set, mirroring the traceback test:

```python
assert Syntax.guess_lexer("banana.html", "<%= @foo %>") in ("rhtml", "html+php", "html")
```

**rstest side:** none — both runners are affected equally.

---

## 6. Wall-clock deadline asserts

These assert on **elapsed time** or a time window. They pass at low concurrency
and miss the deadline when the machine is oversubscribed — under *any* parallel
runner. Not ordering, not isolation: load.

### urllib3 — `test/test_wait.py::test_eintr`

```python
signal.setitimer(signal.ITIMER_REAL, 0.1, 0.1)
wfs(a, read=True, timeout=1)
assert 0.9 < dur < 3      # hard upper bound
```

The `< 3` upper bound breaks under CPU saturation.

### anyio — subprocess-cancellation tests

Spawn a subprocess, cancel it, assert termination within a deadline.

### django-allauth — rate-limit window tests

"N attempts within T seconds → blocked"; the window arithmetic drifts under
load.

**Upstream fix:** mock the clock instead of asserting on real elapsed time
(`freezegun`, a fake `time.monotonic`, or assert only the lower bound /
behavioral outcome, never a tight upper bound).

**rstest side:** `-n 4` for headroom; `@pytest.mark.serial` for the worst; or
`--reruns` as a visible stopgap.

---

## 7. Real OS-resource contention

### werkzeug — `tests/test_serving.py::test_server[unix socket]`

Boots a real dev-server subprocess on a unix socket and connects. Parallel-unsafe
*and* flaky under any runner (it fails even serial/alone in some environments) —
on a given run baseline and rstest can disagree → ~99.9%.

**Upstream fix:** `@pytest.mark.xdist_group("dev_server")` (or rstest's
`@pytest.mark.serial`) to keep all dev-server tests off concurrent workers, and
a startup/connect retry instead of a fixed deadline.

### httpx — the session `server` fixture

```python
@pytest.fixture(scope="session")
def server():
    config = Config(app=app, lifespan="off", loop="asyncio")  # uvicorn default port 8000
    ...
```

Session scope is **per worker**, so every worker that touches it binds the same
fixed port 8000 → clash/hang. The fixture is used across 9 files, so `--dist
loadfile` can't confine it and there are no `xdist_group` markers for `--dist
loadgroup`. Pinned `-n 0`.

**Upstream fix:** bind an **ephemeral** port (`port=0`, read back the assigned
port), the standard xdist-safe pattern. Then the fixture duplicates safely per
worker and httpx runs fully parallel.

---

## 8. Plugin master-hook gating (rstest-side, fixed)

### pytest-retry `server_port` (langgraph `checkpoint-sqlite`)

pytest-retry's controller branch is gated on
`has_plugin("xdist") and getoption("numprocesses")`; it starts a `ReportServer`
and stashes its port for workers. rstest used to null `numprocesses` to keep
xdist inert, which sent the plugin down its *worker* branch to read a
`workerinput["server_port"]` no controller had set → `KeyError`.

**Fix (rstest side, done):** xdist's session gates on `dist != "no"`, not
`numprocesses` — so rstest forces `dist="no"` and leaves `numprocesses` visible.
The plugin's controller branch then fires per worker (each self-provisions an
ephemeral `ReportServer`), and `configure_node` hooks that read
later-stashed state are retried at `pytest_sessionstart`. See
[xdist hooks](../concepts/xdist-hooks.md). No upstream change required.

### pytest-rerunfailures `sock_port`

pytest-rerunfailures with pytest-xdist installed splits master vs. worker on
`workerinput` **presence** (not `numprocesses`), so every rstest pool worker
takes its client branch and reads `workerinput["sock_port"]` — a key only an
xdist master sets → `KeyError` at configure under `-n ≥ 2`. Unlike pytest-retry
there is no knob to flip it to the self-provisioning branch.

**Fix (rstest side, done):** rstest wants the plugin inert under the pool
anyway (it owns reruns natively — crash-aware, `@mark.flaky` — and an active
plugin would double-rerun), so it **unregisters** it in `pytest_cmdline_main`,
before the historic `pytest_configure` call that would otherwise read the
missing key. At `-n 0` the plugin is left native. See
[xdist hooks](../concepts/xdist-hooks.md#when-self-provisioning-cant-apply-pytest-rerunfailures).
No upstream change required.

---

## Summary: the upstream-fix shortlist

For a suite maintainer who wants byte-exact parallel parity:

1. **Never put a runtime path, memory address, or `now()` in a parametrize id.**
   Use `ids=` with stable labels. (requests, pydantic, marshmallow, arrow)
2. **Reset mutated global state per test** — warnings filters, registries,
   env, `sys.path`. (typer)
3. **Assert tolerant sets, not exact values, for inherently fuzzy results.**
   (rich)
4. **Mock the clock; never assert a tight upper time bound.** (urllib3, anyio,
   django-allauth)
5. **Bind ephemeral ports / mark serial for fixed OS resources.** (httpx,
   werkzeug)
