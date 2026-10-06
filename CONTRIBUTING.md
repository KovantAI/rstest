# Contributing to rstest

Thanks for your interest in rstest. It's a Rust orchestrator around a
vendored pytest core; contributions to either side are welcome.

## Reporting bugs and requesting features

Open an issue on the
[GitHub repository](https://github.com/KovantAI/rstest/issues).

For a **behavioral difference from pytest**, include the `rstest -n 0`
result. At `-n 0` (single-worker mode) rstest behaves exactly like pytest:
that is the compatibility contract, so a divergence is a bug we want to hear
about.

For **parallel-only failures**, work through the
[three-run diagnosis](https://python-rstest.readthedocs.io/en/stable/guides/parallel-safety/#diagnosing-a-parallel-only-failure)
first; it classifies most cases.

## Development setup

You need a stable Rust toolchain (with `rustfmt` and `clippy`), Python
3.10+, and [`uv`](https://github.com/astral-sh/uv).

```sh
# Rust orchestrator
cargo build --release

# Python worker + editable install (builds the binary via maturin)
uv sync
```

Install the pre-commit hooks once per clone (pre-commit is not in the dev
dependency group, so run it through `uvx`):

```sh
uvx pre-commit install
```

## Checks before opening a PR

CI runs these; run them locally first. Formatting and clippy run on Linux
only; the build, Rust tests, and end-to-end gate run on Linux, macOS, and
Windows:

```sh
cargo fmt --check                                        # formatting
cargo clippy --all-targets --all-features -- -D warnings # lints (warnings are errors)
cargo build --release                                    # build
cargo test --release                                     # Rust tests
uv run python e2e/gate.py                                # end-to-end test gate (+ persona specs)
```

The end-to-end Rust tests (`crates/rstest-cli/tests/`) need a python with
pytest and msgpack. They use `python3` from `PATH`, or the venv in
`RSTEST_TEST_VENV`, and skip with a `skipping:` line when it lacks them. To
run them against the project venv and fail instead of skipping, as CI does:

```sh
RSTEST_TEST_VENV=$PWD/.venv RSTEST_TEST_REQUIRE=1 cargo test --release
```

The Python worker has its own checks (Linux in CI):

```sh
uv lock --check                                          # lockfile up to date
uvx ruff@0.16.5 check python/rstest_worker python/tests
uvx ruff@0.16.5 format --check python/rstest_worker python/tests
uvx --with msgpack --with pytest --with coverage ty@0.0.75 check python/rstest_worker
uvx --with msgpack --with coverage --with pytest-cov pytest python/tests
```

`uvx pre-commit run --all-files` covers formatting, clippy, `cargo check`, and
the file hygiene hooks.

### Persona specs

What each persona (first-time evaluator, migrator, daily developer, CI owner,
maintainer) expects from rstest is written as Gherkin in
`e2e/personas/features/*.feature`. Those files are the source of truth: every
scenario runs against the built binary through pytest-bdd, and a step with no
definition in `e2e/personas/steps/` fails the run. `python e2e/gate.py` runs
them as its `personas` section; to iterate on them directly:

```sh
uv sync --group dev
uv run pytest e2e/personas                   # all persona scenarios
uv run pytest e2e/personas -k EV-10          # one scenario, by its id
```

Tag a scenario `@known_bug` to pin a current failure: it xfails until the bug
is fixed, then the unexpected pass fails the run so the tag gets dropped.
`@posix_only`, `@windows_only`, `@linux_only` and `@macos_only` skip a scenario
on other platforms.

### Output schemas

The JSON Schemas and field tables under `docs/reference/schemas/` are
generated from the Rust output types (via `schemars`). `cargo test` includes a
golden test that fails if a type drifts from its committed schema. After
changing a documented output type (e.g. `DoctorReport`, `FlakeStats`),
regenerate the artifacts:

```sh
RSTEST_BLESS_SCHEMAS=1 cargo test -p rstest-cli schema
```

Commit the regenerated files. They are embedded into
`docs/reference/output-schemas.md` via snippets, so the published docs stay in
lockstep with the code automatically.

### Writing docs

The site is built with [Zensical](https://zensical.org/) from `docs/` (nav
in `mkdocs.yml`, which Zensical reads). Check a docs change with
`zensical build --strict`, which fails on broken links and anchors (install the
tools from `docs/requirements.txt`; `zensical serve` previews it). Link pages
relatively (`../guides/replay.md`), never by an absolute `/...` URL: the strict
build doesn't check those, and a persona spec rejects them.

- **Where a page goes.** Getting started is for first contact, Playbooks for a
  whole persona's path (wait-bound suites, the inner loop, a plugin stack),
  Guides for one task, Concepts for how rstest works, Reference for exact
  flags, variables, codes and formats. Link to the reference instead of
  repeating a flag table or field list in a guide.
- **Headings are URLs.** Every heading becomes an anchor other pages link to.
  Renaming one breaks those links, so grep `docs/` for the old slug and update
  it in the same change.
- **No em dashes in prose.** Use a comma, colon, parentheses or a new sentence.
  Code blocks and CLI output are exempt.
- **Admonitions, sparingly:** `!!! warning` for data loss, silent no-ops, or
  anything that fails without an error; `!!! note` for version caveats such as
  "Unreleased"; `!!! tip` for an optional shortcut; `??? note` (collapsed) for
  long reference detail such as a full JSON Schema. Everything else is plain
  prose.
- **Tag every code block** (`console` for commands with output, `text` for
  plain output, the language otherwise).
- **Generated files** under `docs/reference/schemas/` are never edited by hand
  (see [Output schemas](#output-schemas)).

## Vendored pytest

`python/rstest_worker/_vendor/{pytest,_pytest,py.py}` is an **unmodified**
copy of pytest. Do not edit files there: local modifications are
forbidden. Behavioral changes belong in `rstest_worker/` (the orchestration
layer) instead. To bump the vendored version, re-extract from the new wheel
verbatim and update `python/VENDOR.md`. See that file for the full
provenance and update procedure.

## Pull requests

- Keep the compatibility contract intact: `-n 0` (single-worker mode) stays
  identical to pytest.
- Add or update tests for behavior changes.
- Note user-facing changes in `CHANGELOG.md`.
- Match the style of the surrounding code.

## License

rstest is dual-licensed under [Apache-2.0](LICENSE-APACHE) or
[MIT](LICENSE-MIT), at the user's option. Unless you state otherwise, any
contribution you submit for inclusion is dual-licensed as above, without any
additional terms or conditions.
