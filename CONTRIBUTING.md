# Contributing to rstest

Thanks for your interest in rstest. It's a Rust orchestrator around a
vendored pytest core; contributions to either side are welcome.

## Reporting bugs and requesting features

Open an issue on the
[GitHub repository](https://github.com/KovantAI/rstest/issues).

For a **behavioral difference from pytest**, include the `rstest -n 0`
result. At `-n 0` rstest is pytest-exact: identical behavior there is the
compatibility contract, so a divergence is a bug we want to hear about.

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

Install the pre-commit hooks once per clone:

```sh
pre-commit install
```

## Checks before opening a PR

The CI gate runs these on Linux, macOS, and Windows, run them locally
first:

```sh
cargo fmt --check                          # formatting
cargo clippy --release -- -D warnings      # lints (warnings are errors)
cargo build --release                      # build
cargo test --release                       # Rust tests
python e2e/gate.py                         # end-to-end test gate
```

`pre-commit run --all-files` covers formatting, clippy, `cargo check`, and
the file hygiene hooks.

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

The site is built with MkDocs Material from `docs/` (nav in `mkdocs.yml`).
Check a docs change with `mkdocs build --strict` (install the tools from
`docs/requirements.txt`).

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

- Keep the compatibility contract intact: `-n 0` stays pytest-exact.
- Add or update tests for behavior changes.
- Note user-facing changes in `CHANGELOG.md`.
- Match the style of the surrounding code.

## License

rstest is dual-licensed under [Apache-2.0](LICENSE-APACHE) or
[MIT](LICENSE-MIT), at the user's option. Unless you state otherwise, any
contribution you submit for inclusion is dual-licensed as above, without any
additional terms or conditions.
