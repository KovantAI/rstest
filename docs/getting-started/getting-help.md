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

rstest is alpha (0.x) software under active development; releases, support
policy and maintainer are listed under [Maturity](evaluating.md#maturity).
Report security issues through GitHub's private vulnerability reporting on
the repository, not a public issue ([Security](../reference/security.md)).
