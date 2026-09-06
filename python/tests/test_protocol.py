"""Unit tests for the framed-msgpack Connection over raw fds."""

import os

import msgpack
from rstest_worker._internal import messages as m
from rstest_worker._internal.protocol import Connection


def test_recv_one_decodes_a_framed_message():
    r, w = os.pipe()
    os.write(w, msgpack.packb({"kind": "run_tests", "payload": {"x": 1}}))
    conn = Connection(cmd_fd=r, evt_fd=-1)
    try:
        assert conn.recv_one() == {"kind": "run_tests", "payload": {"x": 1}}
    finally:
        os.close(w)
        os.close(r)


def test_recv_one_returns_none_on_eof():
    r, w = os.pipe()
    os.close(w)  # writer gone -> os.read yields b"" -> EOF
    conn = Connection(cmd_fd=r, evt_fd=-1)
    try:
        assert conn.recv_one() is None
    finally:
        os.close(r)


def test_recv_one_reassembles_split_frame():
    # A frame delivered in two reads must still decode (unpacker buffers).
    r, w = os.pipe()
    packed = msgpack.packb({"kind": "run_tests", "payload": {"a": [1, 2, 3]}})
    os.write(w, packed[:3])
    os.write(w, packed[3:])
    conn = Connection(cmd_fd=r, evt_fd=-1)
    try:
        assert conn.recv_one() == {"kind": "run_tests", "payload": {"a": [1, 2, 3]}}
    finally:
        os.close(w)
        os.close(r)


def test_commands_yields_until_eof():
    r, w = os.pipe()
    os.write(w, msgpack.packb({"kind": "a", "payload": 1}))
    os.write(w, msgpack.packb({"kind": "b", "payload": 2}))
    os.close(w)  # EOF terminates the iterator
    conn = Connection(cmd_fd=r, evt_fd=-1)
    try:
        assert list(conn.commands()) == [
            {"kind": "a", "payload": 1},
            {"kind": "b", "payload": 2},
        ]
    finally:
        os.close(r)


def test_send_writes_framed_message():
    r, w = os.pipe()
    conn = Connection(cmd_fd=-1, evt_fd=w)
    payload: m.ReportPayload = {
        "nodeid": "t.py::a",
        "when": "call",
        "outcome": "passed",
        "duration": 0.0,
        "longrepr": None,
        "wasxfail": False,
    }
    try:
        conn.send("report", payload)
        data = os.read(r, 65536)
        assert msgpack.unpackb(data, raw=False) == {
            "kind": "report",
            "payload": payload,
        }
    finally:
        os.close(w)
        os.close(r)
