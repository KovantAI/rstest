"""`@pytest.mark.isolated`: run one test in a forked child of its worker.

Whatever the test changes in the process (module globals, `sys.modules`,
env vars, monkeypatches it forgot, threads it left running) dies with the
child, so a known polluter can stay in the parallel run instead of moving to
`@serial` (where it still pollutes, just later).

Fixture ownership is split at the fork. The parent keeps everything already
set up (the setup stack it had when the test started) and tears that down as
usual, for the next test. The child sets up and tears down only what it adds:
the test's own node, any collector it was first to enter, and every finalizer
registered after the fork, including a module- or session-scoped fixture this
test was the first to use. So nothing is finalized twice and nothing the child
created is left behind. The cost is that a wider-scoped fixture first used by
an isolated test is set up again by the next test that needs it.

Reports come back through pytest's own report serialization and are logged by
the parent, so every reporter sees them as usual. A child that dies before
reporting (segfault, `os._exit`) is reported as a failed call naming the exit.

Windows has no fork: the marker runs the test in-process there (warned once).
"""

from __future__ import annotations

import json
import os
import warnings
from typing import Any

import pytest

MARKER = "isolated"
MARKER_HELP = (
    "isolated: rstest - run this test in a forked child of its worker, so "
    "whatever state it changes in the process dies with it (Unix)"
)


class _Chain:
    """Stands in for `nextitem` in the child's teardown: the nodes the parent
    owned at the fork, which the child must leave alone."""

    def __init__(self, nodes: list[Any]) -> None:
        self._nodes = nodes

    def listchain(self) -> list[Any]:
        return self._nodes


def _hand_back_new_finalizers(state, owned: dict[Any, int]) -> None:
    """Move the finalizers registered on parent-owned nodes after the fork
    onto the outermost node the child pushed, ahead of its own, so the child's
    teardown runs them last (after the test's function-scoped ones)."""
    new_nodes = [n for n in state.stack if n not in owned]
    if not new_nodes:
        return
    moved: list[Any] = []
    for node, count in owned.items():
        finalizers = state.stack[node][0]
        moved.extend(finalizers[count:])
        del finalizers[count:]
    state.stack[new_nodes[0]][0][:0] = moved


def _run_in_child(item, nextitem, write_fd: int) -> None:
    """The child's side: the test's whole protocol, its own teardown only,
    the serialized reports written to `write_fd`. Never returns."""
    from _pytest.runner import runtestprotocol

    code = 0
    try:
        state = item.session._setupstate
        owned = {node: len(fins) for node, (fins, _) in state.stack.items()}
        chain = _Chain(list(owned))
        teardown_exact = state.teardown_exact

        def own_teardown(_nextitem):
            _hand_back_new_finalizers(state, owned)
            teardown_exact(chain)

        state.teardown_exact = own_teardown
        reports = runtestprotocol(item, log=False, nextitem=nextitem)
        hook = item.config.hook
        data = [hook.pytest_report_to_serializable(config=item.config, report=r) for r in reports]
        payload = json.dumps(data, default=repr).encode()
        view = memoryview(payload)
        while view:
            view = view[os.write(write_fd, view) :]
    except BaseException:
        code = 1
    finally:
        os._exit(code)


def _crash_report(item, status: int):
    """A failed call report for a child that died without reporting."""
    from _pytest.reports import TestReport

    if os.WIFSIGNALED(status):
        how = f"was killed by signal {os.WTERMSIG(status)}"
    else:
        how = f"exited with code {os.WEXITSTATUS(status)}"
    msg = f"isolated test crashed: its child process {how} before reporting"
    return TestReport(
        item.nodeid,
        item.location,
        {},
        "failed",
        msg,
        "call",
        duration=0.0,
    )


def _parent_teardown(item, nextitem):
    """Tear down the parent's own stack for the next test, as the protocol
    would have; a fixture error there is the isolated test's teardown error."""
    from _pytest.reports import TestReport
    from _pytest.runner import CallInfo

    state = item.session._setupstate
    if item.session.shouldfail or item.session.shouldstop:
        nextitem = None
    call = CallInfo.from_call(lambda: state.teardown_exact(nextitem), when="teardown")
    if call.excinfo is None:
        return None
    return TestReport.from_item_and_call(item, call)


class IsolatedTests:
    """Runs `@pytest.mark.isolated` tests forked (see the module docstring)."""

    def __init__(self) -> None:
        self._warned = False

    def pytest_configure(self, config):
        config.addinivalue_line("markers", MARKER_HELP)

    @pytest.hookimpl(tryfirst=True)
    def pytest_runtest_protocol(self, item, nextitem):
        if item.get_closest_marker(MARKER) is None:
            return None
        if not hasattr(os, "fork"):
            if not self._warned:
                self._warned = True
                warnings.warn(
                    "rstest: @pytest.mark.isolated needs fork(); on this platform "
                    "isolated tests run in their worker as usual",
                    stacklevel=1,
                )
            return None
        ihook = item.ihook
        ihook.pytest_runtest_logstart(nodeid=item.nodeid, location=item.location)
        read_fd, write_fd = os.pipe()
        pid = os.fork()
        if pid == 0:
            os.close(read_fd)
            _run_in_child(item, nextitem, write_fd)
        os.close(write_fd)
        chunks = []
        with os.fdopen(read_fd, "rb") as r:
            while chunk := r.read(65536):
                chunks.append(chunk)
        _, status = os.waitpid(pid, 0)
        hook = item.config.hook
        if chunks:
            reports = [
                hook.pytest_report_from_serializable(config=item.config, data=d)
                for d in json.loads(b"".join(chunks))
            ]
        else:
            reports = [_crash_report(item, status)]
        torn = _parent_teardown(item, nextitem)
        if torn is not None:
            reports.append(torn)
        for report in reports:
            ihook.pytest_runtest_logreport(report=report)
        ihook.pytest_runtest_logfinish(nodeid=item.nodeid, location=item.location)
        return True
