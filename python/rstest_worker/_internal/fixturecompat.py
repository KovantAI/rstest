"""Restore tree-independent conftest fixture visibility for lazy collection.

rstest's D5 lazy collection calls ``Session.perform_collect([file])`` once per
FILE on one long-lived Session. Vendored pytest 9.2 parses a conftest's
fixtures once and files each ``FixtureDef`` under the ``Directory`` node it saw
first (``FixtureManager._arg2node2fixturedefs``); ``getfixturedefs`` then
looks them up by NODE IDENTITY (``node2fixturedefs.get(parent)`` for each of
the item's parents).

Each ``perform_collect`` builds a FRESH tree, so a second file under the same
conftest directory gets a new ``Directory`` node; the old one isn't in its
parent chain and the fixture is reported "not found". This bites only when one
worker collects 2+ files sharing a conftest directory (file affinity, or a
stolen/redistributed file), surfacing as a low-rate flake.

The fix falls back to the parent's NODEID when the identity lookup finds
nothing: a stale node with the same nodeid is the same directory from an
earlier tree. Within one tree the identity lookup always hits first, so the
fallback is a no-op there, and the single cached ``FixtureDef`` is kept
(unlike re-parsing, which would re-run session fixtures).

The vendored pytest copy must stay byte-identical to upstream (see
python/VENDOR.md), so the fix is a narrow monkeypatch installed at worker import.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

from _pytest import fixtures as _fixtures

if TYPE_CHECKING:
    from collections.abc import Iterable, Mapping, Sequence

    from _pytest.fixtures import FixtureDef
    from _pytest.nodes import Node


def _defs_at(
    node2fixturedefs: Mapping[Node, Sequence[FixtureDef[Any]]], parent: Node
) -> Sequence[FixtureDef[Any]]:
    """The FixtureDefs filed under ``parent``, or under a node from an earlier
    collection tree with the same nodeid."""
    found = node2fixturedefs.get(parent)
    if found:
        return found
    return [
        fixturedef
        for registered, fixturedefs in node2fixturedefs.items()
        if registered is not parent and registered.nodeid == parent.nodeid
        for fixturedef in fixturedefs
    ]


def getfixturedefs(self, argname: str, node: Node) -> Sequence[FixtureDef[Any]] | None:
    # pytest 9.2's FixtureManager.getfixturedefs, with `_defs_at` for the
    # node-keyed lookup.
    node2fixturedefs = self._arg2node2fixturedefs.get(argname, {})
    nodeid2fixturedefs = self._arg2nodeid2fixturedefs.get(argname, {})
    if not node2fixturedefs and not nodeid2fixturedefs:
        return None
    fixturedefs = [
        fixturedef
        for parent in node.iter_parents()
        for fixturedef in [
            *_defs_at(node2fixturedefs, parent),
            *nodeid2fixturedefs.get(parent.nodeid, ()),
        ]
    ]
    fixturedefs.reverse()
    return fixturedefs


def _get_all_fixture_defs_for_node(self, node: Node) -> Iterable[FixtureDef[Any]]:
    # pytest 9.2's version (the "available fixtures" list of a lookup error),
    # with the same fallback.
    for node2fixturedefs in self._arg2node2fixturedefs.values():
        for parent in node.iter_parents():
            yield from _defs_at(node2fixturedefs, parent)
    for nodeid2fixturedefs in self._arg2nodeid2fixturedefs.values():
        for parent in node.iter_parents():
            yield from nodeid2fixturedefs.get(parent.nodeid, ())


def install() -> None:
    """Idempotently patch FixtureManager's node-keyed fixture lookups."""
    if getattr(_fixtures.FixtureManager.getfixturedefs, "_rstest_patched", False):
        return
    # Idempotency marker stamped on the replacement function (dynamic attr).
    getfixturedefs._rstest_patched = True  # ty: ignore[unresolved-attribute]
    _fixtures.FixtureManager.getfixturedefs = getfixturedefs
    _fixtures.FixtureManager._get_all_fixture_defs_for_node = _get_all_fixture_defs_for_node
