use super::*;
use testutil::record_test;

fn teardown_with_leak(run: &mut Run, nodeid: &str, threads: Option<i64>, fds: Option<i64>) {
    let rep = |when: &str, td: Option<i64>, fd: Option<i64>| crate::scheduling::proto::Report {
        nodeid: nodeid.into(),
        when: when.into(),
        outcome: "passed".into(),
        duration: 0.1,
        longrepr: None,
        wasxfail: false,
        skip_reason: None,
        cpu: None,
        thread_delta: td,
        fd_delta: fd,
        sections: Vec::new(),
        lineno: None,
        subtest: false,
        crash: None,
        cause: None,
        xfail_reason: None,
        skip_location: None,
        network: Vec::new(),
        marker_timeout: None,
        no_rerun: false,
        subtest_desc: None,
        subtest_shown: false,
    };
    run.record(None, rep("setup", None, None));
    run.record(None, rep("call", None, None));
    run.record(None, rep("teardown", threads, fds));
}

#[test]
fn network_access_ranks_most_destinations_first_and_keeps_top_20() {
    let mut run = Run::default();
    assert!(network_access(&run).is_none());
    for i in 0..22 {
        run.network
            .insert(format!("t.py::one_{i:02}"), vec!["a:80".into()]);
    }
    run.network
        .insert("t.py::two".into(), vec!["a:80".into(), "b:443".into()]);
    let n = network_access(&run).unwrap();
    assert_eq!(n.tests_with_network, 23);
    let ids: Vec<&str> = n.tests.iter().map(|t| t.nodeid.as_str()).collect();
    assert_eq!(ids.len(), 20);
    // Most destinations first, then by nodeid.
    assert_eq!(ids[..3], ["t.py::two", "t.py::one_00", "t.py::one_01"]);
    assert_eq!(ids[19], "t.py::one_18");
}

#[test]
fn detect_leaks_flags_positive_deltas_worst_first() {
    let mut run = Run::default();
    teardown_with_leak(&mut run, "t.py::clean", None, None);
    teardown_with_leak(&mut run, "t.py::released", Some(0), Some(0)); // opened+closed
    teardown_with_leak(&mut run, "t.py::one_fd", None, Some(1));
    teardown_with_leak(&mut run, "t.py::big", Some(3), Some(2)); // worst
    teardown_with_leak(&mut run, "t.py::negative", Some(-1), None); // freed, not a leak

    let leaks = detect_leaks(&run);
    let ids: Vec<&str> = leaks.iter().map(|l| l.nodeid.as_str()).collect();
    assert_eq!(
        ids,
        vec!["t.py::big", "t.py::one_fd"],
        "only real leaks, worst first"
    );
    assert_eq!((leaks[0].threads, leaks[0].fds), (3, 2));
}

#[test]
fn detect_leaks_includes_fixture_leaks_summed_over_workers() {
    // R16: a wide fixture that leaked past its scope end is a leak too,
    // labelled as a fixture; workers' reports of it add up.
    let mut run = Run::default();
    teardown_with_leak(&mut run, "t.py::one_fd", None, Some(1));
    let leak = |threads, fds| crate::scheduling::proto::FixtureLeak {
        fixture: "tests::server".into(),
        scope: "module".into(),
        threads,
        fds,
        instances: 2,
    };
    run.add_fixture_leaks(vec![leak(2, 0)]); // worker 0
    run.add_fixture_leaks(vec![leak(1, 1)]); // worker 1

    let leaks = detect_leaks(&run);
    let labels: Vec<String> = leaks.iter().map(Leak::label).collect();
    assert_eq!(labels, ["[module fixture] tests::server", "t.py::one_fd"]);
    assert_eq!((leaks[0].threads, leaks[0].fds), (3, 1));
    let json = serde_json::to_value(&leaks).unwrap();
    assert_eq!(json[0]["scope"], "module");
    assert!(json[1].get("scope").is_none(), "absent for a test");
}

/// One test whose time is mostly fixture setup/teardown, with per-phase
/// wall and CPU as the worker reports them.
fn record_phases(run: &mut Run, nodeid: &str, worker: usize, phases: [(f64, f64); 3]) {
    for (when, (duration, cpu)) in ["setup", "call", "teardown"].into_iter().zip(phases) {
        run.record(
            Some(worker),
            crate::scheduling::proto::Report {
                nodeid: nodeid.into(),
                when: when.into(),
                outcome: "passed".into(),
                duration,
                longrepr: None,
                wasxfail: false,
                skip_reason: None,
                cpu: Some(cpu),
                thread_delta: None,
                fd_delta: None,
                sections: Vec::new(),
                lineno: None,
                subtest: false,
                crash: None,
                cause: None,
                xfail_reason: None,
                skip_location: None,
                network: Vec::new(),
                marker_timeout: None,
                no_rerun: false,
                subtest_desc: None,
                subtest_shown: false,
            },
        );
    }
}

#[test]
fn fixture_time_counts_toward_every_number() {
    // 8 tests on 4 workers, each 0.25s setup + 0.02s call + 0.1s teardown,
    // all sleeping: the work is 2.96s, not the 0.16s of call phases.
    let mut run = Run::default();
    for i in 0..8 {
        record_phases(
            &mut run,
            &format!("t.py::t{i}"),
            i % 4,
            [(0.25, 0.0), (0.02, 0.0), (0.1, 0.0)],
        );
    }
    let r = analyze(&run, &[], 0.8, 0.0, false, 4, None);
    assert!(
        (r.test_time_seconds - 2.96).abs() < 1e-6,
        "{}",
        r.test_time_seconds
    );
    assert!((r.slowest_files[0].total_seconds - 2.96).abs() < 1e-6);
    let pe = r.parallel_efficiency.as_ref().unwrap();
    assert!((pe.long_pole_seconds - 0.37).abs() < 1e-6);
    assert!((pe.workers_busy[0].busy_seconds - 0.74).abs() < 1e-6);
    assert!((pe.realized_speedup - 3.7).abs() < 1e-6);
    // Fixture sleeps are waiting: shown and gateable.
    let wb = r.wait_bound.as_ref().expect("wait-bound section");
    assert!((wb.wait_pct - 100.0).abs() < 1e-6);
    assert_eq!(wb.tests.len(), 8);
    assert_eq!(r.long_pole_seconds, Some(0.37));
}

#[test]
fn fixture_cpu_is_computing_not_waiting() {
    // A CPU-heavy fixture: setup/teardown CPU equals their wall, so the
    // test is not waiting.
    let mut run = Run::default();
    record_phases(
        &mut run,
        "t.py::hot",
        0,
        [(0.6, 0.6), (0.01, 0.01), (0.4, 0.4)],
    );
    let r = analyze(&run, &[], 1.1, 0.0, false, 1, None);
    assert!(r.wait_bound.is_none());
    let (wait, pct) = r.measured_wait.expect("cpu measured");
    assert!(wait.abs() < 1e-6 && pct.abs() < 1e-6, "{wait} {pct}");
}

#[test]
fn single_worker_run_reports_one_worker_and_gateable_numbers() {
    // `-n 0` passes 0 workers: the report still says 1, keeps the long
    // pole, and measures wait below the WAIT-BOUND display threshold.
    let mut run = Run::default();
    record_phases(
        &mut run,
        "t.py::sleep",
        0,
        [(0.0, 0.0), (0.8, 0.0), (0.0, 0.0)],
    );
    record_phases(
        &mut run,
        "t.py::cpu",
        0,
        [(0.0, 0.0), (1.0, 1.0), (0.0, 0.0)],
    );
    let r = analyze(&run, &[], 1.9, 0.0, false, 0, None);
    assert_eq!(r.workers, 1);
    assert!(r.parallel_efficiency.is_none());
    assert!(
        r.wait_bound.is_none(),
        "0.8s wait is below the 1s display floor"
    );
    let (wait, pct) = r.measured_wait.unwrap();
    assert!((wait - 0.8).abs() < 1e-6 && (pct - 44.444).abs() < 0.01);
    assert_eq!(r.long_pole_seconds, Some(1.0));
}

#[test]
fn all_work_on_one_worker_reports_max_imbalance() {
    // -n 8 but every test lands on gw0: the seven idle workers are
    // absent from the per-worker map, yet imbalance must read ~100%,
    // not 0%.
    let mut run = Run::default();
    for i in 0..4 {
        record_test(&mut run, &format!("t.py::t{i}"), 0, 2.0);
    }
    let pe = analyze(&run, &[], 8.0, 0.0, false, 8, None)
        .parallel_efficiency
        .expect("multi-worker run has efficiency");
    assert_eq!(pe.workers_busy.len(), 1);
    assert!(
        (pe.imbalance_pct - 100.0).abs() < 1e-6,
        "imbalance {} should be ~100%",
        pe.imbalance_pct
    );
    // test_time 8.0 over wall 8.0 => 1× realized of 8× possible.
    assert!((pe.realized_speedup - 1.0).abs() < 1e-6);
    assert_eq!(pe.ideal_speedup, 8);
    assert!((pe.efficiency_pct - 12.5).abs() < 1e-6);
    assert!((pe.long_pole_seconds - 2.0).abs() < 1e-6);
}

#[test]
fn balanced_pool_is_not_a_parallel_floor() {
    // Four ~1s tests on four workers: the longest sits at the ideal share
    // plus jitter, which is not a floor.
    let mut run = Run::default();
    for (i, d) in [1.0047, 1.0012, 1.0031, 1.0008].into_iter().enumerate() {
        record_test(&mut run, &format!("t.py::t{i}"), i, d);
    }
    assert!(analyze(&run, &[], 1.1, 0.0, false, 4, None)
        .parallel_floor
        .is_none());
    // One test clearly longer than the share still is.
    let mut run = Run::default();
    record_test(&mut run, "t.py::long", 0, 4.0);
    record_test(&mut run, "t.py::short", 1, 0.5);
    let floor = analyze(&run, &[], 4.0, 0.0, false, 2, None)
        .parallel_floor
        .expect("4s test over a 2.25s share is a floor");
    assert_eq!(floor.gate_tests.len(), 1);
}

#[test]
fn balanced_workers_report_low_imbalance() {
    let mut run = Run::default();
    record_test(&mut run, "t.py::a", 0, 10.0);
    record_test(&mut run, "t.py::b", 1, 10.0);
    let pe = analyze(&run, &[], 10.0, 0.0, false, 2, None)
        .parallel_efficiency
        .expect("multi-worker run has efficiency");
    assert_eq!(pe.workers_busy.len(), 2);
    assert!(
        pe.imbalance_pct.abs() < 1e-6,
        "imbalance {} should be 0%",
        pe.imbalance_pct
    );
    // 20.0s test time over 10.0s wall => 2× of 2× possible.
    assert!((pe.realized_speedup - 2.0).abs() < 1e-6);
    assert!((pe.efficiency_pct - 100.0).abs() < 1e-6);
}

#[test]
fn some_idle_workers_still_counted() {
    // 4 workers configured, 3 active, one loaded heavier: min must be
    // the idle 0, so imbalance reflects the heaviest vs idle gap.
    let mut run = Run::default();
    record_test(&mut run, "t.py::a", 0, 8.0);
    record_test(&mut run, "t.py::b", 1, 4.0);
    record_test(&mut run, "t.py::c", 2, 4.0);
    let pe = analyze(&run, &[], 9.0, 0.0, false, 4, None)
        .parallel_efficiency
        .expect("multi-worker run has efficiency");
    assert_eq!(pe.workers_busy.len(), 3);
    // max 8.0, min 0.0 (idle gw3) => 100%.
    assert!((pe.imbalance_pct - 100.0).abs() < 1e-6);
}

/// Line -> nodeids that covered it (test fixture shorthand).
type LineSpec<'a> = (u32, &'a [&'a str]);
/// (file path, lines) for one file in a test index.
type FileSpec<'a> = (&'a str, &'a [LineSpec<'a>]);

/// Build a coverage index from `file -> [(line, [nodeids])]` (hash unused
/// by the waste analysis, so a fixed placeholder is fine).
fn cov(files: &[FileSpec]) -> CoverageIndex {
    use crate::select::CoverageFile;
    let mut idx = CoverageIndex {
        schema: 1,
        files: HashMap::new(),
    };
    for (path, lines) in files {
        let mut lm = HashMap::new();
        for (ln, ids) in *lines {
            lm.insert(*ln, ids.iter().map(|s| s.to_string()).collect());
        }
        idx.files.insert(
            (*path).to_string(),
            CoverageFile {
                hash: "H".into(),
                lines: lm,
            },
        );
    }
    idx
}

/// The fixtures' only test file is `t.py`; everything else is product code.
fn test_py(file: &str) -> bool {
    file == "t.py"
}

fn durs<'a>(pairs: &[(&'a str, f64)]) -> HashMap<&'a str, f64> {
    pairs.iter().map(|(id, d)| (*id, *d)).collect()
}

#[test]
fn zero_unique_slow_test_is_flagged_as_waste() {
    // test_dup covers only lines test_keep also covers (mod.py:1-2), so it
    // adds no unique coverage. test_keep owns a unique line (mod.py:3).
    let idx = cov(&[(
        "mod.py",
        &[
            (1, &["t.py::test_dup", "t.py::test_keep"]),
            (2, &["t.py::test_dup", "t.py::test_keep"]),
            (3, &["t.py::test_keep"]),
        ],
    )]);
    let d = durs(&[("t.py::test_dup", 5.0), ("t.py::test_keep", 5.0)]);
    let cw = coverage_waste(&d, &idx, WASTE_MIN_SECONDS, test_py).expect("a waste candidate");
    assert_eq!(cw.redundant_tests, 1);
    assert_eq!(cw.tests.len(), 1);
    assert_eq!(cw.tests[0].nodeid, "t.py::test_dup");
    assert_eq!(cw.tests[0].covered_lines, 2);
    // Its two lines are shared with exactly one other test (test_keep).
    assert_eq!(cw.tests[0].also_covered_by, 1);
    assert!((cw.wasted_seconds - 5.0).abs() < 1e-6);
}

#[test]
fn a_fast_redundant_test_is_below_the_floor() {
    // Fully redundant, but too fast to be worth deleting for time.
    let idx = cov(&[("mod.py", &[(1, &["t.py::test_dup", "t.py::test_keep"])])]);
    let d = durs(&[("t.py::test_dup", 0.1), ("t.py::test_keep", 0.1)]);
    assert!(coverage_waste(&d, &idx, WASTE_MIN_SECONDS, test_py).is_none());
}

#[test]
fn a_test_with_any_unique_line_is_not_waste() {
    // test_a shares line 1 but owns line 2 - not redundant despite being slow.
    let idx = cov(&[(
        "mod.py",
        &[
            (1, &["t.py::test_a", "t.py::test_b"]),
            (2, &["t.py::test_a"]),
        ],
    )]);
    let d = durs(&[("t.py::test_a", 9.0), ("t.py::test_b", 9.0)]);
    let cw = coverage_waste(&d, &idx, WASTE_MIN_SECONDS, test_py).expect("test_b is redundant");
    let ids: Vec<&str> = cw.tests.iter().map(|t| t.nodeid.as_str()).collect();
    assert_eq!(ids, vec!["t.py::test_b"], "only the fully-shared test");
}

#[test]
fn a_tests_own_file_lines_dont_count_as_unique_coverage() {
    // Under `--cov=.` the index also records each test's own body (a line
    // only that test runs). If those counted, no test would ever look
    // redundant. Here both tests cover the same product line (mod.py:1) and
    // each also "covers" its own test-file body line - which must be ignored,
    // so test_dup still reads as pure product-redundant.
    let idx = cov(&[
        ("mod.py", &[(1, &["t.py::test_dup", "t.py::test_keep"])]),
        // The test file itself, each test's own line (self-only coverage):
        (
            "t.py",
            &[(1, &["t.py::test_dup"]), (2, &["t.py::test_keep"])],
        ),
    ]);
    let d = durs(&[("t.py::test_dup", 5.0), ("t.py::test_keep", 5.0)]);
    let cw = coverage_waste(&d, &idx, WASTE_MIN_SECONDS, test_py)
        .expect("still redundant on product code");
    let ids: Vec<&str> = cw.tests.iter().map(|t| t.nodeid.as_str()).collect();
    // mod.py:1 is shared, so one of the pair is product-redundant (the tie
    // breaks on nodeid); the other must stay to keep the line covered.
    assert_eq!(ids, vec!["t.py::test_dup"]);
    // covered_lines counts PRODUCT lines only (1), not the test-file body.
    let dup = cw
        .tests
        .iter()
        .find(|t| t.nodeid == "t.py::test_dup")
        .unwrap();
    assert_eq!(dup.covered_lines, 1);
}

#[test]
fn identical_coverers_yield_one_candidate_not_both() {
    // test_a and test_b cover exactly the same lines. Each is individually
    // redundant, but deleting BOTH drops mod.py:1-2, so only the slower one
    // (test_a) is a candidate and only its time is reclaimable.
    let idx = cov(&[(
        "mod.py",
        &[
            (1, &["t.py::test_a", "t.py::test_b"]),
            (2, &["t.py::test_a", "t.py::test_b"]),
        ],
    )]);
    let d = durs(&[("t.py::test_a", 7.0), ("t.py::test_b", 3.0)]);
    let cw = coverage_waste(&d, &idx, WASTE_MIN_SECONDS, test_py).expect("one is redundant");
    let ids: Vec<&str> = cw.tests.iter().map(|t| t.nodeid.as_str()).collect();
    assert_eq!(ids, vec!["t.py::test_a"]);
    assert_eq!(cw.redundant_tests, 1);
    assert!((cw.wasted_seconds - 7.0).abs() < 1e-6);
}

#[test]
fn a_nodeid_absent_from_the_run_is_not_a_coverer() {
    // The index says test_gone also covers mod.py:1, but it was deleted
    // since the coverage run (no duration). test_slow is now the sole
    // coverer of that line, so it is not waste.
    let idx = cov(&[("mod.py", &[(1, &["t.py::test_slow", "t.py::test_gone"])])]);
    let d = durs(&[("t.py::test_slow", 9.0)]);
    assert!(coverage_waste(&d, &idx, WASTE_MIN_SECONDS, test_py).is_none());
}

#[test]
fn a_doctest_module_is_product_code_not_a_test_file() {
    // Under --doctest-modules, pkg/mod.py hosts a doctest nodeid. It is
    // still product code: test_slow's only unique line lives there, so it
    // is not waste (a "some nodeid lives here" rule would skip the file and
    // flag test_slow as fully shared).
    let idx = cov(&[
        (
            "pkg/mod.py",
            &[
                (1, &["t.py::test_slow", "pkg/mod.py::pkg.mod.f"]),
                (7, &["t.py::test_slow"]),
            ],
        ),
        (
            "pkg/other.py",
            &[(1, &["t.py::test_slow", "t.py::test_keep"])],
        ),
    ]);
    let d = durs(&[
        ("t.py::test_slow", 9.0),
        ("t.py::test_keep", 5.0),
        ("pkg/mod.py::pkg.mod.f", 0.1),
    ]);
    let cw = coverage_waste(&d, &idx, WASTE_MIN_SECONDS, test_py);
    let ids: Vec<String> = cw
        .map(|c| c.tests.into_iter().map(|t| t.nodeid).collect())
        .unwrap_or_default();
    assert!(!ids.contains(&"t.py::test_slow".to_string()), "{ids:?}");
}

#[test]
fn a_skipped_test_is_not_a_coverer() {
    // The index says test_skip also covers mod.py:1, but this run it skipped
    // (a call-phase pytest.skip still has a duration). test_slow is the only
    // test that really covered the line, so it is not waste.
    let mut run = Run::default();
    record_test(&mut run, "tests/test_a.py::test_slow", 0, 9.0);
    for when in ["setup", "call", "teardown"] {
        run.record(
            Some(0),
            crate::scheduling::proto::Report {
                nodeid: "tests/test_a.py::test_skip".into(),
                when: when.into(),
                outcome: if when == "call" { "skipped" } else { "passed" }.into(),
                duration: if when == "call" { 9.0 } else { 0.0 },
                longrepr: None,
                wasxfail: false,
                skip_reason: None,
                cpu: None,
                thread_delta: None,
                fd_delta: None,
                sections: Vec::new(),
                lineno: None,
                subtest: false,
                crash: None,
                cause: None,
                xfail_reason: None,
                skip_location: None,
                network: Vec::new(),
                marker_timeout: None,
                no_rerun: false,
                subtest_desc: None,
                subtest_shown: false,
            },
        );
    }
    let idx = cov(&[(
        "mod.py",
        &[(
            1,
            &["tests/test_a.py::test_slow", "tests/test_a.py::test_skip"],
        )],
    )]);
    let project = ProjectConfig::default();
    let report = analyze(&run, &[], 9.0, 0.0, false, 1, Some((&idx, &project)));
    assert!(report.coverage_waste.is_none());
    // Control: with both passing, one of the pair IS waste.
    let mut both = Run::default();
    record_test(&mut both, "tests/test_a.py::test_slow", 0, 9.0);
    record_test(&mut both, "tests/test_a.py::test_skip", 0, 9.0);
    assert!(
        analyze(&both, &[], 9.0, 0.0, false, 1, Some((&idx, &project)))
            .coverage_waste
            .is_some()
    );
}

#[test]
fn no_coverage_index_yields_no_section() {
    // analyze without an index never produces a waste section.
    let mut run = Run::default();
    record_test(&mut run, "t.py::a", 0, 5.0);
    assert!(analyze(&run, &[], 5.0, 0.0, false, 1, None)
        .coverage_waste
        .is_none());
}

fn fstat(name: &str, count: u64, total: f64, constant: bool, redundant: f64) -> FixtureStat {
    FixtureStat {
        name: name.into(),
        scope: "function".into(),
        count,
        total,
        constant,
        repeated: redundant > 0.0,
        redundant,
        fingerprint: None,
    }
}

#[test]
fn scope_promotion_projects_largest_per_worker_saving() {
    let run = Run::default();
    // Merged stat: the busiest session skipped 1.1s of repeat setup.
    let fixtures = vec![fstat("cfg", 40, 4.0, true, 1.1)];
    let r = analyze(&run, &fixtures, 10.0, 0.0, false, 4, None);
    let e = r.fixtures.iter().find(|f| f.name == "cfg").unwrap();
    assert!(e.constant);
    assert!((e.projected_saving_seconds - 1.1).abs() < 1e-9);
}

#[test]
fn non_constant_and_unrepeated_fixtures_project_no_saving() {
    let run = Run::default();
    let fixtures = vec![
        // Value varied in some session => never a candidate.
        fstat("varies", 40, 4.0, false, 0.0),
        // Never varied, but no session ran it twice (e.g. 5 sessions
        // after a respawn, one call each): no evidence, nothing to save.
        fstat("once_each", 5, 5.0, true, 0.0),
    ];
    let r = analyze(&run, &fixtures, 10.0, 0.0, false, 4, None);
    for f in &r.fixtures {
        assert_eq!(f.projected_saving_seconds, 0.0, "{}", f.name);
        assert!(!f.constant, "{}", f.name);
    }
}

#[test]
fn json_omits_zero_saving_and_false_constant() {
    let r = testutil::report(12);
    let v = serde_json::to_value(&r).unwrap();
    let fixtures = v["fixtures"].as_array().unwrap();
    // `db`: not constant, no saving => both fields skipped.
    let db = &fixtures[0];
    assert_eq!(db["name"], "db");
    assert!(db.get("constant").is_none());
    assert!(db.get("projected_saving_seconds").is_none());
    // `settings`: a candidate => both fields serialized.
    let settings = &fixtures[1];
    assert_eq!(settings["name"], "settings");
    assert_eq!(settings["constant"], true);
    assert_eq!(settings["projected_saving_seconds"], 0.9);
}

#[cfg(test)]
mod grouping_tests {
    use super::*;

    fn group(scope: &str, pieces: usize, chunked: usize, secs: f64) -> FixtureGroup {
        FixtureGroup {
            scope: scope.into(),
            tests: 10,
            pieces,
            setup_seconds: secs,
            chunked_setups: chunked,
        }
    }

    #[test]
    fn grouping_saves_the_setups_chunking_would_have_repeated() {
        assert!(fixture_grouping(&Run::default(), &[]).is_none());
        let mut run = Run::default();
        run.add_scope_costs(vec![
            crate::scheduling::proto::ScopeCost {
                key: "db.py".into(),
                total: 0.6,
                count: 1,
            },
            // Set up more often than chunking would have (reruns): no saving,
            // never a negative one.
            crate::scheduling::proto::ScopeCost {
                key: "m.py::T".into(),
                total: 3.0,
                count: 3,
            },
        ]);
        let g = fixture_grouping(
            &run,
            &[
                group("m.py::T", 1, 2, 1.0),
                group("db.py", 1, 4, 0.5),
                group("gone.py", 2, 3, 2.0),
            ],
        )
        .unwrap();
        let rows: Vec<(&str, usize, f64)> = g
            .groups
            .iter()
            .map(|e| (e.scope.as_str(), e.setups, e.saved_seconds))
            .collect();
        // gone.py reported no setups: its 2 planned pieces count.
        assert_eq!(
            rows,
            vec![("gone.py", 2, 2.0), ("db.py", 1, 1.5), ("m.py::T", 3, 0.0)],
            "most saved first"
        );
        assert_eq!(g.saved_seconds, 3.5);
    }
}

/// Report builders shared by the `analyze`/`gate`/`render` test modules. A
/// descendant of the types' module, so it can populate their private fields.
pub(crate) mod testutil {
    use super::super::*;
    use crate::reporting::report::Run;

    pub fn report(tests: usize) -> DoctorReport {
        DoctorReport {
            schema: SCHEMA_VERSION,
            rstest_version: "test",
            workers: 4,
            wall_seconds: 9.0,
            startup_seconds: 0.3,
            fork_prewarm: false,
            warm: None,
            tests,
            test_time_seconds: 30.0,
            cpu_time_seconds: 6.0,
            measured_wait: Some((24.0, 80.0)),
            long_pole_seconds: Some(8.4),
            wait_bound: Some(WaitBound {
                wait_seconds: 24.0,
                wait_pct: 80.0,
                tests: vec![WaitTest {
                    nodeid: "tests/test_a.py::test_sleepy".into(),
                    duration: 5.1,
                    wait: 5.0,
                }],
            }),
            parallel_floor: Some(ParallelFloor {
                longest_seconds: 8.4,
                ideal_share_seconds: 7.5,
                gate_tests: vec![GateTest {
                    nodeid: "tests/test_a.py::test_long".into(),
                    duration: 8.4,
                }],
            }),
            parallel_efficiency: Some(ParallelEfficiency {
                realized_speedup: 3.3,
                ideal_speedup: 4,
                efficiency_pct: 82.5,
                workers_busy: vec![
                    WorkerLoad {
                        worker: "gw0".into(),
                        busy_seconds: 16.0,
                        tests: 6,
                    },
                    WorkerLoad {
                        worker: "gw1".into(),
                        busy_seconds: 14.0,
                        tests: 6,
                    },
                ],
                imbalance_pct: 12.5,
                long_pole_seconds: 8.4,
            }),
            fixtures: vec![
                FixtureEntry {
                    name: "db".into(),
                    scope: "session".into(),
                    count: 4,
                    total_seconds: 6.1,
                    constant: false,
                    projected_saving_seconds: 0.0,
                },
                FixtureEntry {
                    name: "settings".into(),
                    scope: "function".into(),
                    count: 40,
                    total_seconds: 4.0,
                    constant: true,
                    projected_saving_seconds: 0.9,
                },
            ],
            slowest_files: vec![FileEntry {
                file: "tests/test_a.py".into(),
                total_seconds: 20.0,
                pct: 66.7,
            }],
            coverage_waste: Some(CoverageWaste {
                wasted_seconds: 12.0,
                redundant_tests: 1,
                tests: vec![WasteTest {
                    nodeid: "tests/test_a.py::test_redundant".into(),
                    duration: 12.0,
                    covered_lines: 40,
                    also_covered_by: 3,
                }],
            }),
            fixture_grouping: Some(FixtureGrouping {
                saved_seconds: 1.8,
                groups: vec![GroupEntry {
                    scope: "tests/test_db.py".into(),
                    tests: 40,
                    setups: 1,
                    chunked_setups: 4,
                    setup_seconds: 0.6,
                    saved_seconds: 1.8,
                }],
            }),
            timeouts: Some(TimeoutAdvice {
                suite_timeout: 3,
                tests_judged: 40,
                override_count: 1,
                marker_covered: 0,
                overrides: vec![TimeoutOverride {
                    nodeid: "tests/test_a.py::test_long".into(),
                    slowest_seconds: 8.4,
                    runs: 12,
                    timeout: 26,
                    current: None,
                }],
            }),
            drift: Some(Drift {
                drifting_tests: 1,
                tests: vec![DriftTest {
                    nodeid: "tests/test_a.py::test_creep".into(),
                    older_median: 0.4,
                    middle_median: 0.55,
                    newer_median: 0.7,
                    runs: 30,
                }],
            }),
            network: Some(NetworkAccess {
                tests_with_network: 1,
                tests: vec![NetworkTest {
                    nodeid: "tests/test_a.py::test_fetch".into(),
                    destinations: vec!["api.example.com:443 (93.184.215.14)".into()],
                }],
            }),
            leaks: Vec::new(),
            absorbed_waits: Vec::new(),
        }
    }

    /// Record one completed test (setup/call/teardown) on `worker` with the
    /// given call duration, mirroring what the pool feeds `Run::record`.
    pub fn record_test(run: &mut Run, nodeid: &str, worker: usize, dur: f64) {
        let r = |when: &str, duration: f64| crate::scheduling::proto::Report {
            nodeid: nodeid.into(),
            when: when.into(),
            outcome: "passed".into(),
            duration,
            longrepr: None,
            wasxfail: false,
            skip_reason: None,
            cpu: None,
            thread_delta: None,
            fd_delta: None,
            sections: Vec::new(),
            lineno: None,
            subtest: false,
            crash: None,
            cause: None,
            xfail_reason: None,
            skip_location: None,
            network: Vec::new(),
            marker_timeout: None,
            no_rerun: false,
            subtest_desc: None,
            subtest_shown: false,
        };
        run.record(Some(worker), r("setup", 0.0));
        run.record(Some(worker), r("call", dur));
        run.record(Some(worker), r("teardown", 0.0));
    }
}

mod timeout_tests {
    use super::super::*;
    use super::testutil::record_test;
    use crate::reporting::history::{outcome_code, Entry, History, RunMeta};

    /// History giving each test `runs` passing runs at `secs`, plus one
    /// failed run whose time must not count.
    fn history(tests: &[(&str, f32, usize)]) -> History {
        let mut h = History::default();
        let n = tests.iter().map(|t| t.2).max().unwrap_or(0) + 1;
        for seq in 0..n as u64 {
            h.runs.push(RunMeta {
                seq,
                id: format!("r{seq}"),
                epoch: seq,
                commit: None,
                env: String::new(),
            });
        }
        for (id, secs, runs) in tests {
            let mut series: std::collections::VecDeque<Entry> = (0..*runs)
                .map(|i| Entry {
                    run: i as u64,
                    outcome: outcome_code("passed").unwrap(),
                    call: Some(*secs),
                    setup: 0.0,
                })
                .collect();
            series.push_back(Entry {
                run: *runs as u64,
                outcome: outcome_code("failed").unwrap(),
                call: Some(99.0),
                setup: 0.0,
            });
            h.tests.insert(id.to_string(), series);
        }
        h
    }

    #[test]
    fn drift_needs_a_steady_rise_of_1_5x_and_0_2s() {
        let creep: Vec<f64> = (0..12).map(|i| 0.4 + 0.03 * f64::from(i)).collect();
        let (a, b, c) = drift_of(&creep).unwrap();
        assert!(a < b && b < c);
        // Under 9 runs: not judged.
        assert!(drift_of(&creep[..8]).is_none());
        // Noisy but flat: medians don't rise step by step.
        let flat = [1.0, 2.0, 1.5, 1.0, 2.0, 1.5, 1.0, 2.0, 1.5];
        assert!(drift_of(&flat).is_none());
        // Rising, but only 10%: noise, not drift.
        let small: Vec<f64> = (0..9).map(|i| 1.0 + 0.012 * f64::from(i)).collect();
        assert!(drift_of(&small).is_none());
        // 2x, but only 0.1s overall: too small to matter.
        let tiny: Vec<f64> = (0..9).map(|i| 0.1 + 0.0125 * f64::from(i)).collect();
        assert!(drift_of(&tiny).is_none());
        // A jump then flat: medians 1, 3, 3 don't rise twice.
        let jump = [1.0, 1.0, 1.0, 3.0, 3.0, 3.0, 3.0, 3.0, 3.0];
        assert!(drift_of(&jump).is_none());
    }

    #[test]
    fn drift_section_lists_creeping_tests_in_this_run() {
        let mut h = History::default();
        for seq in 0..12u64 {
            h.runs.push(RunMeta {
                seq,
                id: format!("r{seq}"),
                epoch: seq,
                commit: None,
                env: String::new(),
            });
            for (id, secs) in [
                ("t.py::creep", 0.4 + 0.05 * seq as f32),
                ("t.py::steady", 0.5),
                ("t.py::gone", 0.1 + 0.2 * seq as f32),
            ] {
                h.tests.entry(id.into()).or_default().push_back(Entry {
                    run: seq,
                    outcome: crate::reporting::history::outcome_code("passed").unwrap(),
                    call: Some(secs),
                    setup: 0.0,
                });
            }
        }
        let mut run = Run::default();
        record_test(&mut run, "t.py::creep", 0, 1.0);
        record_test(&mut run, "t.py::steady", 0, 0.5);
        let d = drift(&run, &h).unwrap();
        assert_eq!(d.drifting_tests, 1);
        assert_eq!(d.tests[0].nodeid, "t.py::creep");
        assert_eq!(d.tests[0].runs, 12);
        assert!(drift(&run, &History::default()).is_none());
    }

    #[test]
    fn timeout_for_is_3x_with_a_one_second_floor() {
        assert_eq!(timeout_for(0.1), 2); // 0.1 + 1 = 1.1 -> 2
        assert_eq!(timeout_for(0.5), 2); // 1.5 -> 2
        assert_eq!(timeout_for(5.0), 15);
        assert_eq!(timeout_for(8.4), 26); // 25.2 -> 26
        assert_eq!(timeout_for(0.0), 1);
    }

    #[test]
    fn suite_timeout_covers_95_pct_and_slow_tests_get_overrides() {
        let mut tests: Vec<(String, f32, usize)> = (0..38)
            .map(|i| (format!("t.py::fast{i}"), 0.1, 5))
            .collect();
        tests.push(("t.py::slow".into(), 5.0, 6));
        tests.push(("t.py::slowest".into(), 8.4, 12));
        tests.push(("t.py::young".into(), 30.0, 4)); // under 5 runs: not judged
        tests.push(("t.py::not_run".into(), 50.0, 9)); // not in this run
        let refs: Vec<(&str, f32, usize)> = tests
            .iter()
            .map(|(id, s, n)| (id.as_str(), *s, *n))
            .collect();
        let h = history(&refs);
        let mut run = Run::default();
        for (id, ..) in &tests[..41] {
            record_test(&mut run, id, 0, 0.1);
        }
        let t = timeout_advice(&run, &h).unwrap();
        assert_eq!(t.tests_judged, 40);
        assert_eq!(t.suite_timeout, 2);
        assert_eq!(t.override_count, 2);
        let o: Vec<(&str, u64, usize)> = t
            .overrides
            .iter()
            .map(|o| (o.nodeid.as_str(), o.timeout, o.runs))
            .collect();
        assert_eq!(o, [("t.py::slowest", 26, 12), ("t.py::slow", 15, 6)]);
        // The failed 99s run never counts as "slowest".
        assert!((t.overrides[0].slowest_seconds - 8.4).abs() < 1e-3);

        // No history: no section.
        assert!(timeout_advice(&run, &History::default()).is_none());
        // A small suite: the 95th percentile is its slowest, so no overrides.
        let h = history(&[("a", 0.1, 5), ("b", 5.0, 5)]);
        let mut run = Run::default();
        record_test(&mut run, "a", 0, 0.1);
        record_test(&mut run, "b", 0, 0.1);
        let t = timeout_advice(&run, &h).unwrap();
        assert_eq!((t.suite_timeout, t.override_count), (15, 0));
    }

    #[test]
    fn a_timeout_marker_that_covers_the_suggestion_is_not_an_override() {
        let mut tests: Vec<(String, f32, usize)> = (0..38)
            .map(|i| (format!("t.py::fast{i}"), 0.1, 5))
            .collect();
        tests.push(("t.py::marked_enough".into(), 5.0, 6)); // needs 15
        tests.push(("t.py::marked_low".into(), 8.4, 12)); // needs 26
        let refs: Vec<(&str, f32, usize)> = tests
            .iter()
            .map(|(id, s, n)| (id.as_str(), *s, *n))
            .collect();
        let h = history(&refs);
        let mut run = Run::default();
        for (id, ..) in &tests[..38] {
            record_test(&mut run, id, 0, 0.1);
        }
        record_marked(&mut run, "t.py::marked_enough", 20.0);
        record_marked(&mut run, "t.py::marked_low", 10.0);
        let t = timeout_advice(&run, &h).unwrap();
        assert_eq!(t.tests_judged, 40);
        assert_eq!(t.suite_timeout, 2);
        assert_eq!((t.override_count, t.marker_covered), (1, 1));
        let o = &t.overrides[0];
        assert_eq!(
            (o.nodeid.as_str(), o.timeout, o.current),
            ("t.py::marked_low", 26, Some(10.0))
        );
    }

    /// A passing test whose call report carries a `@pytest.mark.timeout`.
    fn record_marked(run: &mut Run, nodeid: &str, marker: f64) {
        run.record(
            Some(0),
            crate::scheduling::proto::Report {
                nodeid: nodeid.into(),
                when: "call".into(),
                outcome: "passed".into(),
                duration: 0.1,
                longrepr: None,
                wasxfail: false,
                skip_reason: None,
                cpu: None,
                thread_delta: None,
                fd_delta: None,
                sections: Vec::new(),
                lineno: None,
                subtest: false,
                crash: None,
                cause: None,
                xfail_reason: None,
                skip_location: None,
                network: Vec::new(),
                marker_timeout: Some(marker),
                no_rerun: false,
                subtest_desc: None,
                subtest_shown: false,
            },
        );
    }
}
