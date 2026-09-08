"""Unit tests for the lazy-collection fixture-visibility monkeypatch."""

from types import SimpleNamespace
from typing import Any, cast

from _pytest import fixtures as _fixtures
from rstest_worker._internal import fixturecompat


class _Node:
    """Hashable node stand-in (SimpleNamespace defines __eq__ -> unhashable,
    but _matchfactories puts parents in a set)."""

    def __init__(self, nodeid, parents=()):
        self.nodeid = nodeid
        self._parents = list(parents)

    def iter_parents(self):
        return list(self._parents)


def _node(nodeid, parents):
    return _Node(nodeid, parents)


def test_matchfactories_yields_on_node_identity_match():
    parent = _node("tests/", [])
    node = _node("tests/t.py::a", [parent])
    fd = SimpleNamespace(node=parent, baseid=None)
    out = list(fixturecompat._matchfactories(None, cast(Any, [fd]), node))
    assert out == [fd]


def test_matchfactories_yields_on_baseid_prefix_match():
    # Fresh tree: the FixtureDef.node identity differs, but its baseid string
    # still matches a parent's nodeid -> the fix keeps it visible.
    parent = _node("tests/", [])
    node = _node("tests/t.py::a", [parent])
    stale = _node("tests/", [])  # different object, same nodeid
    fd = SimpleNamespace(node=stale, baseid="tests/")
    out = list(fixturecompat._matchfactories(None, cast(Any, [fd]), node))
    assert out == [fd]


def test_matchfactories_skips_unrelated_fixture():
    node = _node("tests/t.py::a", [_node("tests/", [])])
    fd = SimpleNamespace(node=_node("other/", []), baseid="other/")
    assert list(fixturecompat._matchfactories(None, cast(Any, [fd]), node)) == []


def test_matchfactories_ignores_none_node_and_baseid():
    node = _node("tests/t.py::a", [_node("tests/", [])])
    fd = SimpleNamespace(node=None, baseid=None)
    assert list(fixturecompat._matchfactories(None, cast(Any, [fd]), node)) == []


def test_install_patches_and_is_idempotent():
    original = _fixtures.FixtureManager._matchfactories
    try:
        fixturecompat.install()
        patched = _fixtures.FixtureManager._matchfactories
        assert getattr(patched, "_rstest_patched", False) is True
        # Second call is a no-op: same function object, no re-wrap.
        fixturecompat.install()
        assert _fixtures.FixtureManager._matchfactories is patched
    finally:
        _fixtures.FixtureManager._matchfactories = original
