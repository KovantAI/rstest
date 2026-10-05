"""Framed msgpack over raw fds. Messages are maps {"kind": ..., "payload": ...}.

The message schema is defined in `rstest_worker._internal.messages` (mirroring the
Rust `proto.rs`). `Connection.send` is overloaded per event `kind` so a mismatched
(kind, payload) pair is a type error.

The msgpack framing itself comes from `rstest_worker._internal.mpack`, a small
pure-python codec (with the compiled `msgpack` preferred when the venv has it),
so the worker is PYTHONPATH-injectable into any interpreter with zero installs.
"""

from __future__ import annotations

import os
from collections.abc import Iterator
from typing import Literal, cast, overload

from rstest_worker._internal import messages as m
from rstest_worker._internal import mpack

# Upper bound on a single buffered frame. Without it a desynced stream (a stray
# byte claiming a multi-GB map/array) makes the Unpacker buffer unboundedly and
# OOM silently; capped, it raises mpack.BufferFull so the desync fails loud. The
# ceiling is deliberately generous: the largest legit frame is CollectionDone
# carrying ids/locations/marks for the whole suite, which reaches hundreds of MB
# on large monorepos.
_MAX_FRAME_BYTES = 512 * 1024 * 1024


def _readable(fd: int) -> bool:
    """Whether a read on `fd` would return at once (data or EOF). False when
    that can't be told, so the caller just carries on without polling."""
    try:
        if os.name == "nt":
            return _peek_pipe(fd)
        import select

        if hasattr(select, "poll"):
            poller = select.poll()
            poller.register(fd, select.POLLIN)
            return bool(poller.poll(0))
        return bool(select.select([fd], [], [], 0)[0])
    except (OSError, ValueError, AttributeError):
        return False


def _peek_pipe(fd: int) -> bool:  # pragma: no cover - Windows only
    """Windows: bytes waiting in the anonymous pipe behind `fd`."""
    import ctypes
    import msvcrt
    from ctypes import wintypes

    kernel32 = getattr(ctypes, "windll").kernel32  # noqa: B009 - absent off Windows
    avail = wintypes.DWORD()
    ok = kernel32.PeekNamedPipe(
        wintypes.HANDLE(getattr(msvcrt, "get_osfhandle")(fd)),  # noqa: B009
        None,
        0,
        None,
        ctypes.byref(avail),
        None,
    )
    return bool(ok) and avail.value > 0


class Connection:
    def __init__(self, cmd_fd: int, evt_fd: int) -> None:
        self._cmd_fd = cmd_fd
        self._evt_fd = evt_fd
        self._unpacker = mpack.Unpacker(max_buffer_size=_MAX_FRAME_BYTES)

    def commands(self) -> Iterator[m.Command]:
        """Yield command messages until EOF."""
        while True:
            msg = self.recv_one()
            if msg is None:
                return
            yield msg

    def recv_one(self) -> m.Command | None:
        """Block until one command message is available (None on EOF)."""
        while True:
            for msg in self._unpacker:
                # The decoder yields a bare object; the {"kind", "payload"}
                # contract is enforced by the schema (messages.py), not the wire.
                return cast("m.Command", msg)
            data = os.read(self._cmd_fd, 65536)
            if not data:
                return None
            self._unpacker.feed(data)

    def poll_one(self) -> m.Command | None:
        """One command that has already arrived, without blocking; None when
        nothing is waiting. EOF also reads as None here: the next blocking
        `recv_one` reports it. Lets a worker notice a run-wide stop between
        tests while it still has queued items of its own."""
        while True:
            for msg in self._unpacker:
                return cast("m.Command", msg)
            if not _readable(self._cmd_fd):
                return None
            data = os.read(self._cmd_fd, 65536)
            if not data:
                return None
            self._unpacker.feed(data)

    # One overload per Event kind binds the `kind` string to its payload type,
    # so `send("report", collection_done_dict)` is a type error. Keep in sync
    # with `messages.EventKind` and proto.rs.
    @overload
    def send(self, kind: Literal["report"], payload: m.ReportPayload) -> None: ...
    @overload
    def send(self, kind: Literal["collect_error"], payload: m.CollectErrorPayload) -> None: ...
    @overload
    def send(self, kind: Literal["collect_skip"], payload: m.CollectSkipPayload) -> None: ...
    @overload
    def send(self, kind: Literal["doctor_fixtures"], payload: m.DoctorFixturesPayload) -> None: ...
    @overload
    def send(self, kind: Literal["warnings"], payload: m.WarningsPayload) -> None: ...
    @overload
    def send(self, kind: Literal["junit_case"], payload: m.JunitCasePayload) -> None: ...
    @overload
    def send(self, kind: Literal["junit_suite"], payload: m.JunitSuitePayload) -> None: ...
    @overload
    def send(self, kind: Literal["collection_done"], payload: m.CollectionDonePayload) -> None: ...
    @overload
    def send(self, kind: Literal["lazy_ready"], payload: m.LazyReadyPayload) -> None: ...
    @overload
    def send(self, kind: Literal["file_collected"], payload: m.FileCollectedPayload) -> None: ...
    @overload
    def send(self, kind: Literal["item_start_id"], payload: m.ItemStartIdPayload) -> None: ...
    @overload
    def send(self, kind: Literal["item_done_id"], payload: m.ItemDoneIdPayload) -> None: ...
    @overload
    def send(self, kind: Literal["stopped_ids"], payload: m.StoppedIdsPayload) -> None: ...
    @overload
    def send(self, kind: Literal["node_input"], payload: m.NodeInputPayload) -> None: ...
    @overload
    def send(self, kind: Literal["item_start"], payload: m.ItemStartPayload) -> None: ...
    @overload
    def send(self, kind: Literal["item_done"], payload: m.ItemDonePayload) -> None: ...
    @overload
    def send(self, kind: Literal["stopped"], payload: m.StoppedPayload) -> None: ...
    @overload
    def send(self, kind: Literal["done"], payload: m.DonePayload) -> None: ...

    def send(self, kind: str, payload: object) -> None:
        # os.write on a pipe may short-write (a large ids/locations/marks
        # payload can exceed the pipe buffer), so loop until every byte drains;
        # a partial frame would desync the orchestrator's msgpack stream.
        buf = memoryview(mpack.packb({"kind": kind, "payload": payload}))
        while buf:
            buf = buf[os.write(self._evt_fd, buf) :]
