"""Run tests through the vendored pytest core, streaming reports to the wire.

The vendored `pytest`/`_pytest` (see python/VENDOR.md) provide full test
semantics: fixtures, parametrize, classes, marks, conftest, plugin loading.
rstest owns what happens around the session: scheduling, output, exit codes.

This module is the session entrypoint. The moving parts live alongside it:
  _wire         - wire-serialization helper
  _plugincompat - neutralizing third-party plugins under the pool
  _xdistnode    - xdist master-side node shims + hook plumbing
  stream        - StreamPlugin (report streaming + node-hook emulation)
  dispatch      - ItemDispatchPlugin / LazyDispatchPlugin (on-command running)
"""

from __future__ import annotations

import os

import pytest

from rstest_worker._internal import fixturecompat
from rstest_worker._internal.dispatch import (
    ItemDispatchPlugin,
    LazyDispatchPlugin,
)
from rstest_worker._internal.stream import StreamPlugin

fixturecompat.install()

# Re-exported for callers that reach for the plugin classes by their historical
# import path (rstest_worker._internal.runner_pytest.StreamPlugin, ...).
__all__ = [
    "ItemDispatchPlugin",
    "LazyDispatchPlugin",
    "StreamPlugin",
    "run",
    "run_lazy_session",
    "run_session",
]


def _prime_coverage_core(args: list[str]) -> None:
    """Force coverage's C trace core when per-test contexts are requested.

    Python 3.14 makes `sysmon` (sys.monitoring) coverage's default measurement
    core. sysmon does NOT support DYNAMIC contexts (`--cov-context=test`): a
    line executed by two tests keeps only the FIRST test's context and coverage
    emits a `no-sysmon-context` warning. That silently corrupts the line->test
    index rstest builds for coverage-based `--changed` selection. The C tracer
    (`ctrace`, coverage's default before 3.14) supports dynamic contexts, so
    pin it here unless the user chose a core themselves.
    """
    if os.environ.get("COVERAGE_CORE"):
        return
    if any(a == "--cov-context" or a.startswith("--cov-context=") for a in args):
        os.environ["COVERAGE_CORE"] = "ctrace"


def run_session(args: list[str], conn) -> int:
    """Item-dispatch session (pool mode)."""
    _prime_coverage_core(args)
    return _contained(lambda: pytest.main(list(args), plugins=[ItemDispatchPlugin(conn)]), conn)


def run_lazy_session(args: list[str], conn) -> int:
    """Lazy-collection session (pool mode, --collect lazy)."""
    _prime_coverage_core(args)
    return _contained(lambda: pytest.main(list(args), plugins=[LazyDispatchPlugin(conn)]), conn)


def run(args: list[str], conn) -> int:
    """One pytest session over `args`. Returns the session exit status.

    The terminal plugin stays REGISTERED: it owns option definitions
    (`verbose`, `-r`, ...) and the TerminalReporter object that plugins reach
    into (pytest-django, sugar, instafail...). Its output is harmless: worker
    stdout is /dev/null by orchestrator decree.
    """
    _prime_coverage_core(args)
    return _contained(lambda: pytest.main(list(args), plugins=[StreamPlugin(conn)]), conn)


def _contained(session_fn, conn) -> int:
    """Run a session, never letting exceptions kill the worker process.

    pytest.main can be escaped by BaseExceptions: a conftest's module-level
    `pytest.importorskip(...)` raises Skipped at CONFIG time (file-granular
    dispatch hits this; pandas/tests/io/pytables is the canonical case). We
    contain it so one poisoned session never kills the worker or the protocol.
    """
    import traceback

    try:
        return int(session_fn())
    except KeyboardInterrupt:
        # pytest's Interrupted (collection errors, --maxfail interrupts)
        # subclasses KeyboardInterrupt; the per-module errors were already
        # reported via collectreport. Exit code 2, like pytest.
        return 2
    except BaseException as exc:
        conn.send(
            "collect_error",
            {
                "path": f"<session: {type(exc).__name__}>",
                "longrepr": traceback.format_exc(),
            },
        )
        return 1  # observed real-pytest exit for config-time Skipped
