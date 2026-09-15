"""Gate-classification unit tests for the corpus speed-ramp bench.

The bench's job is to tell three cases apart from a single measured row:
  investigate — the pytest *baseline* collapsed at collection (nothing to
                compare against), so the row is surfaced as a warning, not gated;
  regression  — a usable baseline was beaten wrong (rstest broke collection on
                its own, or per-test parity dropped below the floor);
  ok          — usable baseline, rstest matched it.

The regression tests here pin two subtle boundaries that have each been broken
before:
  * a baseline with a FEW collect errors that still collected tests is usable —
    it must be gated on the tests it did collect, not waived wholesale;
  * rstest having collect errors is a fault only when pytest DIDN'T — the SAME
    upstream drift breaking collection under BOTH runners is agreement (they
    miss the same modules), and parity, not the raw error count, is the arbiter.
"""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))  # bench imports its sibling `run`

from bench import _baseline_unusable, _row_status

FLOOR = 99.5


def _row(base_err=0, base_tests=100, cand_err=0, parity=100.0):
    return {
        "baseline_collect_errors": base_err,
        "baseline_tests": base_tests,
        "candidate_collect_errors": cand_err,
        "parity": parity,
    }


# ---- _baseline_unusable: only TOTAL collapse (errors AND zero tests) ---------


def test_baseline_unusable_only_on_total_collapse():
    assert _baseline_unusable(_row(base_err=444, base_tests=0)) is True


def test_baseline_with_errors_but_tests_is_usable():
    # A few collect errors but tests still collected -> parity over the
    # intersection is meaningful, so the baseline is NOT waived.
    assert _baseline_unusable(_row(base_err=5, base_tests=900)) is False


def test_healthy_baseline_is_usable():
    assert _baseline_unusable(_row(base_err=0, base_tests=500)) is False


def test_empty_baseline_without_errors_is_not_unusable():
    # Zero tests but no collect errors is a different problem (empty/misconfig),
    # not the "reference broke" carve-out.
    assert _baseline_unusable(_row(base_err=0, base_tests=0)) is False


# ---- _row_status: the three-way classification ------------------------------


def test_total_collapse_is_investigate_not_gated():
    row = _row(base_err=444, base_tests=0, cand_err=444, parity=0.0)
    assert _row_status(row, FLOOR) == "investigate"


def test_symmetric_partial_drift_that_agrees_is_ok():
    # REGRESSION GUARD: the same drift breaks collection under BOTH runners; they
    # miss the same modules and agree by absence (parity 100). rstest's non-zero
    # collect-error count must NOT, by itself, read as a regression.
    row = _row(base_err=5, base_tests=900, cand_err=5, parity=100.0)
    assert _row_status(row, FLOOR) == "ok"


def test_rstest_broke_collection_while_baseline_healthy_is_regression():
    row = _row(base_err=0, base_tests=900, cand_err=3, parity=100.0)
    assert _row_status(row, FLOOR) == "regression"


def test_parity_below_floor_is_regression_even_with_matching_errors():
    # Both drifted, but per-test outcomes diverged too -> parity is the arbiter.
    row = _row(base_err=5, base_tests=900, cand_err=5, parity=90.0)
    assert _row_status(row, FLOOR) == "regression"


def test_regression_on_collectible_portion_of_partly_broken_baseline():
    # REGRESSION GUARD: baseline has 1 collect error but 999 usable tests; rstest
    # regresses one of them. The partial breakage must not waive the gate.
    row = _row(base_err=1, base_tests=999, cand_err=0, parity=90.0)
    assert _row_status(row, FLOOR) == "regression"


def test_all_healthy_is_ok():
    row = _row(base_err=0, base_tests=500, cand_err=0, parity=100.0)
    assert _row_status(row, FLOOR) == "ok"


def test_parity_exactly_at_floor_is_ok():
    row = _row(parity=FLOOR)
    assert _row_status(row, FLOOR) == "ok"


if __name__ == "__main__":  # allow `python corpus/test_bench.py` without pytest
    import pytest

    raise SystemExit(pytest.main([__file__, "-q"]))
