# Installation

Install rstest from PyPI with pip:

```console
$ pip install rstest
```

Or add it to a uv-managed project (installs alongside your test deps):

```console
$ uv add --dev rstest
```

Install rstest into the **same environment as your test dependencies**:
workers run your tests in that interpreter (see
[Which Python does rstest use?](#which-python-does-rstest-use)). A
standalone tool install is covered in
[Binary vs worker runtime](#binary-vs-worker-runtime-for-tool-scoped-installs).

## Verify your install

From your project root, with one test file (`test_ok.py`) present:

```console
$ rstest --version
rstest 0.8.0
$ rstest --co -q   # list tests without running them
test_ok.py::test_ok

1 test collected in 0.00s
```

In an empty folder the same `rstest --co -q` prints `no tests collected` and
exits with code 5. That is expected: the install works, there is just nothing
to run yet.

## Requirements

- Python **3.10 or newer** in the environment whose tests you run. This
  matches the supported CPython line: 3.9 reached end-of-life in October
  2025 and no longer receives security fixes, so rstest tracks 3.10+.
- macOS on Apple silicon (arm64), Linux, or Windows. There is no Intel
  (x86_64) macOS wheel and no source distribution, so `pip install rstest`
  fails on an Intel Mac. Windows uses an anonymous-pipe transport
  (Unix uses POSIX pipes); the full test gate runs on `windows-latest`
  in CI on every commit, and wheels are built and smoke-tested there.
  The broad public-suite corpus is run on macOS/Linux, so Windows is
  validated by the gate's end-to-end checks rather than at corpus
  scale.

rstest installs its own runtime dependencies (`msgpack`, `pluggy`,
`iniconfig`, `packaging`, `pygments`, plus `exceptiongroup` and `tomli` on
Python 3.10 and `colorama` on Windows). It does **not** require pytest to be
installed (it is not a dependency, so installing rstest never installs or
upgrades pytest), and it does not conflict with an installed pytest either: the
vendored pytest core lives inside the `rstest_worker` package and never
touches your `pytest` installation. The one exception is
[`rstest try`](../reference/cli-commands.md#try): it runs a plain-pytest
baseline, so that command needs `python -m pytest` to work in the project
environment.

The wheel ships a single `rstest` binary (the Rust orchestrator), the
`rstest_worker` Python package, and the vendored pytest core.

First run erroring? See [Troubleshooting](../reference/troubleshooting.md):
it covers the common install/first-run failures (no usable interpreter or a
missing worker shim, rstest picking the wrong Python, `rstest: command not
found`, and import errors from a Python older than 3.10).

## Your suite runs on pytest 9

Adopting rstest adopts pytest 9. Tests always run on the vendored pytest core
(currently 9.1.1), whatever pytest version your project or its plugins pin,
and there is no older-core build. A plugin's `pytest<9` install pin is inert
at runtime: the plugin loads into the vendored core and must support pytest 9
itself ([Plugin versions vs the vendored core](../concepts/compatibility.md#plugin-versions-vs-the-vendored-core)).

pytest 9 is a cleanup major: it removes APIs that already warned throughout
8.x. A suite that is warning-clean on a recent pytest 8.x is almost always
already pytest-9-clean. If it isn't, clear the deprecations first, the same
upgrade you would owe pytest anyway.

!!! note "Still on pytest 8?"
    Your installed pytest does not need upgrading to install rstest. Your
    suite does need to pass on pytest 9, which is what rstest runs. Check it
    first on your current pytest 8.x:

    ```console
    $ python -m pytest -W error::pytest.PytestDeprecationWarning
    ```

    Each failure is a deprecated API to fix. A clean run is not the whole
    check: a few pytest 9 behavior changes raise no warning, so finish with
    the short list and the `rstest -n 0` backstop in
    [Upgrading to pytest 9](../guides/upgrade-to-pytest9.md#the-method).

## Binary vs worker runtime (for tool-scoped installs)

rstest can also be installed as a standalone tool:

```console
$ uv tool install rstest      # or run ad hoc: uvx rstest --version
```

A tool-scoped install (`uv tool install rstest`, `uvx rstest`) still runs
your project's tests: rstest discovers the project interpreter at runtime
(see [Which Python does rstest use?](#which-python-does-rstest-use)), so the
tool env and the test env stay separate. Two things therefore live in two
places: the `rstest` **binary** can live anywhere (tool env, `~/bin`), but the
**worker** runtime (the `rstest_worker` package and its dependencies:
`msgpack` for the worker protocol, plus `pluggy`, `iniconfig`, `packaging`
and `pygments` for the vendored pytest core) must be importable by the *project* interpreter, because workers run your
tests in your environment. `pip install rstest` / `uv add --dev rstest` into
the project venv provides both at once; a tool-only install needs rstest in
the project venv too.

## From a wheel or git

For an air-gapped install, point pip or uv at a downloaded release wheel (no
index access needed):

```console
$ pip install rstest-*.whl
$ uv pip install rstest-*.whl          # or: uv add --dev ./rstest-*.whl
```

To track an unreleased revision, install straight from git (needs network):

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
   line: a path or a version request (`3.12`, `>=3.12,<3.13`, `pypy@3.10`)
2. `$VIRTUAL_ENV` (an activated virtualenv)
3. a `.venv` found walking up from the working directory
4. versioned `python` / `pythonX.Y` names on `PATH`
5. uv-managed interpreters, as a fallback for version requests the above
   can't satisfy

A `.python-version` file sets the *version* that filters those candidates; it
doesn't name an interpreter directly. It is a soft pin: a usable virtualenv
(`$VIRTUAL_ENV` or the project's `.venv`) wins over it, with a warning when
the versions differ, so a stale pin never rejects the project's own
environment.

When `$VIRTUAL_ENV` (or a `PATH` interpreter) wins over a project `.venv` and
the run then fails on an import (`ModuleNotFoundError`), rstest prints a hint
naming both environments. It stays quiet otherwise: running in a tox or nox
environment while a `.venv` also exists is a normal setup.

Install rstest into the same environment as your project's test
dependencies, exactly as you would pytest. If rstest finds the project's
virtualenv but rstest isn't installed in it, it stops with an error naming
that venv rather than running your tests with some other interpreter that
lacks your dependencies (see
[Troubleshooting](../reference/troubleshooting.md#found-venvbinpython-but-rstest-is-not-installed-in-it)).
Pass `--python` to choose a different interpreter on purpose.
