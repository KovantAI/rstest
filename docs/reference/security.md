# Security & supply chain

rstest ships a compiled Rust orchestrator plus a vendored pytest core inside
a Python wheel. That is a wider trust surface than a pure-Python package, so
this page states — in one place — how releases are built and signed, what the
runner does and does not do on your machine, and how the vendored pytest is
sourced and kept current. For the reporting process and the exact policy text,
[`SECURITY.md`](https://github.com/KovantAI/rstest/blob/main/SECURITY.md) in
the repository remains authoritative; this page mirrors and expands it.

## Reporting a vulnerability

**Do not open a public issue for a security vulnerability.** Report privately
through GitHub's [private vulnerability reporting][gh-advisory] on the
repository. Kovant AB aims to acknowledge a report within a few business days
and will coordinate a fix and disclosure timeline with you.

Include the rstest version (`rstest --version`), affected platform(s) and
Python version, a minimal reproduction, and the impact you observed. If the
issue originates in pytest itself, please also report it upstream to the
[pytest project][pytest-sec] so all users benefit.

[gh-advisory]: https://github.com/KovantAI/rstest/security/advisories/new
[pytest-sec]: https://github.com/pytest-dev/pytest/security

## Supported versions

rstest is pre-1.0 (0.x). Security fixes land on the **latest release** only;
there are no long-term-support branches yet. Upgrade to the newest version
before reporting.

## Release integrity

Every release is built and published by the
[release workflow](https://github.com/KovantAI/rstest/blob/main/.github/workflows/release.yml),
not from a maintainer's laptop. Two properties follow:

- **Signed build provenance (SLSA).** Each wheel is signed with
  [GitHub artifact attestations][gh-attest] (Sigstore-backed, keyless via
  OIDC) at build time and the attestation is stored in GitHub's attestation
  store. You can verify any downloaded wheel was built by this repository's
  workflow — see [Verifying your install](#verifying-your-install) below.
- **Trusted Publishing to PyPI.** Wheels are published via PyPI Trusted
  Publishing (OIDC) — **no long-lived API tokens are stored** in the
  repository or CI. The publisher is scoped to this repo and workflow.

Wheels are built as per-platform binaries (manylinux, musllinux, macOS arm64,
Windows x86_64/arm64) — they contain a compiled Rust extension, so they are
**not** pure-Python and are not bit-for-bit reproducible; provenance is
established by attestation, not by reproducible builds. Each release also
ships a `SHA256SUMS` file.

[gh-attest]: https://docs.github.com/en/actions/security-for-github-actions/using-artifact-attestations

## Vendored pytest

rstest ships an **unmodified, vendored copy of pytest** (currently **9.1.1**)
inside the `rstest_worker._vendor` package. It sits at pytest's usual
`pytest` / `_pytest` import paths so plugins keep class identity with the real
library. Provenance and the update procedure are documented in
[`python/VENDOR.md`](https://github.com/KovantAI/rstest/blob/main/python/VENDOR.md);
licensing is in [License](license.md#vendored-software). Key points:

- The vendored tree is copied **verbatim** from the pytest 9.1.1 PyPI wheel.
  Local modifications inside the vendored directories are forbidden by policy —
  rstest replaces orchestration, not pytest semantics.
- There is **one** vendored core, tracked forward. Adopting rstest adopts
  pytest 9's behavior regardless of the pytest version installed elsewhere in
  your environment; there is no older-core build.
- The vendored core's own runtime dependencies must exist in the target
  virtualenv: `pluggy>=1.5`, `iniconfig`, `packaging`, `pygments`. rstest
  depends on the **real** pluggy by design.

### Handling pytest security fixes

When upstream pytest ships a security fix affecting the vendored code, an
rstest release with the re-vendored core is **aimed for within two weeks** of
the upstream release. Because the vendored tree is verbatim, re-vendoring is
mechanical; the two-week budget covers re-running the compatibility battery,
not the patch itself.

**How new pytest releases are detected.** A scheduled workflow —
[`pytest-upgrade-watch.yml`](https://github.com/KovantAI/rstest/blob/main/.github/workflows/pytest-upgrade-watch.yml)
— runs **daily (07:00 UTC)** and compares the version vendored under
`python/rstest_worker/_vendor` against the latest pytest on PyPI. When PyPI is
ahead, it opens a deduplicated tracking issue (label `pytest-upgrade`) that
links the verbatim re-extract procedure in
[`python/VENDOR.md`](https://github.com/KovantAI/rstest/blob/main/python/VENDOR.md)
and requires the full e2e gate before merge. Because a pytest security fix
ships as a new PyPI release, it is surfaced by this watch within a day of
publication — the watch tracks *releases*, not an advisory feed directly, but
a security release is a release.

### Re-vendor history

**Why this table is short.** It records changes to the *vendored pytest*, not
rstest releases. pytest has been on 9.1.1 since rstest 0.1.0 (2026-06-23), so
there has been nothing to re-vendor since — a short table here means the core
has been stable, not that the docs are stale. For rstest's own release cadence
and per-version changes, see the
[CHANGELOG](https://github.com/KovantAI/rstest/blob/main/CHANGELOG.md).

Every vendored-pytest change, newest first, with the commit that re-extracted
the tree and the rstest release it shipped in:

| Vendored pytest | Change | PR / commit | rstest release |
|---|---|---|---|
| **9.1.1** | 9.1.0 → 9.1.1 | [`54ed39d`](https://github.com/KovantAI/rstest/commit/54ed39d) · [#30](https://github.com/KovantAI/rstest/pull/30) | 0.1.0 (2026-06-23) |
| 9.1.0 | 9.0.3 → 9.1.0 | [`ab80f28`](https://github.com/KovantAI/rstest/commit/ab80f28) · [#30](https://github.com/KovantAI/rstest/pull/30) | 0.1.0 (2026-06-23) |
| 9.0.3 | initial vendored core | [`df7dcf1`](https://github.com/KovantAI/rstest/commit/df7dcf1) | 0.0.1 (2026-06-10) |

Both post-initial bumps landed together in [#30](https://github.com/KovantAI/rstest/pull/30)
and released in 0.1.0; see the
[CHANGELOG](https://github.com/KovantAI/rstest/blob/main/CHANGELOG.md) for the
release notes. Keep this table updated whenever the
[pytest-upgrade-watch](#handling-pytest-security-fixes) issue is actioned.

### Verifying the vendored copy is unmodified

The vendored tree is covered by an integrity manifest,
`rstest_worker/vendor.lock`, which pins the pytest version, the upstream
wheel's PyPI sha256 (the trust anchor), and a sha256 of every file under
`_vendor/`. The manifest ships in the wheel, so any installed copy can verify
itself. Two levels of check:

- **Offline integrity — anyone, anytime.** `rstest verify-vendor` rehashes
  the installed `_vendor/` tree and compares it to `vendor.lock`, catching a
  modified, corrupted, or partial vendored copy. It runs without contacting the
  network and exits non-zero on any drift:

    ```console
    $ rstest verify-vendor
    vendored pytest 9.1.1: 84 files verified against vendor.lock
    ```

- **Upstream provenance — CI.** The
  [`vendor.yml`](https://github.com/KovantAI/rstest/blob/main/.github/workflows/vendor.yml)
  workflow runs the offline check on every change and, in a separate job,
  downloads the pinned pytest wheel, asserts its sha256 against the manifest's
  trust anchor, and diffs the extracted tree against `_vendor/` — proving the
  vendored copy is byte-identical to upstream pytest, not merely internally
  consistent. This also re-runs weekly to catch drift. The same provenance
  check is part of the re-vendor procedure in
  [`python/VENDOR.md`](https://github.com/KovantAI/rstest/blob/main/python/VENDOR.md).

The offline check answers "is my installed pytest core the one that shipped?";
the provenance check answers "is what shipped really upstream pytest?".

## Dependency auditing

**Rust dependencies** are audited in CI with
[cargo-deny](https://github.com/EmbarkStudios/cargo-deny), configured in
[`deny.toml`](https://github.com/KovantAI/rstest/blob/main/deny.toml):

- **Advisories** — RustSec vulnerabilities and yanked crates fail the check;
  the advisory database is fetched fresh each run.
- **Licenses** — an allow-list of permissive licenses (MIT, Apache-2.0,
  ISC, …); a dependency introducing a license outside the set fails.
- **Sources** — every crate must come from crates.io; an unknown registry or
  git source fails.

The Rust toolchain is pinned to `stable` via
[`rust-toolchain.toml`](https://github.com/KovantAI/rstest/blob/main/rust-toolchain.toml)
so local dev, pre-commit, and CI build with the same compiler.

**Python dependencies** are audited in CI with
[pip-audit](https://github.com/pypa/pip-audit) (the `pip-audit` job in
[`ci.yml`](https://github.com/KovantAI/rstest/blob/main/.github/workflows/ci.yml)):
the locked runtime dependency tree exported from `uv.lock` is checked against
the PyPI/OSV advisory feed on every push and pull request. Dev-only
dependencies are excluded — they are not part of the shipped artifact.

## What rstest runs on your machine

rstest orchestrates test execution; it does not sandbox your code. The trust
boundary is worth stating plainly:

- **Your test code and conftest run with your privileges**, in worker
  processes, exactly as under pytest. rstest schedules and isolates workers;
  it does not restrict what your tests can do.
- **The vendored pytest is on the import path** at `_pytest.*`. Within a
  worker, imports of pytest internals resolve to the vendored copy — see
  [Architecture](../concepts/architecture.md) for how the worker environment
  is assembled.

### No telemetry, no network calls

**rstest itself makes no network calls and collects no telemetry.** The
orchestrator has no HTTP client or analytics SDK compiled in; its only socket
use is **local inter-process communication** between the orchestrator and its
worker processes (a Unix domain socket / Windows named pipe carrying msgpack —
never a TCP/UDP connection to any remote host). The shared-duration cache
(`--cache-remote`) reads and writes a **filesystem path or `file://` URL
only** — it does not fetch over the network.

The network access rstest *does not* make is not the same as your run making
none. Unchanged from pytest, the following still reach the network, because
they are your code or your commands, not rstest's:

- **Your tests, fixtures, conftest, and plugins** run with your privileges and
  may do whatever I/O they always did.
- **The CI recipes** in the [CI quickstart](../guides/ci-quickstart.md) call
  `gh`, `aws s3`, or `actions/*` to move cache segments — those are steps in
  *your* pipeline, not rstest reaching out.
- **Installing** rstest (`pip`/`uv`) or tracking a git revision fetches over
  the network like any package install; that is your package manager, not the
  runner at test time.

## Verifying your install

Verify a downloaded wheel was built by this repository's release workflow:

```console
$ gh attestation verify rstest-*.whl --repo KovantAI/rstest
```

For fully pinned, reproducible installs, install with hashes from your
lockfile — e.g. pip:

```console
$ pip install --require-hashes -r requirements.txt
```

or pin an exact version (`pip install rstest==0.7.0`). See
[Installation](../getting-started/installation.md#verifying-a-downloaded-wheel)
for the `SHA256SUMS` file and building from source.

## SBOM

Each release ships two CycloneDX Software Bills of Materials, generated by the
[release workflow](https://github.com/KovantAI/rstest/blob/main/.github/workflows/release.yml)
and attached as release assets:

- **`sbom.python.cdx.json`** — the Python runtime dependencies a user installs
  alongside the wheel (inventoried from a clean install of the built wheel).
- **`sbom.rust.cdx.json`** — the Rust crate graph compiled into the
  orchestrator (via `cargo cyclonedx`).

Both are listed in the release's `SHA256SUMS`. One component is **not**
captured by either SBOM: the vendored pytest lives *inside* the wheel rather
than as a declared dependency, so tools don't see it — its version (9.1.1) and
that core's own runtime deps are documented under
[Vendored pytest](#vendored-pytest) and in
[`python/VENDOR.md`](https://github.com/KovantAI/rstest/blob/main/python/VENDOR.md).

## Governance

rstest is maintained by Kovant AB under a dual
[Apache-2.0 / MIT license](license.md). It is pre-1.0 software on a single
vendored pytest core tracked forward; weigh that maturity against your risk
tolerance for a dependency on the CI critical path.
