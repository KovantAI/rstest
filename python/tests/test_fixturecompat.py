"""Unit tests for the lazy-collection fixture-visibility monkeypatch."""

from types import SimpleNamespace
from typing import Any, cast

from _pytest import fixtures as _fixtures
from rstest_worker._internal import fixturecompat


class _Node:
    """Hashable node stand-in: the patched lookups key dicts by node."""

    def __init__(self, nodeid, parents=()):
        self.nodeid = nodeid
        self._parents = list(parents)

    def iter_parents(self):
        return list(self._parents)


def _manager(node2=None, nodeid2=None):
    return SimpleNamespace(
        _arg2node2fixturedefs=node2 or {},
        _arg2nodeid2fixturedefs=nodeid2 or {},
    )


def _lookup(fm, argname, node):
    return fixturecompat.getfixturedefs(cast(Any, fm), argname, cast(Any, node))


def test_getfixturedefs_matches_by_node_identity():
    parent = _Node("tests")
    node = _Node("tests/t.py::a", [parent])
    fd = SimpleNamespace(argname="db")
    fm = _manager({"db": {parent: [fd]}})
    assert _lookup(fm, "db", node) == [fd]


def test_getfixturedefs_falls_back_to_a_stale_node_with_the_same_nodeid():
    # Fresh tree: the conftest's Directory node is a new object, but the
    # FixtureDef is still filed under the old one with the same nodeid.
    stale = _Node("tests")
    node = _Node("tests/t.py::a", [_Node("tests")])
    fd = SimpleNamespace(argname="db")
    fm = _manager({"db": {stale: [fd]}})
    assert _lookup(fm, "db", node) == [fd]


def test_getfixturedefs_prefers_identity_over_a_stale_twin():
    parent = _Node("tests")
    stale = _Node("tests")
    current, old = SimpleNamespace(argname="db"), SimpleNamespace(argname="db")
    fm = _manager({"db": {parent: [current], stale: [old]}})
    assert _lookup(fm, "db", _Node("tests/t.py::a", [parent])) == [current]


def test_getfixturedefs_orders_least_specific_first_and_keeps_nodeid_keys():
    root, sub = _Node(""), _Node("tests")
    node = _Node("tests/t.py::a", [sub, root])
    outer, inner, legacy = (SimpleNamespace(argname="db") for _ in range(3))
    fm = _manager(
        {"db": {_Node(""): [outer], _Node("tests"): [inner]}}, {"db": {"tests": [legacy]}}
    )
    assert _lookup(fm, "db", node) == [outer, legacy, inner]


def test_getfixturedefs_skips_unrelated_and_reports_unknown_names():
    node = _Node("tests/t.py::a", [_Node("tests")])
    fm = _manager({"db": {_Node("other"): [SimpleNamespace(argname="db")]}})
    assert _lookup(fm, "db", node) == []
    assert _lookup(fm, "missing", node) is None


def test_all_fixture_defs_for_node_uses_the_same_fallback():
    node = _Node("tests/t.py", [_Node("tests")])
    fd = SimpleNamespace(argname="db")
    fm = _manager({"db": {_Node("tests"): [fd]}})
    got = list(fixturecompat._get_all_fixture_defs_for_node(cast(Any, fm), cast(Any, node)))
    assert got == [fd]


def test_install_patches_and_is_idempotent():
    originals = (
        _fixtures.FixtureManager.getfixturedefs,
        _fixtures.FixtureManager._get_all_fixture_defs_for_node,
    )
    try:
        fixturecompat.install()
        patched = _fixtures.FixtureManager.getfixturedefs
        assert getattr(patched, "_rstest_patched", False) is True
        assert (
            _fixtures.FixtureManager._get_all_fixture_defs_for_node
            is fixturecompat._get_all_fixture_defs_for_node
        )
        # Second call is a no-op: same function object, no re-wrap.
        fixturecompat.install()
        assert _fixtures.FixtureManager.getfixturedefs is patched
    finally:
        (
            _fixtures.FixtureManager.getfixturedefs,
            _fixtures.FixtureManager._get_all_fixture_defs_for_node,
        ) = originals
