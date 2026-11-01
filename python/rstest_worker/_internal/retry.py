"""Reruns and the -x / --maxfail count inside one pytest session.

`flaky_reruns` reads an item's `@pytest.mark.flaky` budget the way
pytest-rerunfailures does. The pool workers ship it to the orchestrator,
which owns retries there; `FlakyReruns` runs the retries itself in the
single session (`-n 0` / `-n 1`), where no orchestrator loop exists.

`MaxfailExemptions` keeps pytest's own `-x` / `--maxfail` count from
reacting to failures that do not fail the run in the end: a quarantined
test's failure (`--quarantine`, demoted after the run; an auto-quarantined
one only while the run stays under `--auto-quarantine-max`), and in a pool worker
any attempt the orchestrator may still retry (`--reruns`, a flaky mark). In a
pool the orchestrator counts final failures and stops every worker itself.
"""

from __future__ import annotations

import contextlib
import os
import platform
import re
import sys
import time

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


def _mark_filters(mark, name: str) -> list:
    """A rerun filter keyword of the flaky mark: a regex or an exception
    class, or a list of them."""
    value = getattr(mark, "kwargs", {}).get(name)
    if not value:
        return []
    if isinstance(value, (str, type)):
        return [value]
    return list(value)


def _exception_chain(exc, follow_context: bool):
    seen = set()
    while exc is not None and id(exc) not in seen:
        seen.add(id(exc))
        yield exc
        if exc.__cause__ is not None:
            exc = exc.__cause__
        elif follow_context and not getattr(exc, "__suppress_context__", False):
            exc = exc.__context__
        else:
            exc = None


def _matches(filters: list, excinfo, follow_context: bool) -> bool:
    """rerunfailures' `_try_match_error`: an exception class matches by
    isinstance, a regex searches `Type: message`, for every exception in the
    raised one's chain."""
    if excinfo is None:
        return False
    for exc in _exception_chain(excinfo.value, follow_context):
        text = f"{type(exc).__name__}: {exc}"
        for f in filters:
            if isinstance(f, type):
                if issubclass(f, BaseException) and isinstance(exc, f):
                    return True
                continue
            # A bad pattern matches nothing rather than crashing the worker.
            with contextlib.suppress(re.error, TypeError):
                if re.search(f, text):
                    return True
    return False


def mark_rules_out_rerun(item, report, excinfo) -> bool:
    """Whether the flaky mark's own `only_rerun` / `rerun_except` keep this
    failure from being rerun (rerunfailures' `_should_hard_fail_on_error`):
    with `only_rerun`, an error matching none of its filters; with
    `rerun_except`, one matching any of them."""
    if not report.failed or hasattr(report, "wasxfail"):
        return False
    mark = item.get_closest_marker("flaky")
    if mark is None:
        return False
    only = _mark_filters(mark, "only_rerun")
    unless = _mark_filters(mark, "rerun_except")
    if not only and not unless:
        return False
    if only and not _matches(only, excinfo, follow_context=True):
        return True
    return bool(unless) and _matches(unless, excinfo, follow_context=False)


def rerun_delay(item) -> float:
    """The flaky mark's `reruns_delay` in seconds, 0 without one (a negative
    or unparsable value too, as rerunfailures treats a negative one)."""
    mark = item.get_closest_marker("flaky")
    if mark is None:
        return 0.0
    try:
        return max(float(getattr(mark, "kwargs", {}).get("reruns_delay", 0) or 0), 0.0)
    except (TypeError, ValueError):
        return 0.0


class _TagRuledOut:
    """Tag a failed report the flaky mark rules out of a rerun
    (`_rstest_no_rerun`); the exception is only reachable here."""

    _retryable = False

    @pytest.hookimpl(wrapper=True)
    def pytest_runtest_makereport(self, item, call):
        report = yield
        if report.failed and not hasattr(report, "wasxfail"):
            if mark_rules_out_rerun(item, report, call.excinfo):
                report._rstest_no_rerun = True
            else:
                self._retryable = True
        return report


class PoolMarkReruns(_TagRuledOut):
    """The flaky mark's rerun keywords in a pool worker, where the
    orchestrator owns retries: a ruled-out failure is sent as `no_rerun`,
    which the orchestrator never retries, and after an attempt that may be
    retried the worker waits the mark's `reruns_delay` before its next item."""

    def __init__(self) -> None:
        self._reruns = os.environ.get("RSTEST_RERUNS") == "1"

    @pytest.hookimpl(wrapper=True)
    def pytest_runtest_protocol(self, item, nextitem):
        self._retryable = False
        try:
            return (yield)
        finally:
            delay = rerun_delay(item)
            if delay and self._retryable and (self._reruns or flaky_reruns(item) > 0):
                # ponytail: the worker can't see the rerun budget, so it also
                # waits after the last failed attempt; an orchestrator-side
                # not-before time on the requeued item would avoid that.
                time.sleep(delay)


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


def _auto_quarantine() -> tuple[set[str], int]:
    """The auto-quarantined nodeids (RSTEST_AUTO_QUARANTINE, one per line)
    and their cap, a percent of the session's tests
    (RSTEST_AUTO_QUARANTINE_MAX)."""
    ids = {line for line in os.environ.get("RSTEST_AUTO_QUARANTINE", "").splitlines() if line}
    try:
        max_pct = int(os.environ.get("RSTEST_AUTO_QUARANTINE_MAX", ""))
    except ValueError:
        max_pct = 0
    return ids, max_pct


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
        self._auto, self._auto_max = _auto_quarantine()
        self._auto_failed: set[str] = set()
        self._held = 0
        self._item = None

    def active(self) -> bool:
        return self._pool or bool(self._quarantine) or bool(self._auto)

    @pytest.hookimpl(wrapper=True)
    def pytest_runtest_protocol(self, item, nextitem):
        self._item = item
        try:
            return (yield)
        finally:
            self._item = None

    def _exempt(self, report, session) -> bool:
        nodeid = report.nodeid
        if any(p.match(nodeid) for p in self._quarantine):
            return True
        if self._pool:
            if self._reruns:
                return True
            item = self._item
            if item is not None and item.nodeid == nodeid and flaky_reruns(item) > 0:
                return True
        if nodeid not in self._auto:
            return False
        # A pool worker sees only its share of the failures (and a lazy one
        # not even the run's total): the orchestrator holds auto failures
        # against the cap and stops every worker itself.
        return self._pool or self._hold(nodeid, session)

    def _hold(self, nodeid, session) -> bool:
        """An auto-quarantined failure: held back while the auto tests failed
        so far fit the cap (the post-run demotion still quarantines them).
        Past it none of them is quarantined, so this one counts and every
        held one is put back into pytest's count."""
        self._auto_failed.add(nodeid)
        total = getattr(session, "testscollected", 0)
        cap = max(1, -(-total * self._auto_max // 100))
        if len(self._auto_failed) <= cap:
            self._held += 1
            return True
        session.testsfailed += self._held
        self._held = 0
        return False

    @pytest.hookimpl(wrapper=True)
    def pytest_runtest_logreport(self, report):
        session = getattr(self._item, "session", None)
        option = getattr(getattr(session, "config", None), "option", None)
        if (
            session is None
            or option is None
            or not getattr(option, "maxfail", 0)
            or not report.failed
            or not self._exempt(report, session)
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


class FlakyReruns(_TagRuledOut):
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
                if (
                    attempt < reruns
                    and report.failed
                    and not hasattr(report, "wasxfail")
                    and not getattr(report, "_rstest_no_rerun", False)
                ):
                    report.outcome = "rerun"
                    item.ihook.pytest_runtest_logreport(report=report)
                    retry = True
                    break
                item.ihook.pytest_runtest_logreport(report=report)
            if not retry:
                break
            delay = rerun_delay(item)
            if delay:
                time.sleep(delay)
            _forget_failed_setup(item)
        item.ihook.pytest_runtest_logfinish(nodeid=item.nodeid, location=item.location)
        return True

    @pytest.hookimpl(tryfirst=True)
    def pytest_report_teststatus(self, report):
        if report.outcome == "rerun":
            return "rerun", "R", ("RERUN", {"yellow": True})
        return None
