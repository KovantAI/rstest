# Contributing to rstest

Thanks for your interest in rstest. It's a Rust orchestrator around a
vendored pytest core; contributions to either side are welcome.

## Reporting bugs and requesting features

Open an issue on the
[GitHub repository](https://github.com/KovantAI/rstest/issues).

For a **behavioral difference from pytest**, include the `rstest -n 0`
result. At `-n 0` rstest is pytest-exact — identical behavior there is the
compatibility contract, so a divergence is a bug we want to hear about.

For **parallel-only failures**, work through the
[three-run diagnosis](https://rstest.readthedocs.io/en/latest/guides/parallel-safety/#diagnosing-a-parallel-only-failure)
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

The CI gate runs these on Linux, macOS, and Windows — run them locally
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

## Vendored pytest

`python/rstest_worker/_vendor/{pytest,_pytest,py.py}` is an **unmodified**
copy of pytest. Do not edit files there — local modifications are
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
