"""Unit coverage for the --timeout parse helper (the interrupt itself is
covered end-to-end by the e2e gate's native-timeout section)."""

from rstest_worker._internal.stream import _parse_timeout


def test_parse_timeout_accepts_positive_floats():
    assert _parse_timeout("1") == 1.0
    assert _parse_timeout("0.3") == 0.3
    assert _parse_timeout("12.5") == 12.5


def test_parse_timeout_rejects_non_positive_and_bad_input():
    assert _parse_timeout(None) is None
    assert _parse_timeout("") is None
    assert _parse_timeout("0") is None
    assert _parse_timeout("-2") is None
    assert _parse_timeout("garbage") is None
