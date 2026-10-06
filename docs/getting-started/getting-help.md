# Getting help

Where to report problems and where to look first:

- **Bugs and feature requests**: open an issue on the
  [GitHub repository](https://github.com/KovantAI/rstest/issues). For a
  behavioral difference from pytest, include the `-n 0` result: identical
  behavior there is the compatibility contract, and a difference is a bug
  we want.
- **Questions**: the repository has no Discussions forum, so questions are
  welcome as [issues](https://github.com/KovantAI/rstest/issues) too.
- **Parallel-only failures**: run
  [`rstest migrate-check`](../reference/cli-commands.md#migrate-check) first;
  it classifies each failure and names the fix. For what it leaves open, see
  [Diagnosing a parallel-only failure](../guides/parallel-safety.md#diagnosing-a-parallel-only-failure).
- **Known gaps** are tracked in
  [Compatibility](../concepts/compatibility.md#known-gaps).

## Project status

rstest is **pre-1.0** software under active development. What that means:

- Versions are 0.x; CLI flags and the report-json schema aim for
  stability but may change until 1.0. Every change is listed in the
  repository's `CHANGELOG.md`.
- The vendored pytest core carries a maintenance commitment: when upstream
  pytest ships a security fix affecting the vendored code, an rstest release
  with the re-vendored core is expected **within two weeks** of the upstream
  release ([policy](../reference/security.md#handling-pytest-security-fixes)).
- Security fixes land on the **latest release** only; there are no
  long-term-support branches yet.
- Maintained by **Kovant AB**.
- Security reports: use GitHub's private vulnerability reporting on the
  repository, not a public issue.
