# Vendored code provenance

- `rstest_worker/_vendor/{pytest,_pytest,py.py}`: pytest 9.1.1, copied unmodified from the PyPI wheel
  contents (via a pytest==9.1.1 site-packages install). MIT licensed.
  - `rstest_worker/_vendor/LICENSE.pytest`: pytest's MIT license text, copied verbatim from the
    pytest 9.1.1 dist-info. Required by the MIT terms (license must ship with the vendored copies).
    Re-copy it whenever the vendored pytest version changes.
  - Architecture decision D7 (rstest-research/notes/track4-decision-memo.md):
    the shim IS vendored pytest at original import paths; rstest replaces
    orchestration, not semantics. Vendor must stay COMPLETE — partial
    namespaces don't fall back to site-packages (research spike 4).
  - Runtime deps of the vendored core (must exist in the target venv):
    pluggy>=1.5, iniconfig, packaging, pygments. We depend on REAL pluggy by design (D2).
- Integrity manifest: `rstest_worker/vendor.lock` pins the vendored pytest
  version, the upstream wheel's PyPI sha256 (the trust anchor), and a per-file
  sha256 of the whole `_vendor/` tree (excluding `__pycache__`/`.pyc`). It ships
  in the wheel so an installed copy can self-verify. `.gitattributes` marks
  `_vendor/**` and `vendor.lock` as `-text` so the hashes stay byte-stable
  across checkouts.
- Update procedure: re-extract from the new wheel verbatim; local
  modifications are forbidden in these two dirs (keep diffs in rstest_worker/).
  Then:
  1. `python .github/scripts/vendor_verify.py --mode generate --version <X.Y.Z>`
     — rebuilds `vendor.lock` (fetches the upstream wheel sha256 from PyPI).
  2. `python .github/scripts/vendor_verify.py --mode provenance` — proves the
     re-extracted tree is byte-identical to upstream. Must pass.
  3. Commit `_vendor/` and `vendor.lock` together. CI (`vendor.yml`) re-checks
     both the offline integrity and the upstream provenance.
- Verify anytime: `rstest verify-vendor` (offline, runs the shipped
  `rstest_worker._internal.verify_vendor`), or the CI checks above.
- New pytest releases are surfaced daily by
  `.github/workflows/pytest-upgrade-watch.yml`, which opens a tracking issue.
- Re-vendor history (which pytest version shipped in which release, with the
  commit + PR for each bump) is logged in the docs:
  [Security & supply chain → Re-vendor history](../docs/reference/security.md#re-vendor-history).
  Add a row there whenever you complete a bump via this procedure.
