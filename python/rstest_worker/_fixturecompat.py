"""Restore tree-independent conftest fixture visibility for lazy collection.

rstest's D5 lazy collection (LazyDispatchPlugin) calls
``Session.perform_collect([file])`` once per assigned FILE, reusing a single
long-lived Session so session-scope fixtures survive across files. pytest's
own collection model assumes ONE collection pass building ONE tree; vendored
pytest 9.1's fixture visibility leans on that assumption:

  * A conftest's fixtures are parsed exactly once, when that conftest's
    ``Directory`` collector is first collected (``pytest_make_collect_report``
    pops the conftest from ``_pending_conftests``). The resulting ``FixtureDef``
    is bound to *that* ``Directory`` node instance via ``FixtureDef.node``.
  * ``FixtureManager._matchfactories`` then matches a fixture to an item by
    NODE IDENTITY: ``fixturedef.node in item.iter_parents()``.

Each ``perform_collect`` builds a FRESH collection tree, so the SECOND (and
later) file collected under the same conftest directory gets a brand-new
``Directory`` node instance. The conftest's ``FixtureDef.node`` still points at
the FIRST tree's ``Directory``, which is not in the second file's parent chain,
so node-identity matching fails and the fixture is reported "not found".

This only bites when one worker collects two+ files sharing a conftest
directory in separate ``perform_collect`` calls (e.g. file affinity hands both
files to one worker, or a stolen/redistributed file is re-collected) — which is
why it surfaced as a low-rate flake (gate "lazy: session fixture once per
worker").

``FixtureDef.baseid`` is always set to the defining node's nodeid (a string
prefix), and string-prefix matching is tree-INDEPENDENT and exactly the legacy
pytest visibility rule. It is also a strict superset of node-identity matching
within a single tree (``node in parents`` implies ``node.nodeid in
parentnodeids``), so OR-ing it in is a no-op for the normal single-pass case
and a correct fix for the repeated-collection case. Crucially it keeps the
conftest's single ``FixtureDef`` (and its cached session-scope result) intact,
unlike re-parsing which would re-run session fixtures.

The vendored pytest copy must stay byte-identical to upstream (see
python/VENDOR.md), so the fix lives here as a narrow monkeypatch installed once
at worker import.
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
