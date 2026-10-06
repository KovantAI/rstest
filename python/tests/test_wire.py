"""Unit tests for the wire-serialization helper."""

from rstest_worker._internal.wire import _wire_safe


def test_primitives_pass_through():
    assert _wire_safe("x") == "x"
    assert _wire_safe(3) == 3
    assert _wire_safe(1.5) == 1.5
    assert _wire_safe(True) is True
    assert _wire_safe(None) is None


def test_tuple_becomes_list():
    assert _wire_safe((1, 2, 3)) == [1, 2, 3]


def test_nested_containers_recurse():
    value = {"a": [1, ("b", 2)], "c": {"d": None}}
    assert _wire_safe(value) == {"a": [1, ["b", 2]], "c": {"d": None}}


def test_non_str_dict_keys_stringified():
    assert _wire_safe({1: "a", 2: "b"}) == {"1": "a", "2": "b"}


def test_non_serializable_becomes_none():
    assert _wire_safe(object()) is None
    assert _wire_safe({1, 2, 3}) is None  # sets aren't in the allowed set


def test_non_serializable_nested_becomes_none():
    # A non-serializable leaf inside a container is coerced, not dropped.
    assert _wire_safe([1, object(), 3]) == [1, None, 3]
    assert _wire_safe({"k": object()}) == {"k": None}
