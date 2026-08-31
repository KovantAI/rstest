"""Restore tree-independent conftest fixture visibility for lazy collection.

rstest's D5 lazy collection calls ``Session.perform_collect([file])`` once per
FILE on one long-lived Session. Vendored pytest 9.1 parses a conftest's
fixtures once, binding each ``FixtureDef`` to the first ``Directory`` node it
saw, and ``_matchfactories`` then matches by NODE IDENTITY
(``fixturedef.node in item.iter_parents()``).

Each ``perform_collect`` builds a FRESH tree, so a second file under the same
conftest directory gets a new ``Directory`` node; the old ``FixtureDef.node``
isn't in its parent chain and the fixture is reported "not found". This bites
only when one worker collects 2+ files sharing a conftest directory (file
affinity, or a stolen/redistributed file), surfacing as a low-rate flake.

``FixtureDef.baseid`` (the defining node's nodeid, a string prefix) matches
tree-independently and is a superset of node-identity within one tree, so
OR-ing it in is a no-op for the single-pass case and a fix for the repeated
case, keeping the single cached ``FixtureDef`` (unlike re-parsing, which would
re-run session fixtures).

The vendored pytest copy must stay byte-identical to upstream (see
python/VENDOR.md), so the fix is a narrow monkeypatch installed at worker import.
"""

from __future__ import annotations

from _pytest import fixtures as _fixtures


def _matchfactories(self, fixturedefs, node):
    parent_nodes = set(node.iter_parents())
    parentnodeids = {n.nodeid for n in parent_nodes}
    for fixturedef in fixturedefs:
        if fixturedef.node is not None and fixturedef.node in parent_nodes:
            # Node-identity match (fast path, single collection tree).
            yield fixturedef
        elif fixturedef.baseid is not None and fixturedef.baseid in parentnodeids:
            # Tree-independent baseid (string-prefix) match: survives the
            # fresh Directory nodes that lazy per-file perform_collect creates.
            yield fixturedef


def install() -> None:
    """Idempotently patch FixtureManager._matchfactories."""
    if getattr(_fixtures.FixtureManager._matchfactories, "_rstest_patched", False):
        return
    _matchfactories._rstest_patched = True
    _fixtures.FixtureManager._matchfactories = _matchfactories
