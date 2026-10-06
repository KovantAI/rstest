"""Checks behind persona steps that judge timing-sensitive logs, tested on
recorded runs so a slow-starting worker cannot flake them."""

from steps.migration import reversed_dispatch_ok


def _rows(*pairs):
    return [{"i": i, "w": w} for w, i in pairs]


def test_reversed_dispatch_tolerates_a_late_starting_worker():
    # macOS CI: gw0 took item 19 but started late, so gw1 logged 17 and 16
    # before gw0 logged 19.
    gw1 = [17, 16, 15, 14, 12, 9, 7, 5, 4, 3, 1, 0]
    gw0 = [19, 18, 13, 11, 10, 8, 6, 2]
    rows = _rows(*[("gw1", i) for i in gw1[:2]], *[("gw0", i) for i in gw0])
    rows += _rows(*[("gw1", i) for i in gw1[2:]])
    ok, _ = reversed_dispatch_ok(rows, 20, 19)
    assert ok


def test_reversed_dispatch_rejects_an_unreversed_run():
    rows = _rows(*[("gw0", i) for i in range(0, 20, 2)], *[("gw1", i) for i in range(1, 20, 2)])
    ok, _ = reversed_dispatch_ok(rows, 20, 19)
    assert not ok


def test_reversed_dispatch_rejects_19_arriving_after_another_item():
    rows = _rows(("gw0", 18), ("gw0", 19), *[("gw1", i) for i in range(17, -1, -1)])
    ok, _ = reversed_dispatch_ok(rows, 20, 19)
    assert not ok
