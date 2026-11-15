"""Per-test marker lookups without the node-chain walk on the common miss."""

from __future__ import annotations

from typing import Any


def closest_marker(item: Any, name: str) -> Any:
    """`item.get_closest_marker(name)`, cheaper when the item has no such mark.

    `item.keywords` holds every marker name on the item and its parents (pytest
    adds them with the markers: at collection, from `pytest.param(marks=)`, and
    in `add_marker`), so a name missing there means no marker. The membership
    check costs about half the `get_closest_marker` walk it skips. A plugin that
    appends to `own_markers` directly, bypassing `add_marker`, is not seen.
    """
    return item.get_closest_marker(name) if name in item.keywords else None
