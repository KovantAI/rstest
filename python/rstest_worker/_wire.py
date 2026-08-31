"""Wire-serialization helpers shared across the worker plugins."""


def _wire_safe(value):
    """msgpack-serializable subset of a workerinput value (xdist requires
    execnet-serializable workerinput; this is the same contract)."""
    if isinstance(value, (str, int, float, bool, type(None))):
        return value
    if isinstance(value, (list, tuple)):
        return [_wire_safe(v) for v in value]
    if isinstance(value, dict):
        return {str(k): _wire_safe(v) for k, v in value.items()}
    return None
