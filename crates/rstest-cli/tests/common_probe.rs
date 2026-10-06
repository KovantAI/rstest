//! Guards for the interpreter probe the end-to-end tests share
//! (`tests/common`), and for the CI wiring that keeps those tests running.
//! They used to skip silently whenever PYTHONPATH was unset or CI ran
//! `cargo test` before installing pytest, so a regression there would hide
//! every test behind it.

use std::path::Path;

mod common;

#[test]
fn worker_dir_is_the_repo_python_package_root() {
    assert!(
        common::worker_dir().join("rstest_worker").is_dir(),
        "{} has no rstest_worker",
        common::worker_dir().display()
    );
}

#[test]
fn probe_puts_the_worker_dir_first_and_keeps_existing_entries() {
    let worker = common::worker_dir();
    let alone: Vec<_> = std::env::split_paths(&common::pythonpath_with_worker(None)).collect();
    assert_eq!(alone, vec![worker.clone()]);

    let existing = std::env::join_paths(["/a", "/b"]).unwrap();
    let joined: Vec<_> =
        std::env::split_paths(&common::pythonpath_with_worker(Some(existing))).collect();
    assert_eq!(joined, vec![worker, "/a".into(), "/b".into()]);
}

#[test]
fn probe_imports_the_worker_without_an_ambient_pythonpath() {
    // The regression: `import rstest_worker` failed unless the caller had
    // set PYTHONPATH, so the interrupt and replay tests skipped in CI.
    let Some(py) = common::python("pytest, msgpack") else {
        return;
    };
    assert!(common::importable_with(&py, "pytest, rstest_worker", None));
}

#[test]
fn a_missing_python_skips_when_not_required() {
    assert_eq!(common::skip_or_fail::<()>(false, "no python"), None);
}

#[test]
#[should_panic(expected = "RSTEST_TEST_REQUIRE=1 but no python")]
fn a_missing_python_fails_when_required() {
    let _: Option<()> = common::skip_or_fail(true, "no python");
}

/// The text of the named step in `workflow`, from its `- name:` line to the
/// next step.
fn step<'a>(workflow: &'a str, name: &str) -> (usize, &'a str) {
    let marker = format!("- name: {name}\n");
    let start = workflow
        .find(&marker)
        .unwrap_or_else(|| panic!("ci.yml has no step `{name}`"));
    let rest = &workflow[start + marker.len()..];
    let end = rest.find("\n      - ").unwrap_or(rest.len());
    (start, &rest[..end])
}

#[test]
fn ci_installs_python_deps_before_cargo_test_and_requires_them() {
    let ci = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.github/workflows/ci.yml"),
    )
    .expect("read ci.yml");

    // Gate job: the dev group (pytest, msgpack) lands before `cargo test`,
    // which then refuses to skip on Linux and macOS.
    let (install_at, install) = step(&ci, "install python test deps");
    let (test_at, test) = step(&ci, "cargo test");
    assert!(install.contains("--group dev"), "{install}");
    assert!(
        install_at < test_at,
        "deps must be installed before cargo test"
    );
    assert!(
        test.contains("RSTEST_TEST_REQUIRE: ${{ matrix.os != 'windows-latest' && '1' || '' }}"),
        "{test}"
    );

    // Coverage job: same contract for its instrumented `cargo test`.
    let (deps_at, deps) = step(&ci, "install worker deps for live serve tests");
    let (cov_at, cov) = step(&ci, "collect rust unit coverage");
    assert!(
        deps.contains("pytest") && deps.contains("msgpack"),
        "{deps}"
    );
    assert!(deps_at < cov_at, "deps must be installed before cargo test");
    assert!(cov.contains("RSTEST_TEST_REQUIRE: \"1\""), "{cov}");
}
