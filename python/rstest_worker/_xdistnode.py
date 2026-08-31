"""xdist master-side node plumbing: shims standing in for xdist's
WorkerController, plus the introspection needed to invoke node hooks
(pytest_testnodeready / pytest_testnodedown) with the right arguments."""

import contextlib
import inspect
from collections.abc import Mapping
from typing import Any

_Params = Mapping[str, inspect.Parameter]

# Signature introspection is stable per underlying function; cache it so a
# hook fired once per worker teardown doesn't re-parse every invocation.
_NODE_IMPL_PARAMS: dict[Any, _Params | None] = {}


def _node_impl_params(impl: Any) -> _Params | None:
    """Return the impl's parameter mapping, or None when its signature isn't
    introspectable (builtins / C hooks). Cached by the underlying function."""
    key = getattr(impl, "__func__", impl)
    try:
        return _NODE_IMPL_PARAMS[key]
    except (KeyError, TypeError):
        # Unhashable key: fall through and compute without caching.
        pass
    try:
        params: _Params | None = inspect.signature(impl).parameters
    except (ValueError, TypeError):
        params = None
    # Unhashable key: skip the cache write, still return the computed params.
    with contextlib.suppress(TypeError):
        _NODE_IMPL_PARAMS[key] = params
    return params


def _call_node_impl(impl: Any, node: Any, **kwargs: Any) -> Any:
    """Invoke an xdist node hook (pytest_testnodeready / _testnodedown),
    passing only the keyword args the impl declares. Real-world hooks often use
    the one-arg ``(node)`` form, so dropping unaccepted kwargs (e.g. ``error=``)
    avoids TypeError; a ``**kwargs`` param accepts everything.

    ``node`` goes by keyword when introspectable (pluggy names the param
    ``node``, and a ``(**kwargs)`` impl can't take it positionally), but
    positionally for positional-only params and un-introspectable C hooks."""
    params = _node_impl_params(impl)
    if params is None:
        # No introspectable signature (builtin / C hook). Pass node
        # positionally (C funcs often reject keywords), then retry node-only if
        # the extra kwargs don't bind (a one-arg impl would TypeError on error=).
        try:
            return impl(node, **kwargs)
        except TypeError as exc:
            # Retry ONLY when the impl was never entered (arg-binding failure:
            # no inner traceback frame). If the impl body itself raised, its
            # side effects already ran, so a retry would double-execute them.
            tb = exc.__traceback__
            if tb is not None and tb.tb_next is not None:
                raise
            return impl(node)
    if any(p.kind is inspect.Parameter.VAR_KEYWORD for p in params.values()):
        return impl(node=node, **kwargs)
    accepted = {k: v for k, v in kwargs.items() if k in params}
    node_param = params.get("node")
    if node_param is not None and node_param.kind is inspect.Parameter.POSITIONAL_ONLY:
        return impl(node, **accepted)
    return impl(node=node, **accepted)


class _XdistGatewayShim:
    def __init__(self, gid: str) -> None:
        self.id = gid


class _XdistNodeShim:
    """Stands in for xdist's WorkerController in master-side hooks.

    Implementations touch node.workerinput (filled per worker),
    node.gateway.id, and occasionally node.config.
    """

    def __init__(self, config: Any, worker_id: str) -> None:
        self.config = config
        self.workerinput = config.workerinput
        self.gateway = _XdistGatewayShim(worker_id)
