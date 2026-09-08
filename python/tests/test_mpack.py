"""Tests for the pure-python msgpack framing codec (`_internal.mpack`).

The load-bearing guarantee is wire-compatibility with canonical msgpack (which
the Rust orchestrator's rmp_serde speaks): the pure encoder must emit the exact
bytes `msgpack.packb` does, and the pure decoder must read whatever msgpack
emits. Those two parity tests skip when msgpack isn't installed; the round-trip,
framing, and bound tests run everywhere.
"""

import importlib
from typing import Any

import pytest
from rstest_worker._internal import mpack

# Imported dynamically so the handle is `Any`: `import msgpack as _msgpack` would
# make ty treat _msgpack as a hard module type and reject the `.packb` accesses
# below (it can't see that @skipif guards them on _msgpack being present).
_msgpack: Any = None
try:
    _msgpack = importlib.import_module("msgpack")
except ImportError:
    _msgpack = None


# A message that exercises every type the protocol puts on the wire: nested
# maps/arrays, None, unicode, bool, negative + wide ints, float.
SAMPLE = {
    "kind": "collection_done",
    "payload": {
        "count": 3,
        "hash": "deadbeef",
        "ids": ["t.py::a", "t.py::b", "pkg/mod.py::test_ünicode"],
        "locations": [["t.py", 1], ["t.py", None], ["pkg/mod.py", 4096]],
        "marks": [["serial"], [], ["flaky", "slow"]],
        "serial": [0, -1, 65536],
        "flaky": {"0": 2, "2": 5},
        "ratio": 0.5,
        "ok": True,
        "missing": None,
    },
}


def _pure_roundtrip(obj):
    up = mpack._PureUnpacker()
    up.feed(mpack._pure_packb(obj))
    return next(iter(up))


def test_pure_roundtrip_preserves_the_message():
    assert _pure_roundtrip(SAMPLE) == SAMPLE


@pytest.mark.parametrize(
    "value",
    [
        None,
        True,
        False,
        0,
        127,
        128,
        255,
        256,
        65535,
        65536,
        2**32 - 1,
        2**32,
        2**64 - 1,
        -1,
        -32,
        -33,
        -128,
        -129,
        -(2**31),
        -(2**31) - 1,
        -(2**63),
        3.14,
        -0.0,
        "",
        "x" * 40,  # crosses fixstr -> str8
        "y" * 300,  # crosses str8 -> str16
        [],
        {},
        [1, [2, [3]]],
        {"a": {"b": {"c": 1}}},
    ],
)
def test_pure_roundtrip_scalar_and_boundary_values(value):
    assert _pure_roundtrip(value) == value


def test_pure_unpacker_reassembles_split_frame():
    packed = mpack._pure_packb(SAMPLE)
    up = mpack._PureUnpacker()
    # Feed one byte at a time; only the final byte completes the object.
    seen = []
    for i in range(len(packed)):
        up.feed(packed[i : i + 1])
        seen.extend(up)
    assert seen == [SAMPLE]


def test_pure_unpacker_yields_multiple_frames():
    up = mpack._PureUnpacker()
    up.feed(mpack._pure_packb({"kind": "a", "payload": 1}))
    up.feed(mpack._pure_packb({"kind": "b", "payload": 2}))
    assert list(up) == [{"kind": "a", "payload": 1}, {"kind": "b", "payload": 2}]


def test_pure_unpacker_enforces_max_buffer_size():
    up = mpack._PureUnpacker(max_buffer_size=8)
    with pytest.raises(mpack.BufferFull):
        up.feed(mpack._pure_packb("this string is definitely longer than eight bytes"))


def test_pure_packb_rejects_unencodable_type():
    with pytest.raises(TypeError):
        mpack._pure_packb({1, 2, 3})  # a set has no msgpack representation


def test_pure_packb_rejects_oversized_int():
    with pytest.raises(ValueError):
        mpack._pure_packb(2**64)  # past uint64


@pytest.mark.skipif(_msgpack is None, reason="msgpack not installed")
def test_pure_encoding_is_byte_identical_to_msgpack():
    # The whole point: rmp_serde on the Rust side reads canonical msgpack, so the
    # pure encoder must match msgpack.packb byte-for-byte.
    assert mpack._pure_packb(SAMPLE) == _msgpack.packb(SAMPLE)


@pytest.mark.skipif(_msgpack is None, reason="msgpack not installed")
@pytest.mark.parametrize(
    "value",
    [None, True, 0, 127, 128, 65536, -1, -33, -129, 3.14, "hi", "z" * 500, [1, 2], {"k": "v"}],
)
def test_pure_encoding_matches_msgpack_per_value(value):
    assert mpack._pure_packb(value) == _msgpack.packb(value)


@pytest.mark.skipif(_msgpack is None, reason="msgpack not installed")
def test_pure_decoder_reads_msgpack_output():
    # And the reverse direction: the pure decoder must parse what msgpack (hence
    # rmp_serde) emits, including int/str widths the pure encoder might not pick.
    up = mpack._PureUnpacker()
    up.feed(_msgpack.packb(SAMPLE))
    assert next(iter(up)) == SAMPLE


@pytest.mark.skipif(_msgpack is None, reason="msgpack not installed")
def test_pure_decoder_reads_float32_and_wide_ints():
    # msgpack never emits float32 for a Python float, but rmp/other encoders can;
    # hand-build a frame to prove the decoder handles the forms it doesn't emit.
    packed = _msgpack.packb({"a": 1}) + b"\xca\x40\x49\x0f\xdb"  # + float32 3.1415927
    up = mpack._PureUnpacker()
    up.feed(packed)
    got = list(up)
    assert got[0] == {"a": 1}
    assert isinstance(got[1], float) and abs(got[1] - 3.1415927) < 1e-6
