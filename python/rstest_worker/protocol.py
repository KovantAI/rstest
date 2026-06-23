"""Framed msgpack over raw fds. Messages are maps {"kind": ..., "payload": ...}.

Requires the `msgpack` package in the target interpreter for now.
TODO(M1): vendor msgpack's pure-python fallback so the worker is
PYTHONPATH-injectable into any venv with zero installs.
"""

import os

try:
    import msgpack
except ImportError:  # pragma: no cover
    raise SystemExit(
        "rstest worker requires the 'msgpack' package in the test environment "
        "(pip install msgpack)"
    )


class Connection:
    def __init__(self, cmd_fd: int, evt_fd: int):
        self._cmd_fd = cmd_fd
        self._evt_fd = evt_fd
        self._unpacker = msgpack.Unpacker(raw=False)

    def commands(self):
        """Yield command messages until EOF."""
        while True:
            msg = self.recv_one()
            if msg is None:
                return
            yield msg

    def recv_one(self):
        """Block until one command message is available (None on EOF)."""
        while True:
            for msg in self._unpacker:
                return msg
            data = os.read(self._cmd_fd, 65536)
            if not data:
                return None
            self._unpacker.feed(data)

    def send(self, kind: str, payload) -> None:
        os.write(self._evt_fd, msgpack.packb({"kind": kind, "payload": payload}))
