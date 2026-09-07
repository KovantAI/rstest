"""Unit tests for the offline vendored-tree integrity verifier.

Exercises the hashing core (`hash_tree`), the manifest diff (`compare`), and
the `main` entrypoint used by `rstest --verify-vendor`. The real `_vendor/`
tree and `vendor.lock` are never touched: tests build a throwaway tree under
`tmp_path` and point the module's globals at it.
"""

import hashlib
import json

from rstest_worker._internal import verify_vendor


def _sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


# --- hash_tree -------------------------------------------------------------


def test_hash_tree_maps_vendor_relative_paths_to_sha256(tmp_path):
    (tmp_path / "a.py").write_bytes(b"print(1)\n")
    sub = tmp_path / "pkg"
    sub.mkdir()
    (sub / "b.py").write_bytes(b"x = 2\n")

    tree = verify_vendor.hash_tree(tmp_path)

    assert tree == {
        "_vendor/a.py": _sha(b"print(1)\n"),
        "_vendor/pkg/b.py": _sha(b"x = 2\n"),
    }


def test_hash_tree_skips_pycache_and_pyc(tmp_path):
    (tmp_path / "keep.py").write_bytes(b"k\n")
    (tmp_path / "stale.pyc").write_bytes(b"bytecode")
    cache = tmp_path / "__pycache__"
    cache.mkdir()
    (cache / "keep.cpython-312.pyc").write_bytes(b"more")

    tree = verify_vendor.hash_tree(tmp_path)

    assert list(tree) == ["_vendor/keep.py"]


def test_hash_tree_uses_posix_separators_for_nested_paths(tmp_path):
    deep = tmp_path / "_pytest" / "mark"
    deep.mkdir(parents=True)
    (deep / "structures.py").write_bytes(b"s\n")

    tree = verify_vendor.hash_tree(tmp_path)

    # Forward slashes on every platform so the manifest is portable.
    assert "_vendor/_pytest/mark/structures.py" in tree


def test_hash_tree_empty_tree_is_empty_map(tmp_path):
    assert verify_vendor.hash_tree(tmp_path) == {}


# --- compare ---------------------------------------------------------------


def test_compare_identical_trees_reports_no_problems():
    manifest = {"_vendor/a.py": "h1", "_vendor/b.py": "h2"}

    assert verify_vendor.compare(manifest, dict(manifest)) == []


def test_compare_reports_missing_file_in_manifest_not_on_disk():
    expected = {"_vendor/a.py": "h1", "_vendor/gone.py": "h2"}
    actual = {"_vendor/a.py": "h1"}

    problems = verify_vendor.compare(expected, actual)

    assert problems == ["MISSING   _vendor/gone.py (in manifest, not on disk)"]


def test_compare_reports_unexpected_file_on_disk_not_in_manifest():
    expected = {"_vendor/a.py": "h1"}
    actual = {"_vendor/a.py": "h1", "_vendor/extra.py": "h2"}

    problems = verify_vendor.compare(expected, actual)

    assert problems == ["UNEXPECTED _vendor/extra.py (on disk, not in manifest)"]


def test_compare_reports_hash_mismatch_for_edited_file():
    expected = {"_vendor/a.py": "original"}
    actual = {"_vendor/a.py": "tampered"}

    problems = verify_vendor.compare(expected, actual)

    assert problems == ["MISMATCH  _vendor/a.py (hash differs from manifest)"]


def test_compare_orders_problems_missing_then_unexpected_then_mismatch():
    expected = {"_vendor/miss.py": "h", "_vendor/same.py": "h", "_vendor/edit.py": "old"}
    actual = {"_vendor/extra.py": "h", "_vendor/same.py": "h", "_vendor/edit.py": "new"}

    problems = verify_vendor.compare(expected, actual)

    assert problems == [
        "MISSING   _vendor/miss.py (in manifest, not on disk)",
        "UNEXPECTED _vendor/extra.py (on disk, not in manifest)",
        "MISMATCH  _vendor/edit.py (hash differs from manifest)",
    ]


# --- main ------------------------------------------------------------------


def _point_module_at(monkeypatch, tmp_path, files, manifest_files=..., write_manifest=True):
    """Redirect the module's VENDOR_DIR/MANIFEST globals at a throwaway tree.

    `files` is written under a fresh `_vendor/`. `manifest_files` defaults to the
    exact tree just written (a clean, intact copy); pass an explicit dict to
    simulate drift, or leave the manifest unwritten to test the missing case.
    """
    vendor = tmp_path / "_vendor"
    vendor.mkdir()
    for rel, data in files.items():
        path = vendor / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
    monkeypatch.setattr(verify_vendor, "VENDOR_DIR", vendor)

    manifest = tmp_path / "vendor.lock"
    monkeypatch.setattr(verify_vendor, "MANIFEST", manifest)
    if write_manifest:
        expected = verify_vendor.hash_tree(vendor) if manifest_files is ... else manifest_files
        manifest.write_text(
            json.dumps({"pytest_version": "9.1.1", "files": expected}),
            encoding="utf-8",
        )
    return vendor, manifest


def test_main_returns_zero_when_tree_matches_manifest(monkeypatch, tmp_path, capsys):
    _point_module_at(monkeypatch, tmp_path, {"a.py": b"k\n", "pkg/b.py": b"x\n"})

    assert verify_vendor.main() == 0
    out = capsys.readouterr().out
    assert "2 files verified against vendor.lock" in out
    assert "9.1.1" in out


def test_main_returns_one_and_reports_drift_when_hash_differs(monkeypatch, tmp_path, capsys):
    _point_module_at(
        monkeypatch,
        tmp_path,
        {"a.py": b"tampered\n"},
        manifest_files={"_vendor/a.py": "some-old-hash"},
    )

    assert verify_vendor.main() == 1
    err = capsys.readouterr().err
    assert "INTEGRITY CHECK FAILED" in err
    assert "MISMATCH  _vendor/a.py" in err


def test_main_returns_one_when_manifest_absent(monkeypatch, tmp_path, capsys):
    _point_module_at(monkeypatch, tmp_path, {"a.py": b"k\n"}, write_manifest=False)

    assert verify_vendor.main() == 1
    assert "vendor.lock not found" in capsys.readouterr().err


def test_main_returns_one_when_vendor_dir_absent(monkeypatch, tmp_path, capsys):
    # Manifest exists, but the _vendor tree it describes is gone.
    manifest = tmp_path / "vendor.lock"
    manifest.write_text(json.dumps({"pytest_version": "9.1.1", "files": {}}), encoding="utf-8")
    monkeypatch.setattr(verify_vendor, "MANIFEST", manifest)
    monkeypatch.setattr(verify_vendor, "VENDOR_DIR", tmp_path / "_vendor")

    assert verify_vendor.main() == 1
    assert "_vendor not found" in capsys.readouterr().err


def test_main_missing_manifest_takes_precedence_over_missing_vendor(monkeypatch, tmp_path, capsys):
    # Neither exists: the manifest check runs first, so that is the error shown.
    monkeypatch.setattr(verify_vendor, "MANIFEST", tmp_path / "vendor.lock")
    monkeypatch.setattr(verify_vendor, "VENDOR_DIR", tmp_path / "_vendor")

    assert verify_vendor.main() == 1
    err = capsys.readouterr().err
    assert "vendor.lock not found" in err
    assert "_vendor not found" not in err


def test_main_prints_question_mark_when_manifest_omits_version(monkeypatch, tmp_path, capsys):
    # Manifest has no `pytest_version` key -> the "?" fallback is shown.
    vendor = tmp_path / "_vendor"
    vendor.mkdir()
    (vendor / "a.py").write_bytes(b"k\n")
    monkeypatch.setattr(verify_vendor, "VENDOR_DIR", vendor)

    manifest = tmp_path / "vendor.lock"
    manifest.write_text(
        json.dumps({"files": verify_vendor.hash_tree(vendor)}),  # no pytest_version
        encoding="utf-8",
    )
    monkeypatch.setattr(verify_vendor, "MANIFEST", manifest)

    assert verify_vendor.main() == 0
    assert "vendored pytest ?:" in capsys.readouterr().out


# --- real shipped tree -----------------------------------------------------


def test_shipped_vendor_tree_matches_committed_manifest():
    """The real `_vendor/` on disk is byte-identical to `vendor.lock`.

    Every other test builds a throwaway tree; this one exercises the actual
    shipped copy so a corrupt/edited/partial vendor commit fails here before
    it ever ships.
    """
    manifest = json.loads(verify_vendor.MANIFEST.read_text(encoding="utf-8"))
    expected = manifest.get("files", {})
    actual = verify_vendor.hash_tree(verify_vendor.VENDOR_DIR)

    assert verify_vendor.compare(expected, actual) == []
