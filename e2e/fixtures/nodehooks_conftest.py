
import os

import pytest


def pytest_addhooks(pluginmanager):
    # Real suites get these specs from pytest-xdist; declare them the
    # same way so this fixture is hermetic.
    class XdistSpecs:
        @pytest.hookspec
        def pytest_configure_node(self, node): ...

        @pytest.hookspec
        def pytest_testnodeready(self, node): ...

        @pytest.hookspec
        def pytest_testnodedown(self, node, error): ...

    pluginmanager.add_hookspecs(XdistSpecs)


def _log(line):
    path = os.environ.get("NODE_HOOK_LOG")
    if path:
        with open(path + "." + str(os.getpid()), "a") as f:
            f.write(line + "\n")


class XDistHooks:
    # the sqlalchemy pattern: master fills workerinput per node
    def pytest_configure_node(self, node):
        node.workerinput["follower_ident"] = "follower_" + node.gateway.id

    def pytest_testnodeready(self, node):
        _log("ready:" + node.gateway.id)

    def pytest_testnodedown(self, node, error):
        _log("down:" + node.workerinput["follower_ident"])


def pytest_configure(config):
    config.pluginmanager.register(XDistHooks())
    # read it back IMMEDIATELY (sqlalchemy does exactly this): only a
    # synchronous configure_node call at registration time satisfies it
    if hasattr(config, "workerinput"):
        config._follower_ident = config.workerinput["follower_ident"]
