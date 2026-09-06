"""Offline integrity check for the vendored pytest tree.

Rehashes every file under ``rstest_worker/_vendor`` and compares it to the
committed manifest (``rstest_worker/vendor.lock``). This proves the shipped
vendored copy is byte-identical to what was committed — it catches accidental
edits, corruption, and partial namespaces.

It does NOT reach the network and does NOT prove the tree matches upstream
pytest; that stronger provenance check lives in
``.github/scripts/vendor_verify.py`` (``--mode provenance``).

Runnable as ``python -m rstest_worker._internal.verify_vendor`` and reused by
the ``rstest --verify-vendor`` flag and CI.
"""

from __future__ import annotations

import hashlib
import json
import sys
from pathlib import Path

# rstest_worker/ — parent of _internal/
PKG_ROOT = Path(__file__).resolve().parent.parent
VENDOR_DIR = PKG_ROOT / "_vendor"
MANIFEST = PKG_ROOT / "vendor.lock"


def _iter_vendor_files(vendor_dir: Path):
    """Every vendored file, excluding compiled artifacts (never hashed)."""
    for path in sorted(vendor_dir.rglob("*")):
        if not path.is_file():
            continue
        if "__pycache__" in path.parts or path.suffix == ".pyc":
            continue
        yield path


def hash_tree(vendor_dir: Path) -> dict[str, str]:
    """Map ``_vendor/<rel>`` -> sha256 for the current tree on disk."""
    tree: dict[str, str] = {}
    for path in _iter_vendor_files(vendor_dir):
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        rel = path.relative_to(vendor_dir).as_posix()
        tree[f"_vendor/{rel}"] = digest
    return tree


def compare(expected: dict[str, str], actual: dict[str, str]) -> list[str]:
    """Human-readable problems; empty list means the tree matches."""
    problems: list[str] = []
    for key in sorted(set(expected) - set(actual)):
        problems.append(f"MISSING   {key} (in manifest, not on disk)")
    for key in sorted(set(actual) - set(expected)):
        problems.append(f"UNEXPECTED {key} (on disk, not in manifest)")
    for key in sorted(set(expected) & set(actual)):
        if expected[key] != actual[key]:
            problems.append(f"MISMATCH  {key} (hash differs from manifest)")
    return problems


def main() -> int:
    if not MANIFEST.is_file():
        print(f"vendor.lock not found at {MANIFEST}", file=sys.stderr)
        return 1
    if not VENDOR_DIR.is_dir():
        print(f"_vendor not found at {VENDOR_DIR}", file=sys.stderr)
        return 1

    manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
    expected = manifest.get("files", {})
    actual = hash_tree(VENDOR_DIR)
    problems = compare(expected, actual)

    version = manifest.get("pytest_version", "?")
    if problems:
        print(f"vendored pytest {version}: INTEGRITY CHECK FAILED", file=sys.stderr)
        for line in problems:
            print(f"  {line}", file=sys.stderr)
        return 1
    print(f"vendored pytest {version}: {len(actual)} files verified against vendor.lock")
    return 0


if __name__ == "__main__":
    sys.exit(main())
