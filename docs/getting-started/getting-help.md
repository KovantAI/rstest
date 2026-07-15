# Getting help

- **Bugs and feature requests**: open an issue on the
  [GitHub repository](https://github.com/KovantAI/rstest/issues). For a
  behavioral difference from pytest, include the `-n 0` result — identical
  behavior there is the compatibility contract, and a difference is a bug
  we want.
- **Parallel-only failures**: work through the
  [three-run diagnosis](../guides/parallel-safety.md#diagnosing-a-parallel-only-failure)
  first; it classifies most cases.
- **Known gaps** are tracked honestly in
  [Compatibility](../concepts/compatibility.md#known-gaps).

## Project status

rstest is **alpha** software under active development. What that means
honestly:

- Versions are 0.x (pre-1.0); CLI flags and the report-json schema aim for
  stability but may change until 1.0 — every change is listed in the
  repository's `CHANGELOG.md`.
- The vendored pytest core carries a maintenance commitment: upstream
  pytest **security fixes are re-vendored and released within two weeks**
  ([policy](../concepts/compatibility.md#vendored-pytest-version)).
- Maintained by **Kovant AB**.
- Security reports: use GitHub's private vulnerability reporting on the
  repository — not a public issue.
