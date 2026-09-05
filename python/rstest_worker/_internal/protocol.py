"""Framed msgpack over raw fds. Messages are maps {"kind": ..., "payload": ...}.

The message schema is defined in `rstest_worker._internal.messages` (mirroring the
Rust `proto.rs`). `Connection.send` is overloaded per event `kind` so a mismatched
(kind, payload) pair is a type error.

Requires the `msgpack` package in the target interpreter for now.
TODO(M1): vendor msgpack's pure-python fallback so the worker is
PYTHONPATH-injectable into any venv with zero installs.
"""

from __future__ import annotations

import os
from collections.abc import Iterator
from typing import Literal, overload

from rstest_worker._internal import messages as m

try:
    import msgpack
except ImportError:  # pragma: no cover
    raise SystemExit(
        "rstest worker requires the 'msgpack' package in the test environment (pip install msgpack)"
    ) from None


class Connection:
    def __init__(self, cmd_fd: int, evt_fd: int) -> None:
        self._cmd_fd = cmd_fd
        self._evt_fd = evt_fd
        self._unpacker = msgpack.Unpacker(raw=False)

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
                return msg
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
        buf = memoryview(msgpack.packb({"kind": kind, "payload": payload}))
        while buf:
            buf = buf[os.write(self._evt_fd, buf) :]
