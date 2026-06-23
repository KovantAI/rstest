# Security Policy

## Reporting a vulnerability

**Do not open a public issue for a security vulnerability.**

Report privately through GitHub's
[private vulnerability reporting](https://github.com/KovantAI/rstest/security/advisories/new)
on the repository. We aim to acknowledge a report within a few business
days and will coordinate a fix and disclosure timeline with you.

When reporting, please include:

- the rstest version (`rstest --version`),
- affected platform(s) and Python version,
- a minimal reproduction, and
- the impact you observed.

## Supported versions

rstest is alpha (0.0.x). Security fixes land on the **latest release**
only; there are no long-term support branches yet. Upgrade to the newest
version before reporting.

## Vendored pytest

rstest ships an **unmodified, vendored copy of pytest** (currently 9.0.3)
inside `rstest_worker._vendor`. When upstream pytest ships a security fix
affecting the vendored code, an rstest release with the re-vendored core is
expected **within two weeks** of the upstream release. Because the vendored
tree is verbatim, re-vendoring is mechanical; the two-week budget covers
re-running the compatibility battery, not the patch itself.

If you find a vulnerability that originates in pytest itself, please also
report it upstream to the
[pytest project](https://github.com/pytest-dev/pytest/security) so all
users benefit.
