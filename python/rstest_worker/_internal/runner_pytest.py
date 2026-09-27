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

import json
import os
import sys

import pytest

from rstest_worker._internal import fixturecompat
from rstest_worker._internal import messages as m
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
    "SessionStreamPlugin",
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


def _cov_config(args: list[str]) -> str:
    """The `--cov-config` value pytest-cov would hand coverage (its default)."""
    for i, a in enumerate(args):
        if a.startswith("--cov-config="):
            return a.split("=", 1)[1]
        if a == "--cov-config" and i + 1 < len(args):
            return args[i + 1]
    return ".coveragerc"


def _erase_coverage_data(config_file: str) -> None:
    """Erase the run's `.coverage` data, riding out transient Windows locks.

    A just-written data file can be briefly held (antivirus scan, a sibling's
    handle closing) and `os.remove` then fails with WinError 5. Retry briefly;
    if it never clears, warn and keep the worker alive.
    """
    import time

    import coverage

    for delay in (0.05, 0.1, 0.2, 0.4, 0.8, None):
        try:
            coverage.Coverage(config_file=config_file).erase()
            return
        except PermissionError as exc:
            if delay is None:
                print(
                    f"rstest: could not erase stale coverage data: {exc}",
                    file=sys.stderr,
                    flush=True,
                )
                return
            time.sleep(delay)


def _pool_coverage_args(args: list[str]) -> list[str]:
    """Make exactly one pool worker erase the previous run's coverage data.

    pytest-cov starts a Central controller in `pytest_load_initial_conftests`,
    before rstest sets `workerinput`, and that controller erases `.coverage`.
    Every worker doing so at once races on the same file (on Windows the loser
    dies with WinError 5, taking the id-carrier with it). The erase itself is
    load-bearing - covtool's combine merges any `.coverage` left on disk - so
    gw0 does it up front and every worker runs with `--cov-append` to skip
    pytest-cov's own. A user-passed `--cov-append` / `--no-cov` is honored.
    """
    worker_id = os.environ.get("RSTEST_WORKER_ID")
    if worker_id is None:
        return args
    if not any(a == "--cov" or a.startswith("--cov=") for a in args):
        return args
    if "--cov-append" in args or "--no-cov" in args:
        return args
    if worker_id == "gw0":
        try:
            _erase_coverage_data(_cov_config(args))
        except ImportError:
            return args  # no coverage installed: pytest-cov will report it
    return ["--cov-append", *args]


def run_session(args: list[str], conn) -> int:
    """Item-dispatch session (pool mode)."""
    _prime_coverage_core(args)
    args = _pool_coverage_args(args)
    return _contained(lambda: pytest.main(list(args), plugins=[ItemDispatchPlugin(conn)]), conn)


def run_lazy_session(args: list[str], conn) -> int:
    """Lazy-collection session (pool mode, --collect lazy)."""
    _prime_coverage_core(args)
    args = _pool_coverage_args(args)
    return _contained(lambda: pytest.main(list(args), plugins=[LazyDispatchPlugin(conn)]), conn)


def _maybe_start_debugpy() -> None:
    """Under `rstest --debug`, block until an editor (VS Code) attaches.

    The orchestrator forces single-worker mode with inherited stdio for a debug
    run (like --pdb) and sets RSTEST_DEBUGPY_PORT on this worker. We start
    debugpy's listener and wait for the client BEFORE collection, so breakpoints
    in conftest, collection, and tests are all honored. No-op when the env var is
    unset. A missing/failed debugpy degrades to a stderr note rather than killing
    the worker — the session still runs, just without a debugger attached.
    """
    port = os.environ.get("RSTEST_DEBUGPY_PORT")
    if not port:
        return
    # Idempotent across re-imported children (multiprocessing spawn / anyio
    # to_process re-exec this module): only the first call binds the port.
    if os.environ.get("RSTEST_DEBUGPY_LISTENING") == port:
        return
    # Silence debugpy's pydevd file-validation warning (frozen modules are
    # already disabled via `-X frozen_modules=off` at worker launch). setdefault
    # so an explicit user value wins.
    os.environ.setdefault("PYDEVD_DISABLE_FILE_VALIDATION", "1")
    try:
        import debugpy  # ty: ignore[unresolved-import]
    except ImportError:
        print(
            "rstest --debug: the target interpreter has no `debugpy` installed; "
            "run `pip install debugpy` in the test environment. Continuing "
            "without a debugger.",
            file=sys.stderr,
            flush=True,
        )
        return
    try:
        debugpy.listen(("127.0.0.1", int(port)))
        os.environ["RSTEST_DEBUGPY_LISTENING"] = port
        # Machine-readable ready signal on stderr: the editor watches for this
        # line and attaches a DAP client deterministically, instead of grepping
        # human text or polling the port. Emitted before wait_for_client so it
        # arrives while we block. stderr, not stdout, so it never mixes into the
        # inherited pytest stdout stream.
        print(
            json.dumps({"event": "debugpy", "host": "127.0.0.1", "port": int(port)}),
            file=sys.stderr,
            flush=True,
        )
        print(
            f"rstest: debugpy listening on 127.0.0.1:{port}; waiting for client...",
            file=sys.stderr,
            flush=True,
        )
        debugpy.wait_for_client()
    except Exception as exc:
        print(
            f"rstest --debug: could not start debugpy on {port}: {exc}. "
            "Continuing without a debugger.",
            file=sys.stderr,
            flush=True,
        )


def run(args: list[str], conn) -> int:
    """One pytest session over `args`. Returns the session exit status.

    The terminal plugin stays REGISTERED: it owns option definitions
    (`verbose`, `-r`, ...) and the TerminalReporter object that plugins reach
    into (pytest-django, sugar, instafail...). Its output is harmless: worker
    stdout is /dev/null by orchestrator decree.
    """
    _prime_coverage_core(args)
    # `rstest --debug` routes here (single-worker passthrough): wait for the
    # editor to attach before pytest collects, so early breakpoints hold.
    _maybe_start_debugpy()
    return _contained(lambda: pytest.main(list(args), plugins=[SessionStreamPlugin(conn)]), conn)


class SessionStreamPlugin(StreamPlugin):
    """StreamPlugin for the single-session path (`run`): also reports the
    collected count so the orchestrator can print pytest's `[ NN%]` column.
    The dispatch plugins send their own (hash-verified) collection_done."""

    def pytest_collection_finish(self, session):
        payload: m.CollectionDonePayload = {"count": len(session.items), "hash": ""}
        self._conn.send("collection_done", payload)


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
