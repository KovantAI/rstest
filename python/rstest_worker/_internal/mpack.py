"""Minimal msgpack framing codec for the worker wire protocol.

Pure-python by default so the worker is PYTHONPATH-injectable into any venv with
zero installs (the whole point of vendoring pytest too). When the compiled
`msgpack` package happens to be importable we use it instead — it is faster on
the large `collection_done` frames big monorepos produce — but the pure-python
path below is byte-for-byte wire-identical and fully sufficient on its own.

Scope is deliberately narrow: only the msgpack types this protocol puts on the
wire — nil, bool, int, float (as float64), str, bin, array, map. No ext, no
timestamp, no float32 on encode. The Rust orchestrator (`rmp_serde`, see
`crates/rstest-cli/src/scheduling/proto.rs`) is the other end of every frame, so
this MUST stay byte-compatible with canonical msgpack; the round-trip and
cross-backend parity tests in `tests/test_mpack.py` guard that.

Public surface (mirrors the slice of `msgpack` protocol.py used to import):
    packb(obj) -> bytes
    Unpacker(max_buffer_size=0)  # .feed(bytes); iterate to yield decoded objects
    BufferFull                   # raised when buffered-but-unconsumed exceeds cap
"""

from __future__ import annotations

import struct
from collections.abc import Iterator
from typing import Any


class BufferFull(Exception):
    """A single unconsumed frame exceeded the Unpacker's max_buffer_size."""


class _OutOfData(Exception):
    """Internal: the buffer holds only a partial object; wait for more bytes."""


# ---- encode ----------------------------------------------------------------

_PACK_U16 = struct.Struct(">H")
_PACK_U32 = struct.Struct(">I")
_PACK_U64 = struct.Struct(">Q")
_PACK_I8 = struct.Struct(">b")
_PACK_I16 = struct.Struct(">h")
_PACK_I32 = struct.Struct(">i")
_PACK_I64 = struct.Struct(">q")
_PACK_F64 = struct.Struct(">d")


def _pack_int(out: bytearray, n: int) -> None:
    # Canonical minimal encoding, matching msgpack's own choices so a frame this
    # codec produces is byte-identical to msgpack.packb (see the parity test).
    if 0 <= n <= 0x7F:  # positive fixint
        out.append(n)
    elif -0x20 <= n < 0:  # negative fixint
        out.append(0xE0 | (n + 0x20))
    elif n >= 0:  # unsigned widths
        if n <= 0xFF:
            out += b"\xcc"
            out.append(n)
        elif n <= 0xFFFF:
            out += b"\xcd"
            out += _PACK_U16.pack(n)
        elif n <= 0xFFFFFFFF:
            out += b"\xce"
            out += _PACK_U32.pack(n)
        elif n <= 0xFFFFFFFFFFFFFFFF:
            out += b"\xcf"
            out += _PACK_U64.pack(n)
        else:
            raise ValueError(f"integer out of msgpack range: {n}")
    else:  # signed widths
        if n >= -0x80:
            out += b"\xd0"
            out += _PACK_I8.pack(n)
        elif n >= -0x8000:
            out += b"\xd1"
            out += _PACK_I16.pack(n)
        elif n >= -0x80000000:
            out += b"\xd2"
            out += _PACK_I32.pack(n)
        elif n >= -0x8000000000000000:
            out += b"\xd3"
            out += _PACK_I64.pack(n)
        else:
            raise ValueError(f"integer out of msgpack range: {n}")


def _pack_len(out: bytearray, n: int, fix_base: int, u16: bytes, u32: bytes, fix_max: int) -> None:
    if n <= fix_max:
        out.append(fix_base | n)
    elif n <= 0xFFFF:
        out += u16
        out += _PACK_U16.pack(n)
    elif n <= 0xFFFFFFFF:
        out += u32
        out += _PACK_U32.pack(n)
    else:
        raise ValueError(f"length out of msgpack range: {n}")


def _pack(out: bytearray, obj: object) -> None:
    if obj is None:
        out += b"\xc0"
    elif obj is True:
        out += b"\xc3"
    elif obj is False:
        out += b"\xc2"
    elif isinstance(obj, int):  # bool already handled above (bool subclasses int)
        _pack_int(out, obj)
    elif isinstance(obj, float):
        out += b"\xcb"
        out += _PACK_F64.pack(obj)
    elif isinstance(obj, str):
        data = obj.encode("utf-8")
        n = len(data)
        if n <= 0x1F:  # fixstr
            out.append(0xA0 | n)
        elif n <= 0xFF:
            out += b"\xd9"
            out.append(n)
        elif n <= 0xFFFF:
            out += b"\xda"
            out += _PACK_U16.pack(n)
        elif n <= 0xFFFFFFFF:
            out += b"\xdb"
            out += _PACK_U32.pack(n)
        else:
            raise ValueError("str too long for msgpack")
        out += data
    elif isinstance(obj, (bytes, bytearray)):
        n = len(obj)
        if n <= 0xFF:
            out += b"\xc4"
            out.append(n)
        elif n <= 0xFFFF:
            out += b"\xc5"
            out += _PACK_U16.pack(n)
        elif n <= 0xFFFFFFFF:
            out += b"\xc6"
            out += _PACK_U32.pack(n)
        else:
            raise ValueError("bytes too long for msgpack")
        out += obj
    elif isinstance(obj, (list, tuple)):
        _pack_len(out, len(obj), 0x90, b"\xdc", b"\xdd", 0x0F)  # array
        for item in obj:
            _pack(out, item)
    elif isinstance(obj, dict):
        _pack_len(out, len(obj), 0x80, b"\xde", b"\xdf", 0x0F)  # map
        for key, val in obj.items():
            _pack(out, key)
            _pack(out, val)
    else:
        raise TypeError(f"cannot msgpack-encode {type(obj).__name__}")


def _pure_packb(obj: object) -> bytes:
    """Serialize one object to a msgpack frame."""
    out = bytearray()
    _pack(out, obj)
    return bytes(out)


# ---- decode ----------------------------------------------------------------


_Buf = bytes | bytearray


def _need(buf: _Buf, pos: int, n: int) -> None:
    if pos + n > len(buf):
        raise _OutOfData


def _unpack(buf: _Buf, pos: int) -> tuple[object, int]:
    """Decode one object at `pos`; return (value, next_pos). Raise _OutOfData if
    the buffer holds only part of it."""
    _need(buf, pos, 1)
    b = buf[pos]
    pos += 1

    if b <= 0x7F:  # positive fixint
        return b, pos
    if b >= 0xE0:  # negative fixint
        return b - 0x100, pos
    if 0xA0 <= b <= 0xBF:  # fixstr
        return _read_str(buf, pos, b & 0x1F)
    if 0x90 <= b <= 0x9F:  # fixarray
        return _read_array(buf, pos, b & 0x0F)
    if 0x80 <= b <= 0x8F:  # fixmap
        return _read_map(buf, pos, b & 0x0F)

    if b == 0xC0:
        return None, pos
    if b == 0xC2:
        return False, pos
    if b == 0xC3:
        return True, pos

    if b == 0xCC:  # uint8
        _need(buf, pos, 1)
        return buf[pos], pos + 1
    if b == 0xCD:  # uint16
        _need(buf, pos, 2)
        return _PACK_U16.unpack_from(buf, pos)[0], pos + 2
    if b == 0xCE:  # uint32
        _need(buf, pos, 4)
        return _PACK_U32.unpack_from(buf, pos)[0], pos + 4
    if b == 0xCF:  # uint64
        _need(buf, pos, 8)
        return _PACK_U64.unpack_from(buf, pos)[0], pos + 8
    if b == 0xD0:  # int8
        _need(buf, pos, 1)
        return _PACK_I8.unpack_from(buf, pos)[0], pos + 1
    if b == 0xD1:  # int16
        _need(buf, pos, 2)
        return _PACK_I16.unpack_from(buf, pos)[0], pos + 2
    if b == 0xD2:  # int32
        _need(buf, pos, 4)
        return _PACK_I32.unpack_from(buf, pos)[0], pos + 4
    if b == 0xD3:  # int64
        _need(buf, pos, 8)
        return _PACK_I64.unpack_from(buf, pos)[0], pos + 8

    if b == 0xCA:  # float32
        _need(buf, pos, 4)
        return struct.unpack_from(">f", buf, pos)[0], pos + 4
    if b == 0xCB:  # float64
        _need(buf, pos, 8)
        return _PACK_F64.unpack_from(buf, pos)[0], pos + 8

    if b == 0xD9:  # str8
        _need(buf, pos, 1)
        return _read_str(buf, pos + 1, buf[pos])
    if b == 0xDA:  # str16
        _need(buf, pos, 2)
        return _read_str(buf, pos + 2, _PACK_U16.unpack_from(buf, pos)[0])
    if b == 0xDB:  # str32
        _need(buf, pos, 4)
        return _read_str(buf, pos + 4, _PACK_U32.unpack_from(buf, pos)[0])

    if b == 0xC4:  # bin8
        _need(buf, pos, 1)
        return _read_bin(buf, pos + 1, buf[pos])
    if b == 0xC5:  # bin16
        _need(buf, pos, 2)
        return _read_bin(buf, pos + 2, _PACK_U16.unpack_from(buf, pos)[0])
    if b == 0xC6:  # bin32
        _need(buf, pos, 4)
        return _read_bin(buf, pos + 4, _PACK_U32.unpack_from(buf, pos)[0])

    if b == 0xDC:  # array16
        _need(buf, pos, 2)
        return _read_array(buf, pos + 2, _PACK_U16.unpack_from(buf, pos)[0])
    if b == 0xDD:  # array32
        _need(buf, pos, 4)
        return _read_array(buf, pos + 4, _PACK_U32.unpack_from(buf, pos)[0])
    if b == 0xDE:  # map16
        _need(buf, pos, 2)
        return _read_map(buf, pos + 2, _PACK_U16.unpack_from(buf, pos)[0])
    if b == 0xDF:  # map32
        _need(buf, pos, 4)
        return _read_map(buf, pos + 4, _PACK_U32.unpack_from(buf, pos)[0])

    # ext / fixext / reserved (0xc1) — this protocol never emits them.
    raise ValueError(f"unsupported msgpack type byte: {b:#04x}")


def _read_str(buf: _Buf, pos: int, n: int) -> tuple[str, int]:
    _need(buf, pos, n)
    return buf[pos : pos + n].decode("utf-8"), pos + n


def _read_bin(buf: _Buf, pos: int, n: int) -> tuple[bytes, int]:
    _need(buf, pos, n)
    return bytes(buf[pos : pos + n]), pos + n


def _read_array(buf: _Buf, pos: int, n: int) -> tuple[list, int]:
    items = []
    for _ in range(n):
        val, pos = _unpack(buf, pos)
        items.append(val)
    return items, pos


def _read_map(buf: _Buf, pos: int, n: int) -> tuple[dict, int]:
    out: dict = {}
    for _ in range(n):
        key, pos = _unpack(buf, pos)
        val, pos = _unpack(buf, pos)
        out[key] = val
    return out, pos


class _PureUnpacker:
    """Streaming decoder mirroring the slice of `msgpack.Unpacker` used here:
    `feed(bytes)` appends to an internal buffer; iterating yields each fully
    buffered object and stops (StopIteration) when only a partial one remains,
    so the caller reads more bytes and feeds again."""

    def __init__(self, max_buffer_size: int = 0) -> None:
        self._buf = bytearray()
        self._pos = 0
        self._max = max_buffer_size

    def feed(self, data: bytes) -> None:
        self._buf += data
        # Guard against a desynced stream buffering unboundedly: cap the
        # unconsumed span, mirroring msgpack's BufferFull.
        if self._max and (len(self._buf) - self._pos) > self._max:
            raise BufferFull

    def __iter__(self) -> Iterator[object]:
        return self

    def __next__(self) -> object:
        try:
            obj, self._pos = _unpack(self._buf, self._pos)
        except _OutOfData:
            self._compact()
            raise StopIteration from None
        self._compact()
        return obj

    def _compact(self) -> None:
        # Drop the consumed prefix so a long-lived Unpacker doesn't grow without
        # bound and the max_buffer_size check stays accurate.
        if self._pos:
            del self._buf[: self._pos]
            self._pos = 0


# ---- backend selection -----------------------------------------------------
#
# The pure-python codec above is the always-available baseline. If the compiled
# `msgpack` is importable we prefer it (much faster on the tens-of-MB frames a
# huge suite produces), wrapping it so `BufferFull` above stays the single
# exception every caller sees regardless of backend.

# A backend handle, imported dynamically so its type is a plain `Any` (a bare
# `import msgpack as _msgpack` would make the name a hard module declaration that
# conflicts with the None fallback). Every dereference is on the not-None path.
_msgpack: Any = None
_msgpack_bf: Any = BufferFull  # msgpack's BufferFull, when present
try:
    import importlib

    _msgpack = importlib.import_module("msgpack")
    _msgpack_bf = _msgpack.exceptions.BufferFull
except ImportError:  # pragma: no cover - exercised only in a msgpack-less venv
    _msgpack = None


def _msgpack_packb(obj: object) -> bytes:
    return _msgpack.packb(obj)


class _MsgpackUnpacker:
    """Adapts `msgpack.Unpacker` to this module's surface, translating msgpack's
    own BufferFull into ours so callers catch a single type."""

    def __init__(self, max_buffer_size: int = 0) -> None:
        self._u = _msgpack.Unpacker(raw=False, max_buffer_size=max_buffer_size)

    def feed(self, data: bytes) -> None:
        try:
            self._u.feed(data)
        except _msgpack_bf as exc:
            raise BufferFull from exc

    def __iter__(self) -> Iterator[object]:
        return self

    def __next__(self) -> object:
        try:
            return next(self._u)
        except _msgpack_bf as exc:
            raise BufferFull from exc


packb = _pure_packb if _msgpack is None else _msgpack_packb
Unpacker = _PureUnpacker if _msgpack is None else _MsgpackUnpacker
