# Installation

```console
$ pip install rstest
```

or into a uv-managed project (installs alongside your test deps):

```console
$ uv add --dev rstest
```

or as a standalone tool:

```console
$ uv tool install rstest      # or run ad hoc: uvx rstest --version
```

Install rstest into the **same environment as your test dependencies** —
workers run your tests in that interpreter (see
[Which Python does rstest use?](#which-python-does-rstest-use) for the tool-vs-project split).

## Requirements

- Python **3.10 or newer** in the environment whose tests you run. This
  matches the supported CPython line — 3.9 reached end-of-life in October
  2025 and no longer receives security fixes, so rstest tracks 3.10+.
- macOS, Linux, or Windows. Windows uses an anonymous-pipe transport
  (Unix uses POSIX pipes); the full test gate runs on `windows-latest`
  in CI on every commit, and wheels are built and smoke-tested there.
  The broad public-suite corpus is run on macOS/Linux, so Windows is
  validated by the gate's end-to-end checks rather than at corpus
  scale.

rstest installs its own runtime dependencies (`msgpack`, `pluggy`,
`iniconfig`, `packaging`, `pygments`). It does **not** require pytest to be
installed — and it does not conflict with an installed pytest either: the
vendored pytest core lives inside the `rstest_worker` package and never
touches your `pytest` installation. (One exception: `rstest --try` runs your
suite under plain `pytest` to produce a baseline, so *that* command needs
pytest installed — see [`--try`](../reference/cli.md#-try).)

First run erroring? See [Troubleshooting](../reference/troubleshooting.md) —
it covers the common install/first-run failures (missing `msgpack`, wrong
interpreter, import errors).

The wheel ships a single `rstest` binary (the Rust orchestrator), the
`rstest_worker` Python package, and the vendored pytest core.

## Binary vs worker runtime (for tool-scoped installs)

A tool-scoped install (`uv tool install rstest`, `uvx rstest`) still runs
your project's tests — rstest discovers the project interpreter at runtime
(see [Which Python does rstest use?](#which-python-does-rstest-use)), so the
tool env and the test env stay separate. Two things therefore live in two
places: the `rstest` BINARY can live anywhere (tool env, `~/bin`), but the
WORKER runtime — the `rstest_worker` package and its `msgpack` dependency —
must be importable by the *project* interpreter, because workers run your
tests in your environment. `pip install rstest` / `uv add --dev rstest` into
the project venv provides both at once; a tool-only install needs rstest in
the project venv too.

## From a wheel (offline / air-gapped)

To install without index access, point pip or uv at a downloaded release
wheel:

```console
$ pip install rstest-*.whl
$ uv pip install rstest-*.whl          # or: uv add --dev ./rstest-*.whl
```

Or pull straight from git:

```console
$ uv add --dev "rstest @ git+https://github.com/KovantAI/rstest"
```

## Verifying a downloaded wheel

Release wheels are signed with [GitHub artifact attestations] (Sigstore-backed
build provenance). Verify that a wheel was built by this
repository's release workflow:

```console
$ gh attestation verify rstest-*.whl --repo KovantAI/rstest
```

Each release also ships a `SHA256SUMS` file.

[GitHub artifact attestations]: https://docs.github.com/en/actions/security-for-github-actions/using-artifact-attestations

## From source

Requires a Rust toolchain (stable) and [maturin]:

```console
$ git clone https://github.com/KovantAI/rstest
$ cd rstest
$ uvx maturin build --release
$ pip install target/wheels/rstest-*.whl
```

[maturin]: https://github.com/PyO3/maturin

## Which Python does rstest use?

Workers run in the interpreter of your project's environment, discovered in
this order:

1. [`--python`](../reference/cli.md#-python-path-or-version) on the command
   line — a path or a version request (`3.12`, `>=3.12,<3.13`, `pypy@3.10`)
2. `$VIRTUAL_ENV` (an activated virtualenv)
3. a `.venv` found walking up from the working directory
4. versioned `python` / `pythonX.Y` names on `PATH`
5. uv-managed interpreters, as a fallback for version requests the above
   can't satisfy

A `.python-version` file sets the *version* that filters those candidates; it
doesn't name an interpreter directly.

Install rstest into the same environment as your project's test
dependencies, exactly as you would pytest.

## Verify

```console
$ rstest --version
rstest 0.2.1
$ rstest --co -q   # list tests without running them
```
