"""Wire-serialization helpers shared across the worker plugins."""

from __future__ import annotations

import logging
from typing import Any

log = logging.getLogger("rstest.worker")


def _wire_safe(value: Any) -> Any:
    """msgpack-serializable subset of a workerinput value (xdist requires
    execnet-serializable workerinput; this is the same contract). A value
    outside that subset is dropped to None and warned about, rather than
    silently losing plugin data."""
    if isinstance(value, (str, int, float, bool, type(None))):
        return value
    if isinstance(value, (list, tuple)):
        return [_wire_safe(v) for v in value]
    if isinstance(value, dict):
        return {str(k): _wire_safe(v) for k, v in value.items()}
    log.warning(
        "dropping non-serializable workerinput value of type %r (set to None)",
        type(value).__name__,
    )
    return None
