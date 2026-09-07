#!/usr/bin/env python3
"""rstest test gate: end-to-end assertions for every shipped behavior.

Hermetic: builds its own venv (worker runtime deps) and fixture suites.
Usage: python3 e2e/gate.py [--binary target/release/rstest]

Exit 0 = all gates green. Designed to be the single CI entry point.
"""

import argparse
import sys
from pathlib import Path

import _harness
from _harness import REPO, WINDOWS, Gate, make_venv
from gates.coverage import (
    gate_coverage,
    gate_coverage_based_selection_changed_uses_th,
    gate_coverage_contexts_line_test_index_cov_co,
    gate_coverage_selection_under_autocrlf_crlf_w,
    gate_smart_selection,
)
from gates.dispatch import (
    gate_auto_worker_capping,
    gate_dist_each,
    gate_dist_validation,
    gate_duration_regression_gate,
    gate_lazy_collection,
    gate_lf,
    gate_loadscope_loadgroup,
    gate_native_timeout,
    gate_serial_mark,
    gate_shard_k_n,
    gate_shuffle,
    gate_worker_timeout_watchdog,
    gate_x_maxfail,
)
from gates.flaky import (
    gate_flaky_aware_reruns_reruns_only_known_fla,
    gate_flaky_marks_only_rerun,
    gate_flaky_reruns,
    gate_quarantine,
)
from gates.incremental import (
    gate_incremental_dispatch_skip,
    gate_incremental_guards,
    gate_since_green_incremental,
)
from gates.misc import (
    gate_basics,
    gate_collection_error_semantics,
)
from gates.monorepo import (
    gate_monorepo,
    gate_shared_cache_backend,
    gate_tool_rstest_config,
)
from gates.plugins import (
    gate_crash_handling,
    gate_doctest_modules,
    gate_interpreter_probe_cache_heals_after_deps,
    gate_multiprocessing_spawn_children,
    gate_one_arg_pytest_testnodedown,
    gate_pytest_randomly_real_plugin,
    gate_pytest_rerunfailures_xdist_no_sock_port_,
    gate_pytest_retry_xdist_server_port_self_prov,
    gate_testnodedown_for_crashed_workers,
    gate_xdist_master_side_hooks,
)
from gates.reporting import (
    gate_collect_only_discovery_json,
    gate_doctor,
    gate_durations,
    gate_failure_output,
    gate_html_report,
    gate_junitxml,
    gate_output_styles,
    gate_report_json_contract,
    gate_resource_leak_detection,
    gate_warnings,
)
from gates.serve_watch import (
    gate_migrate_check,
    gate_serve,
    gate_try,
    gate_watch_mode,
)


def main():
    ap = argparse.ArgumentParser()
    default_binary = REPO / "target" / "release" / ("rstest.exe" if WINDOWS else "rstest")
    ap.add_argument("--binary", default=str(default_binary))
    ap.add_argument("--venv", default=str(REPO / ".gate-venv"))
    ap.add_argument(
        "--only",
        default="",
        help="run only sections whose name contains this substring (dev iteration; "
        "sections run in order, so a section that reuses an earlier one's fixtures "
        "may need a broader filter). Use --list to see names.",
    )
    ap.add_argument("--list", action="store_true", help="list section names and exit")
    args = ap.parse_args()

    sections = (
        gate_basics,
        gate_collection_error_semantics,
        gate_output_styles,
        gate_multiprocessing_spawn_children,
        gate_crash_handling,
        gate_report_json_contract,
        gate_collect_only_discovery_json,
        gate_pytest_randomly_real_plugin,
        gate_pytest_rerunfailures_xdist_no_sock_port_,
        gate_pytest_retry_xdist_server_port_self_prov,
        gate_interpreter_probe_cache_heals_after_deps,
        gate_lazy_collection,
        gate_serial_mark,
        gate_failure_output,
        gate_x_maxfail,
        gate_lf,
        gate_junitxml,
        gate_html_report,
        gate_shard_k_n,
        gate_dist_each,
        gate_dist_validation,
        gate_testnodedown_for_crashed_workers,
        gate_xdist_master_side_hooks,
        gate_one_arg_pytest_testnodedown,
        gate_durations,
        gate_doctest_modules,
        gate_monorepo,
        gate_warnings,
        gate_doctor,
        gate_resource_leak_detection,
        gate_auto_worker_capping,
        gate_coverage,
        gate_coverage_contexts_line_test_index_cov_co,
        gate_smart_selection,
        gate_since_green_incremental,
        gate_incremental_dispatch_skip,
        gate_incremental_guards,
        gate_coverage_based_selection_changed_uses_th,
        gate_coverage_selection_under_autocrlf_crlf_w,
        gate_shuffle,
        gate_duration_regression_gate,
        gate_shared_cache_backend,
        gate_tool_rstest_config,
        gate_flaky_reruns,
        gate_flaky_aware_reruns_reruns_only_known_fla,
        gate_quarantine,
        gate_loadscope_loadgroup,
        gate_flaky_marks_only_rerun,
        gate_worker_timeout_watchdog,
        gate_native_timeout,
        gate_try,
        gate_migrate_check,
        gate_watch_mode,
        gate_serve,
    )
    names = [s.__name__.removeprefix("gate_") for s in sections]
    if args.list:
        print("\n".join(names))
        return
    selected = [s for s in sections if not args.only or args.only in s.__name__]
    if not selected:
        sys.exit(f"--only {args.only!r} matched no section; --list to see names")
    if args.only:
        print(f"running {len(selected)}/{len(sections)} sections matching {args.only!r}")

    binary = Path(args.binary).resolve()
    assert binary.exists(), f"binary missing: {binary} (cargo build --release first)"
    make_venv(Path(args.venv))
    g = Gate(binary, Path(args.venv).resolve())

    for _section in selected:
        _section(g, args, binary)

    print(f"\n{_harness.PASS} ok, {len(_harness.FAIL)} failed")
    if _harness.FAIL:
        print("FAILED:", ", ".join(_harness.FAIL))
        sys.exit(1)


if __name__ == "__main__":
    main()
