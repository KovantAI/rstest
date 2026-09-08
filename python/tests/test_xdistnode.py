"""Unit tests for the xdist node shims and hook-argument plumbing."""

import pytest
from rstest_worker._internal import xdistnode
from rstest_worker._internal.xdistnode import (
    _call_node_impl,
    _node_impl_params,
    _XdistGatewayShim,
    _XdistNodeShim,
)


def _no_signature(_impl):
    # Mimic a C builtin / un-introspectable callable.
    raise ValueError("no signature found for builtin")


def test_node_impl_params_introspectable():
    def hook(node, error=None):
        return None

    params = _node_impl_params(hook)
    assert params is not None
    assert set(params) == {"node", "error"}


def test_node_impl_params_cached():
    def hook(node):
        return None

    first = _node_impl_params(hook)
    second = _node_impl_params(hook)
    assert first is second  # cached by underlying function identity


def test_node_impl_params_uninspectable_returns_none_and_caches():
    # A C builtin has no introspectable signature -> None. `range` reliably
    # raises ValueError from inspect.signature. The None result is cached, so a
    # second call returns it from the cache (not recomputed).
    assert _node_impl_params(range) is None
    assert _node_impl_params(range) is None


def test_call_node_impl_cfunc_passes_node_only(monkeypatch):
    # No introspectable signature -> the impl is called with node ONLY; extra
    # kwargs (error=) are dropped because we can't tell which the impl accepts.
    monkeypatch.setattr(xdistnode.inspect, "signature", _no_signature)
    seen = {}

    def hook(node, error=None):
        seen["node"] = node
        seen["error"] = error
        return "ok"

    result = _call_node_impl(hook, "N", error="boom")
    assert result == "ok"
    assert seen == {"node": "N", "error": None}


def test_call_node_impl_cfunc_one_arg_impl_runs_once(monkeypatch):
    # C-hook path, one-arg impl: called impl(node) directly (kwargs dropped),
    # so the one-arg hook binds and runs its body exactly once.
    monkeypatch.setattr(xdistnode.inspect, "signature", _no_signature)
    calls = []

    def hook(node):
        calls.append(node)
        return "ok"

    result = _call_node_impl(hook, "N", error="boom")
    assert result == "ok"
    assert calls == ["N"]  # entered once (the retry), not double-executed


def test_call_node_impl_cfunc_reraises_when_impl_body_raises(monkeypatch):
    # C-hook path: impl(node) runs and its body raises TypeError. The error
    # propagates and the body runs exactly once (no retry).
    monkeypatch.setattr(xdistnode.inspect, "signature", _no_signature)
    calls = []

    def hook(node, error=None):
        calls.append(node)
        raise TypeError("from inside the body")

    with pytest.raises(TypeError, match="from inside the body"):
        _call_node_impl(hook, "N", error="boom")
    assert calls == ["N"]  # ran once, no double-execution


def test_call_node_impl_drops_unaccepted_kwargs():
    # The common one-arg (node) form: error= must be dropped, not raised.
    seen = {}

    def hook(node):
        seen["node"] = node
        return "ok"

    result = _call_node_impl(hook, "N", error=RuntimeError("x"))
    assert result == "ok"
    assert seen["node"] == "N"


def test_call_node_impl_passes_declared_kwargs():
    seen = {}

    def hook(node, error=None):
        seen["node"] = node
        seen["error"] = error

    _call_node_impl(hook, "N", error="boom")
    assert seen == {"node": "N", "error": "boom"}


def test_call_node_impl_var_keyword_gets_everything():
    seen = {}

    def hook(**kwargs):
        seen.update(kwargs)

    _call_node_impl(hook, "N", error="boom")
    assert seen == {"node": "N", "error": "boom"}


def test_call_node_impl_positional_only_node():
    seen = {}

    def hook(node, /, error=None):
        seen["node"] = node
        seen["error"] = error

    _call_node_impl(hook, "N", error="boom")
    assert seen == {"node": "N", "error": "boom"}


def test_gateway_shim():
    assert _XdistGatewayShim("gw3").id == "gw3"


def test_node_shim_exposes_workerinput_and_gateway():
    class Config:
        def __init__(self):
            self.workerinput = {"workerid": "gw1"}

    shim = _XdistNodeShim(Config(), "gw1")
    assert shim.workerinput == {"workerid": "gw1"}
    assert shim.gateway.id == "gw1"
    assert isinstance(shim.config, Config)
