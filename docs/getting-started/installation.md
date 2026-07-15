# Installation

!!! warning "Pre-release"
    rstest is alpha software and not yet published to PyPI. Install from a
    built wheel or from source.

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

## From a wheel

```console
$ pip install rstest-*.whl
```

or into a uv-managed environment:

```console
$ uv pip install rstest-*.whl
```

The wheel ships a single `rstest` binary (the Rust orchestrator), the
`rstest_worker` Python package, and the vendored pytest core.

## With uv

rstest is not on PyPI yet, so there is no `uv add rstest` from the index.
Until then, the uv-native paths point at a built wheel or the git repo.

As a project dev-dependency (writes to `pyproject.toml` + lockfile), so it
installs into the same environment as your tests:

```console
$ uv add --dev ./rstest-*.whl
$ uv run rstest                     # runs in the project venv
```

Or pull straight from git:

```console
$ uv add --dev "rstest @ git+https://github.com/KovantAI/rstest"
```

As a standalone tool (not in any project), use `uv tool` / `uvx`:

```console
$ uv tool install ./rstest-*.whl
$ uvx --from ./rstest-*.whl rstest --version
```

A tool-scoped install still runs your project's tests — rstest discovers the
project interpreter at runtime (see *Which Python does rstest use?* below),
so the tool env and the test env stay separate. Two things therefore live
in two places: the `rstest` BINARY can live anywhere (tool env, ~/bin),
but the WORKER runtime — the `rstest_worker` package and its `msgpack`
dependency — must be importable by the *project* interpreter, because
workers run your tests in your environment. `pip install rstest` into the
project venv provides both at once; a tool-only install needs the wheel
in the project venv too.

Once rstest is published, `uv add --dev rstest` and `uvx rstest` will work
against the index directly.

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
