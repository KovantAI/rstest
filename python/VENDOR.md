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
- Update procedure: re-extract from the new wheel verbatim; local
  modifications are forbidden in these two dirs (keep diffs in rstest_worker/).
