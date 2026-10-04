"""Reruns and the -x / --maxfail count inside one pytest session.

`flaky_reruns` reads an item's `@pytest.mark.flaky` budget the way
pytest-rerunfailures does. The pool workers ship it to the orchestrator,
which owns retries there; `FlakyReruns` runs the retries itself in the
single session (`-n 0` / `-n 1`), where no orchestrator loop exists.

`MaxfailExemptions` keeps pytest's own `-x` / `--maxfail` count from
reacting to failures that do not fail the run in the end: a quarantined
test's failure (`--quarantine`, demoted after the run), and in a pool worker
any attempt the orchestrator may still retry (`--reruns`, a flaky mark). In a
pool the orchestrator counts final failures and stops every worker itself.
"""

from __future__ import annotations

import contextlib
import os
import platform
import re
import sys

import pytest


def _condition_holds(item, condition) -> bool:
    """rerunfailures' `condition=`: a bool, or a string evaluated with `os`,
    `sys`, `platform`, `config` and the test module's globals. A string that
    fails to evaluate keeps the reruns (rstest never fails a test over it)."""
    if not isinstance(condition, str):
        return bool(condition)
    namespace = {"os": os, "sys": sys, "platform": platform, "config": item.config}
    obj = getattr(item, "obj", None)
    namespace.update(getattr(obj, "__globals__", {}))
    # Same contract as pytest's own `skipif("...")` strings and rerunfailures:
    # the expression is the test author's code, evaluated like the test itself.
    try:
        return bool(eval(compile(condition, "<flaky condition>", "eval"), namespace))
    except Exception:
        return True


def flaky_reruns(item) -> int:
    """The rerun budget of `item`'s `@pytest.mark.flaky` mark, 0 without one.

    Mirrors pytest-rerunfailures: the `reruns=` keyword, else the first
    positional argument (`flaky(3)`), else 1; 0 when `condition=` is false.
    """
    mark = item.get_closest_marker("flaky")
    if mark is None:
        return 0
    kwargs = getattr(mark, "kwargs", {})
    args = getattr(mark, "args", ())
    if "reruns" in kwargs:
        count = kwargs["reruns"]
    elif args:
        count = args[0]
    else:
        count = 1
    try:
        count = int(count)
    except (TypeError, ValueError):
        return 0
    if count <= 0:
        return 0
    if "condition" in kwargs and not _condition_holds(item, kwargs["condition"]):
        return 0
    return count


def _quarantine_patterns() -> list[re.Pattern]:
    """The `--quarantine` patterns the orchestrator passes in
    RSTEST_QUARANTINE: one anchored regex per line (its `*` globs already
    translated). Unparsable lines are skipped."""
    out = []
    for line in os.environ.get("RSTEST_QUARANTINE", "").splitlines():
        if not line:
            continue
        try:
            out.append(re.compile(line))
        except re.error:
            continue
    return out


class MaxfailExemptions:
    """Undo pytest's maxfail bookkeeping for failures that must not count.

    pytest's Session counts every failed report in `testsfailed` and sets
    `shouldfail` once the count reaches `--maxfail`. This wrapper runs around
    that hook: for an exempt report it lifts the limit and restores the count,
    so `-x` trips on the first failure that really fails the run. `pool`: also exempt every attempt
    the orchestrator may retry (global `--reruns`, or a flaky mark).
    """

    def __init__(self, pool: bool) -> None:
        self._pool = pool
        self._reruns = pool and os.environ.get("RSTEST_RERUNS") == "1"
        self._quarantine = _quarantine_patterns()
        self._item = None

    def active(self) -> bool:
        return self._pool or bool(self._quarantine)

    @pytest.hookimpl(wrapper=True)
    def pytest_runtest_protocol(self, item, nextitem):
        self._item = item
        try:
            return (yield)
        finally:
            self._item = None

    def _exempt(self, report) -> bool:
        nodeid = report.nodeid
        if any(p.match(nodeid) for p in self._quarantine):
            return True
        if not self._pool:
            return False
        if self._reruns:
            return True
        item = self._item
        return item is not None and item.nodeid == nodeid and flaky_reruns(item) > 0

    @pytest.hookimpl(wrapper=True)
    def pytest_runtest_logreport(self, report):
        session = getattr(self._item, "session", None)
        option = getattr(getattr(session, "config", None), "option", None)
        if (
            session is None
            or option is None
            or not getattr(option, "maxfail", 0)
            or not report.failed
            or not self._exempt(report)
        ):
            return (yield)
        # pytest refuses to unset `shouldfail`, so keep it from being set:
        # the Session reads maxfail from the option at each report.
        failed, maxfail = session.testsfailed, getattr(option, "maxfail", 0)
        option.maxfail = 0
        try:
            return (yield)
        finally:
            option.maxfail = maxfail
            session.testsfailed = failed


def _forget_failed_setup(item) -> None:
    """Before a rerun: tear down every setup-stack node from the first one
    whose setup failed (pytest would re-raise its cached error instead of
    setting it up again), and drop fixture results cached as errors."""
    state = item.session._setupstate
    nodes = list(state.stack)
    failed = [i for i, node in enumerate(nodes) if state.stack[node][1] is not None]
    if failed:
        for node in reversed(nodes[failed[0] :]):
            finalizers, _ = state.stack.pop(node)
            while finalizers:
                fin = finalizers.pop()
                # Best effort: the node's setup already failed, and the rerun
                # reports whatever goes wrong when it sets the node up again.
                with contextlib.suppress(Exception):
                    fin()
    info = getattr(item, "_fixtureinfo", None)
    for defs in getattr(info, "name2fixturedefs", {}).values():
        for fixturedef in defs:
            cached = getattr(fixturedef, "cached_result", None)
            if cached is not None and cached[2] is not None:
                fixturedef.cached_result = None


class FlakyReruns:
    """`@pytest.mark.flaky` reruns in the single session (no pool).

    The pytest-rerunfailures protocol: a failed attempt with budget left is
    reported with outcome `rerun` (shown as `R`, counted as `N rerun`, never as
    a failure), and the test runs again; the last attempt reports as usual.
    Steps aside when pytest-rerunfailures itself is installed: it then owns
    the mark, as it does under plain pytest.
    """

    @pytest.hookimpl(trylast=True)
    def pytest_configure(self, config):
        if config.pluginmanager.hasplugin("rerunfailures"):
            config.pluginmanager.unregister(self)

    @pytest.hookimpl(tryfirst=True)
    def pytest_runtest_protocol(self, item, nextitem):
        reruns = flaky_reruns(item)
        if reruns <= 0:
            return None
        from _pytest.runner import runtestprotocol

        for attempt in range(reruns + 1):
            item.ihook.pytest_runtest_logstart(nodeid=item.nodeid, location=item.location)
            reports = runtestprotocol(item, nextitem=nextitem, log=False)
            retry = False
            for report in reports:
                if attempt < reruns and report.failed and not hasattr(report, "wasxfail"):
                    report.outcome = "rerun"
                    item.ihook.pytest_runtest_logreport(report=report)
                    retry = True
                    break
                item.ihook.pytest_runtest_logreport(report=report)
            if not retry:
                break
            _forget_failed_setup(item)
        item.ihook.pytest_runtest_logfinish(nodeid=item.nodeid, location=item.location)
        return True

    @pytest.hookimpl(tryfirst=True)
    def pytest_report_teststatus(self, report):
        if report.outcome == "rerun":
            return "rerun", "R", ("RERUN", {"yellow": True})
        return None
