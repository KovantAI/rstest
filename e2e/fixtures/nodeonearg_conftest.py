
import os

import pytest


def pytest_addhooks(pluginmanager):
    class XdistSpecs:
        @pytest.hookspec
        def pytest_configure_node(self, node): ...

        @pytest.hookspec
        def pytest_testnodedown(self, node, error): ...

    pluginmanager.add_hookspecs(XdistSpecs)


def _log(line):
    path = os.environ.get("NODE_HOOK_LOG")
    if path:
        with open(path + "." + str(os.getpid()), "a") as f:
            f.write(line + "\n")


class OneArgHooks:
    def pytest_configure_node(self, node):
        node.workerinput["oa_ident"] = "oa_" + node.gateway.id

    # ONE arg, no `error` - the pytest-html / pytest-metadata signature.
    def pytest_testnodedown(self, node):
        _log("down:" + node.workerinput["oa_ident"])


def pytest_configure(config):
    config.pluginmanager.register(OneArgHooks())
