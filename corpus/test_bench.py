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

import json

from bench import _baseline_unusable, _parity_row, _render_sweep, _row_status, render
from procmem import linear_fit, suspended
from run import Suite, diff

FLOOR = 99.5


def _write_snap(path, tests, collect_errors=()):
    path.write_text(json.dumps({"tests": tests, "collect_errors": list(collect_errors)}))
    return str(path)


def _row(
    base_err=0, base_tests=100, cand_err=0, parity=100.0, missing=0, mismatch=0, unexplained=0
):
    # `missing`/`mismatch` = per-test diff counts: tests the baseline had that
    # rstest lost, and shared tests whose outcome diverged. `unexplained` = extra
    # tests rstest collected from a module the baseline collected FINE (phantom
    # over-collection); extras from a module the baseline dropped don't count.
    return {
        "baseline_collect_errors": base_err,
        "baseline_tests": base_tests,
        "candidate_collect_errors": cand_err,
        "parity": parity,
        "parity_missing": missing,
        "parity_mismatch": mismatch,
        "parity_unexplained": unexplained,
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
    row = _row(base_err=5, base_tests=900, cand_err=5, parity=90.0, mismatch=1)
    assert _row_status(row, FLOOR) == "regression"


def test_regression_on_collectible_portion_of_partly_broken_baseline():
    # REGRESSION GUARD: baseline has 1 collect error but 999 usable tests; rstest
    # regresses one of them (a shared-test mismatch). The partial breakage must
    # not waive the gate.
    row = _row(base_err=1, base_tests=999, cand_err=0, parity=90.0, mismatch=1)
    assert _row_status(row, FLOOR) == "regression"


def test_partial_baseline_superset_is_investigate():
    # REGRESSION GUARD: baseline drifted (5 collect errors) but still collected
    # 900 tests; rstest collected a SUPERSET (lost none, diverged on none — its
    # extra tests just drag parity under the floor). That is the baseline's
    # missing coverage, not an rstest fault -> investigate, not gated.
    row = _row(base_err=5, base_tests=900, cand_err=0, parity=95.0, missing=0, mismatch=0)
    assert _row_status(row, FLOOR) == "investigate"


def test_partial_baseline_with_lost_tests_is_regression():
    # Same drifting baseline, but rstest actually LOST tests it should have had
    # (missing > 0) -> not a superset -> gated.
    row = _row(base_err=5, base_tests=900, cand_err=0, parity=95.0, missing=3)
    assert _row_status(row, FLOOR) == "regression"


def test_partial_baseline_with_unexplained_extras_is_regression():
    # REGRESSION GUARD: baseline drifted, but rstest's extra tests come from
    # modules the baseline collected FINE (phantom over-collection, unexplained >
    # 0). The superset carve-out must NOT waive a genuine over-collection fault
    # just because the baseline happened to drift elsewhere.
    row = _row(base_err=5, base_tests=900, cand_err=0, parity=95.0, unexplained=4)
    assert _row_status(row, FLOOR) == "regression"


def test_all_healthy_is_ok():
    row = _row(base_err=0, base_tests=500, cand_err=0, parity=100.0)
    assert _row_status(row, FLOOR) == "ok"


def test_parity_exactly_at_floor_is_ok():
    row = _row(parity=FLOOR)
    assert _row_status(row, FLOOR) == "ok"


# ---- diff() -> row wiring: the keys _parity_row/_row_status rely on exist -----


def test_diff_emits_collect_error_and_unexplained_keys(tmp_path):
    base = _write_snap(tmp_path / "b.json", {"t1": {"call": "passed"}}, collect_errors=["mod_x.py"])
    cand = _write_snap(tmp_path / "c.json", {"t1": {"call": "passed"}})
    d = diff(base, cand)
    for key in ("baseline_collect_errors", "candidate_collect_errors", "extra_unexplained_count"):
        assert key in d, f"diff() dropped {key} — the bench gate reads it"
    assert d["baseline_collect_errors"] == 1
    assert d["candidate_collect_errors"] == 0
    assert d["extra_unexplained_count"] == 0


def test_diff_extra_from_dropped_module_is_explained(tmp_path):
    # rstest collected mod_x's test that the baseline dropped at collection ->
    # extra, but EXPLAINED -> _parity_row feeds a superset -> investigate.
    base = _write_snap(tmp_path / "b.json", {"keep.py::t1": {"call": "passed"}}, ["mod_x.py"])
    cand = _write_snap(
        tmp_path / "c.json",
        {"keep.py::t1": {"call": "passed"}, "mod_x.py::t2": {"call": "passed"}},
    )
    row = _parity_row(base, cand)
    assert row["parity_unexplained"] == 0
    assert row["parity"] < 100.0  # the extra dragged parity down
    assert _row_status(row, FLOOR) == "investigate"


def test_diff_extra_from_healthy_module_is_unexplained(tmp_path):
    # rstest collected an extra from a module the baseline collected FINE (only
    # mod_x errored) -> unexplained -> _row_status gates it.
    base = _write_snap(tmp_path / "b.json", {"keep.py::t1": {"call": "passed"}}, ["mod_x.py"])
    cand = _write_snap(
        tmp_path / "c.json",
        {"keep.py::t1": {"call": "passed"}, "keep.py::t_phantom": {"call": "passed"}},
    )
    row = _parity_row(base, cand)
    assert row["parity_unexplained"] == 1
    assert _row_status(row, FLOOR) == "regression"


# ---- sweep rendering: matched-n xdist column, ties are ties -----------------


def _point(n, rs, xd=None):
    """A sweep point: rstest (median, min, max), optional xdist likewise."""
    med, lo, hi = rs
    p = {
        "workers": n,
        "rstest_wall": med,
        "rstest_band": f"{med} ({lo}-{hi})",
        "min": lo,
        "max": hi,
        "speedup": round(10 / med, 2),
        "efficiency": round(10 / med / n, 2),
        **_row(),
    }
    if xd:
        xm, xl, xh = xd
        p["xdist"] = {
            "wall": xm,
            "band": f"{xm} ({xl}-{xh})",
            "min": xl,
            "max": xh,
            "speedup": round(10 / xm, 2),
            "efficiency": round(10 / xm / n, 2),
            "parity": 100.0,
        }
    return p


def _sweep_table(points, gated=True):
    sweep = {"suite": "s", "pytest_wall": 10.0, "pytest_band": "10.0 (9.9-10.1)", "points": points}
    return "\n".join(_render_sweep(sweep, 2.0, FLOOR, gated))


def test_sweep_overlapping_spreads_render_as_parity():
    out = _sweep_table([_point(4, (3.0, 2.9, 3.2), xd=(3.1, 3.0, 3.3))])
    assert "| parity |" in out
    assert "| rstest |" not in out


def test_sweep_disjoint_spreads_name_the_faster_runner():
    out = _sweep_table([_point(4, (3.0, 2.9, 3.1), xd=(4.0, 3.9, 4.1))])
    assert "| rstest |" in out


def test_sweep_without_xdist_has_no_xdist_columns():
    out = _sweep_table([_point(2, (5.0, 5.0, 5.0))])
    assert "xdist" not in out
    assert "| 100% |" in out  # efficiency = speedup / n = 2.0 / 2


def test_only_first_sweep_is_gated():
    assert "(floor 2.0x)" in _sweep_table([_point(2, (5.0, 5.0, 5.0))], gated=True)
    assert "(not gated)" in _sweep_table([_point(2, (5.0, 5.0, 5.0))], gated=False)


def test_render_accepts_single_sweep_dict_and_list():
    sweep = {"suite": "s", "pytest_wall": 10.0, "points": [_point(2, (5.0, 5.0, 5.0))]}
    assert render([], sweep, 2.0, FLOOR) == render([], [sweep], 2.0, FLOOR)


# ---- memory model fit ----------------------------------------------------------


def test_linear_fit_recovers_fixed_and_per_worker_cost():
    xs = [1, 2, 4, 8]
    a, b, r2 = linear_fit(xs, [12 + 106 * x for x in xs])
    assert round(a, 6) == 12
    assert round(b, 6) == 106
    assert r2 == 1.0


def test_linear_fit_single_x_does_not_divide_by_zero():
    a, b, _ = linear_fit([4, 4], [100, 110])
    assert b == 0.0
    assert a == 105


def test_suspended_flags_a_run_that_straddled_sleep():
    # Wall clock ran 20 min longer than the monotonic clock: the machine slept.
    assert suspended(1285.0, 85.0) is True


def test_suspended_ignores_clock_noise():
    assert suspended(85.3, 85.0) is False


# ---- Suite command lines -------------------------------------------------------


def test_rstest_argv_workers_override_suite_policy(tmp_path):
    suite = Suite("x", {"rstest_args": ["-n", "4", "--collect", "lazy"]}, "w.whl", "rstest")
    argv = suite.rstest_argv(tmp_path / "s.json", workers=2)
    assert argv.count("-n") == 1
    assert argv[argv.index("-n") + 1] == "2"
    assert "--collect" in argv


def test_rstest_argv_default_keeps_suite_policy(tmp_path):
    suite = Suite("x", {"rstest_args": ["-n", "4"]}, "w.whl", "rstest")
    argv = suite.rstest_argv(tmp_path / "s.json")
    assert argv[argv.index("-n") + 1] == "4"


if __name__ == "__main__":  # allow `python corpus/test_bench.py` without pytest
    import pytest

    raise SystemExit(pytest.main([__file__, "-q"]))
