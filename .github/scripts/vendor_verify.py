#!/usr/bin/env python3
"""Generate and verify the vendored-pytest integrity manifest.

Three modes:

* ``--mode local``      offline: rehash ``_vendor/`` and diff against
                        ``vendor.lock`` (same check the shipped
                        ``rstest_worker._internal.verify_vendor`` runs; used by
                        the fast CI job on the source tree).
* ``--mode generate``   rebuild ``vendor.lock`` over the current ``_vendor/``,
                        pinning the pytest version and the upstream wheel's
                        PyPI sha256. Run this as part of the re-vendor
                        procedure (see python/VENDOR.md), then commit the file.
* ``--mode provenance`` download the pinned wheel, assert its sha256 matches
                        the manifest's trust anchor, extract it, and diff the
                        vendored tree against upstream. Proves verbatim ==
                        upstream. Needs the network.

The offline hashing core is imported from the shipped worker module so there is
a single source of truth for the file set and hashing rules.
"""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import sys
import urllib.request
import zipfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
PKG_ROOT = REPO_ROOT / "python" / "rstest_worker"
VENDOR_DIR = PKG_ROOT / "_vendor"
MANIFEST = PKG_ROOT / "vendor.lock"
MANIFEST_SCHEMA = 1

# Import the shipped hashing core (single source of truth for the file set).
sys.path.insert(0, str(REPO_ROOT / "python"))
from rstest_worker._internal.verify_vendor import compare, hash_tree  # noqa: E402


def _pypi_wheel(version: str) -> tuple[str, str, str]:
    """Return (filename, sha256, url) for the pure-python pytest wheel."""
    url = f"https://pypi.org/pypi/pytest/{version}/json"
    with urllib.request.urlopen(url) as resp:
        data = json.load(resp)
    for entry in data["urls"]:
        if entry["filename"].endswith("-py3-none-any.whl"):
            return entry["filename"], entry["digests"]["sha256"], entry["url"]
    raise SystemExit(f"no py3-none-any wheel found for pytest {version}")


def _read_manifest() -> dict:
    if not MANIFEST.is_file():
        raise SystemExit(f"vendor.lock not found at {MANIFEST}")
    return json.loads(MANIFEST.read_text(encoding="utf-8"))


def mode_local() -> int:
    manifest = _read_manifest()
    problems = compare(manifest.get("files", {}), hash_tree(VENDOR_DIR))
    if problems:
        print("INTEGRITY CHECK FAILED", file=sys.stderr)
        for line in problems:
            print(f"  {line}", file=sys.stderr)
        return 1
    print(f"local integrity OK: pytest {manifest.get('pytest_version')}")
    return 0


def mode_generate(version: str) -> int:
    filename, sha256, _url = _pypi_wheel(version)
    manifest = {
        "schema": MANIFEST_SCHEMA,
        "pytest_version": version,
        "upstream_wheel": filename,
        "upstream_wheel_sha256": sha256,
        "hash_algorithm": "sha256",
        "generated_by": "vendor_verify.py",
        "files": hash_tree(VENDOR_DIR),
    }
    MANIFEST.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    count = len(manifest["files"])
    print(f"wrote {MANIFEST.relative_to(REPO_ROOT)} for pytest {version} ({count} files)")
    return 0


def mode_provenance() -> int:
    manifest = _read_manifest()
    version = manifest["pytest_version"]
    _filename, pinned_sha, url = _pypi_wheel(version)

    if pinned_sha != manifest["upstream_wheel_sha256"]:
        print(
            "TRUST ANCHOR MISMATCH: PyPI's wheel sha256 differs from vendor.lock\n"
            f"  manifest: {manifest['upstream_wheel_sha256']}\n"
            f"  pypi:     {pinned_sha}",
            file=sys.stderr,
        )
        return 1

    with urllib.request.urlopen(url) as resp:
        blob = resp.read()
    got_sha = hashlib.sha256(blob).hexdigest()
    if got_sha != pinned_sha:
        print(f"downloaded wheel sha256 {got_sha} != pinned {pinned_sha}", file=sys.stderr)
        return 1

    # Extract upstream contents keyed the way the manifest is: "_vendor/<rel>".
    upstream: dict[str, str] = {}
    with zipfile.ZipFile(io.BytesIO(blob)) as zf:
        for info in zf.infolist():
            if info.is_dir():
                continue
            name = info.filename
            digest = hashlib.sha256(zf.read(name)).hexdigest()
            if name.startswith(("pytest/", "_pytest/")) or name == "py.py":
                upstream[f"_vendor/{name}"] = digest
            elif name.endswith((".dist-info/licenses/LICENSE", ".dist-info/LICENSE")):
                upstream["_vendor/LICENSE.pytest"] = digest

    problems = compare(upstream, hash_tree(VENDOR_DIR))
    if problems:
        print(f"PROVENANCE FAILED: differs from upstream pytest {version}", file=sys.stderr)
        for line in problems:
            print(f"  {line}", file=sys.stderr)
        return 1
    print(f"provenance OK: vendored tree == upstream pytest {version} ({len(upstream)} files)")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mode", choices=["local", "generate", "provenance"], required=True)
    parser.add_argument(
        "--version",
        help="pytest version for --mode generate (defaults to vendor.lock's pinned version)",
    )
    args = parser.parse_args()

    if args.mode == "local":
        return mode_local()
    if args.mode == "provenance":
        return mode_provenance()
    version = args.version or (_read_manifest()["pytest_version"] if MANIFEST.is_file() else None)
    if not version:
        raise SystemExit("--mode generate needs --version (no vendor.lock to read it from)")
    return mode_generate(version)


if __name__ == "__main__":
    sys.exit(main())
