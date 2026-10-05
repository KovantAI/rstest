"""Per-test resource-leak attribution (`--doctor` / `--fail-on-leak`).

Tracks resource *identities*, not counts: the live `threading.Thread` objects
and the open fds (keyed by `(fd, st_dev, st_ino)`, so a reused fd number that
now names a different file is a different resource). A test is charged with
what it created during its own window (setup + call + teardown) and is still
alive when its teardown ends. A count nets one test's leak against another
test's release (a module fixture's teardown in the last test cancels that
test's own leak; a thread that exits during a later test hides that test's
leak); a set difference doesn't: the creator stays charged however long the
resource lives.

Resources created while a fixture whose scope outlives the test (class,
module, package, session) is being set up belong to that fixture, not to the
test that happened to trigger the setup, and are never charged to a test. They
are set up once per scope, so they can't pile up per test; and they sit in
every later test's starting snapshot, so their release at scope end (often in
another test's window) changes nothing either.
"""

from __future__ import annotations

import os
import threading
from typing import Any

import pytest

Fd = tuple[int, int, int]


def live_threads() -> frozenset[threading.Thread]:
    """The live Python threads. Native C-extension threads that bypass the
    `threading` module are not seen."""
    return frozenset(threading.enumerate())


def open_fds() -> frozenset[Fd] | None:
    """Open fds as `(fd, st_dev, st_ino)`, or None where they can't be listed.
    `/proc/self/fd` on Linux, `/dev/fd` on macOS/BSD; other platforms disable fd
    tracking. The listing's own directory fd is closed by the time it would be
    stat'ed, so it drops out."""
    for d in ("/proc/self/fd", "/dev/fd"):
        try:
            names = os.listdir(d)
        except OSError:
            continue
        out: set[Fd] = set()
        for name in names:
            try:
                fd = int(name)
                st = os.fstat(fd)
            except (ValueError, OSError):
                continue
            out.add((fd, st.st_dev, st.st_ino))
        return frozenset(out)
    return None


class _Window:
    """One test's measurement: what was live when its setup began, plus what
    wider-scoped fixtures created inside it (not the test's to release)."""

    __slots__ = ("fds", "fixture_fds", "fixture_threads", "threads")

    def __init__(self, threads: frozenset[Any], fds: frozenset[Fd] | None) -> None:
        self.threads = threads
        self.fds = fds
        self.fixture_threads: set[Any] = set()
        self.fixture_fds: set[Fd] = set()


class LeakTracker:
    """pytest plugin: charges each test with the threads/fds it created and
    never released.

    `pop(nodeid)` returns `(threads, fds)` leaked by that test (fds None where
    fds can't be listed), or None when the test wasn't measured (the warm-up,
    or never run)."""

    def __init__(self) -> None:
        self._current: _Window | None = None
        self._res: dict[str, tuple[int, int | None]] = {}
        # Skip the worker's FIRST test: a library can lazily spin up a
        # persistent thread / open a cache fd on first use, which is not a
        # per-test leak. Measuring from the 2nd test on drops that noise.
        self._warmed = False

    def pop(self, nodeid: str) -> tuple[int, int | None] | None:
        return self._res.pop(nodeid, None)

    @pytest.hookimpl(wrapper=True)
    def pytest_runtest_setup(self, item):
        # Snapshot BEFORE any setup fixture runs.
        self._current = _Window(live_threads(), open_fds())
        return (yield)

    @pytest.hookimpl(wrapper=True)
    def pytest_runtest_teardown(self, item, nextitem):
        try:
            return (yield)
        finally:
            win, self._current = self._current, None
            if win is not None:
                self._finish(item.nodeid, win)

    def _finish(self, nodeid: str, win: _Window) -> None:
        if not self._warmed:
            self._warmed = True
            return
        threads, fds = live_threads(), open_fds()
        leaked_t = threads - win.threads - win.fixture_threads
        fd_count: int | None = None
        if fds is not None and win.fds is not None:
            fd_count = len(fds - win.fds - win.fixture_fds)
        self._res[nodeid] = (len(leaked_t), fd_count)

    @pytest.hookimpl(wrapper=True)
    def pytest_fixture_setup(self, fixturedef, request):
        win = self._current
        if win is None or getattr(fixturedef, "scope", "function") == "function":
            return (yield)
        bt, bf = live_threads(), open_fds()
        try:
            return (yield)
        finally:
            win.fixture_threads |= live_threads() - bt
            af = open_fds()
            if af is not None and bf is not None:
                win.fixture_fds |= af - bf
