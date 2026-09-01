
import os
import uuid

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


class XDistHooks:
    def pytest_configure_node(self, node):
        ident = "res_%s_%s" % (node.gateway.id, uuid.uuid4().hex[:6])
        node.workerinput["resource_ident"] = ident
        _log("up:" + ident)

    def pytest_testnodedown(self, node, error):
        _log("down:" + node.workerinput["resource_ident"])


def pytest_configure(config):
    config.pluginmanager.register(XDistHooks())
