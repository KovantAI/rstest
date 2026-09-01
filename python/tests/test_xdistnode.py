"""Unit tests for the xdist node shims and hook-argument plumbing."""

from rstest_worker._internal.xdistnode import (
    _call_node_impl,
    _node_impl_params,
    _XdistGatewayShim,
    _XdistNodeShim,
)


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
