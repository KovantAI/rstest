use super::*;
use crate::select::CoverageFile;

/// `--cov=pkg` rooted at `scope`.
fn pkg_scope(scope: &Path) -> CovScope {
    CovScope::from_sources(scope, &["pkg"])
}

/// Line -> nodeids that covered it (test fixture shorthand).
type LineSpec<'a> = (u32, &'a [&'a str]);
/// (file path, content hash, lines) for one file in a test index.
type FileSpec<'a> = (&'a str, &'a str, &'a [LineSpec<'a>]);

fn index(files: &[FileSpec]) -> CoverageIndex {
    let mut idx = CoverageIndex {
        schema: 0,
        files: HashMap::new(),
    };
    for (path, hash, lines) in files {
        let mut lm = HashMap::new();
        for (ln, ids) in *lines {
            lm.insert(*ln, ids.iter().map(|s| s.to_string()).collect());
        }
        idx.files.insert(
            (*path).to_string(),
            CoverageFile {
                hash: (*hash).to_string(),
                lines: lm,
            },
        );
    }
    idx
}

/// Baseline with `ids` green and each of their test files hashed to `tf_hash`.
fn base(ids: &[&str], tf_hash: &str) -> Baseline {
    let green: HashSet<String> = ids.iter().map(|s| s.to_string()).collect();
    let test_file_hashes = green
        .iter()
        .map(|id| (test_file_of(id).to_string(), tf_hash.to_string()))
        .collect();
    Baseline {
        green,
        test_file_hashes,
        test_lines: HashMap::new(),
        test_named_hashes: HashMap::new(),
    }
}

/// A hash stub: test files hash to `tf`, everything else to `src`.
fn stub<'a>(src: &'a str, tf: &'a str) -> impl Fn(&str) -> Option<String> + 'a {
    move |rel: &str| {
        Some(
            if rel.ends_with(".py") && rel.starts_with('t') {
                tf
            } else {
                src
            }
            .to_string(),
        )
    }
}

#[test]
fn green_and_unchanged_is_skippable() {
    // mod.py (hash H) covered by test_a; both mod.py and the test file unchanged.
    let idx = index(&[("mod.py", "H", &[(1, &["t.py::test_a"])])]);
    let skip = skippable(&idx, &base(&["t.py::test_a"], "TF"), stub("H", "TF"));
    assert!(skip.contains("t.py::test_a"));
}

#[test]
fn changed_covered_file_is_not_skippable() {
    let idx = index(&[("mod.py", "H", &[(1, &["t.py::test_a"])])]);
    // covered mod.py hash differs from stored H.
    let skip = skippable(
        &idx,
        &base(&["t.py::test_a"], "TF"),
        stub("DIFFERENT", "TF"),
    );
    assert!(skip.is_empty());
}

#[test]
fn changed_test_file_is_not_skippable() {
    // The dependency is unchanged, but the test's OWN file was edited.
    let idx = index(&[("mod.py", "H", &[(1, &["t.py::test_a"])])]);
    let skip = skippable(&idx, &base(&["t.py::test_a"], "TF"), stub("H", "EDITED"));
    assert!(skip.is_empty(), "editing the test file must force a run");
}

#[test]
fn non_green_test_is_not_skippable() {
    let idx = index(&[("mod.py", "H", &[(1, &["t.py::test_a"])])]);
    // test_a not in the green set (failed / never recorded).
    let skip = skippable(&idx, &base(&["t.py::test_b"], "TF"), stub("H", "TF"));
    assert!(!skip.contains("t.py::test_a"));
}

#[test]
fn deleted_covered_file_is_not_skippable() {
    let idx = index(&[("mod.py", "H", &[(1, &["t.py::test_a"])])]);
    let skip = skippable(&idx, &base(&["t.py::test_a"], "TF"), |_| None);
    assert!(skip.is_empty());
}

#[test]
fn test_covering_many_files_needs_all_unchanged() {
    // test_a covers a.py (H1) and b.py (H2); a changed, b not.
    let idx = index(&[
        ("a.py", "H1", &[(1, &["t.py::test_a"])]),
        ("b.py", "H2", &[(2, &["t.py::test_a"])]),
    ]);
    let skip = skippable(&idx, &base(&["t.py::test_a"], "TF"), |rel| {
        Some(
            match rel {
                "a.py" => "CHANGED",
                "b.py" => "H2",
                _ => "TF", // the test file
            }
            .to_string(),
        )
    });
    assert!(skip.is_empty(), "one changed dependency must force a run");
}

#[test]
fn carry_forward_unrun_keeps_unrun_tests_unless_a_covered_file_changed() {
    // A subset run executed only test_b. test_a (not run) covered a.py,
    // unmeasured this run: carried. test_c (not run) covered b.py, which
    // this run measured with NEW content: its old lines are stale, so it is
    // dropped everywhere. test_b ran, so only its fresh coverage counts.
    let old = index(&[
        ("a.py", "HA", &[(1, &["t.py::test_a"])]),
        ("b.py", "HB", &[(2, &["t.py::test_b", "t.py::test_c"])]),
        ("c.py", "HC", &[(3, &["t.py::test_b", "t.py::test_c"])]),
    ]);
    let mut new = index(&[("b.py", "HB2", &[(5, &["t.py::test_b"])])]);
    let ran: HashSet<String> = ["t.py::test_b".to_string()].into_iter().collect();
    carry_forward_unrun(&old, &mut new, &ran);
    assert_eq!(new.files["a.py"].hash, "HA");
    assert_eq!(
        new.files["a.py"].lines[&1],
        vec!["t.py::test_a".to_string()]
    );
    assert_eq!(new.files["b.py"].hash, "HB2");
    assert_eq!(new.files["b.py"].lines.len(), 1);
    assert_eq!(
        new.files["b.py"].lines[&5],
        vec!["t.py::test_b".to_string()]
    );
    assert!(!new.files.contains_key("c.py"), "{:?}", new.files.keys());
}

#[test]
fn record_keeps_unrun_green_tests_of_a_subset_run() {
    let scope = std::env::temp_dir().join(format!("rstest-covskip-merge-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scope);
    std::fs::create_dir_all(&scope).unwrap();
    let file = |name: &str, body: &str| {
        let p = scope.join(name);
        std::fs::write(&p, body).unwrap();
        p.to_string_lossy().into_owned()
    };
    let a = file("t_a.py", "def test_a(): pass\n");
    let b = file("t_b.py", "def test_b(): pass\n");
    let c = file("t_c.py", "def test_c(): pass\n");
    let ids = |names: &[&str]| -> HashSet<String> { names.iter().map(|s| s.to_string()).collect() };
    let (ta, tb, tc) = (
        format!("{a}::test_a"),
        format!("{b}::test_b"),
        format!("{c}::test_c"),
    );
    let cfg = |fp: &str| ConfigState {
        fp: fp.to_string(),
        ..ConfigState::default()
    };
    record(
        &scope,
        &cfg("A"),
        ids(&[&ta, &tb, &tc]),
        HashMap::new(),
        &ids(&[&ta, &tb, &tc]),
    );
    // A subset run executes only test_b (green). test_a and test_c were not
    // run: kept. test_c's file was edited meanwhile, so it keeps the hash
    // it was recorded under (never the new content's), which busts its skip.
    let c_old = current_sha256(Path::new(&c)).unwrap();
    std::fs::write(&c, "def test_c(): assert 1\n").unwrap();
    record(&scope, &cfg("A"), ids(&[&tb]), HashMap::new(), &ids(&[&tb]));
    let b1 = load(&scope, "A");
    assert_eq!(b1.green, ids(&[&ta, &tb, &tc]));
    assert_eq!(b1.test_file_hashes.get(&c), Some(&c_old));
    // A test file this run hashed with new content drops the unrun tests
    // recorded under its old content.
    let tb2 = format!("{b}::test_b2");
    std::fs::write(&b, "def test_b(): pass\ndef test_b2(): pass\n").unwrap();
    record(
        &scope,
        &cfg("A"),
        ids(&[&tb2]),
        HashMap::new(),
        &ids(&[&tb2]),
    );
    let b2 = load(&scope, "A").green;
    assert!(b2.contains(&tb2) && !b2.contains(&tb), "{b2:?}");
    // A test this run executed and that is not green now is dropped.
    record(
        &scope,
        &cfg("A"),
        ids(&[&tb]),
        HashMap::new(),
        &ids(&[&ta, &tb]),
    );
    assert!(!load(&scope, "A").green.contains(&ta));
    // A different config fingerprint carries nothing over.
    record(
        &scope,
        &cfg("A"),
        ids(&[&ta, &tb]),
        HashMap::new(),
        &ids(&[&ta, &tb]),
    );
    record(&scope, &cfg("B"), ids(&[&tb]), HashMap::new(), &ids(&[&tb]));
    assert_eq!(load(&scope, "B").green, ids(&[&tb]));
    let _ = std::fs::remove_dir_all(&scope);
}

#[test]
fn carry_forward_restores_cached_test_coverage() {
    // Old index knew both tests; the new one (only test_b ran) dropped
    // test_a's coverage. Carrying test_a forward must restore it.
    let old = index(&[
        ("a.py", "HA", &[(1, &["t.py::test_a"])]),
        ("b.py", "HB", &[(2, &["t.py::test_b"])]),
    ]);
    let mut new = index(&[("b.py", "HB", &[(2, &["t.py::test_b"])])]);
    let cached: HashSet<String> = ["t.py::test_a".to_string()].into_iter().collect();
    carry_forward(&old, &mut new, &cached);
    assert_eq!(new.files["a.py"].hash, "HA");
    assert_eq!(
        new.files["a.py"].lines[&1],
        vec!["t.py::test_a".to_string()]
    );
    assert!(new.files.contains_key("b.py"), "ran test's coverage kept");
}

#[test]
fn conftest_change_busts_config_fingerprint() {
    // A conftest.py outside the coverage scope must still bust skipping:
    // adding, editing, and removing one must each move the fingerprint.
    let scope = std::env::temp_dir().join(format!("rstest-conftest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scope);
    std::fs::create_dir_all(scope.join("tests")).unwrap();
    let base = config_fingerprint(&scope, &CovScope::default());
    std::fs::write(scope.join("tests/conftest.py"), b"import pytest\n").unwrap();
    let added = config_fingerprint(&scope, &CovScope::default());
    assert_ne!(base, added, "adding a nested conftest must bust");
    std::fs::write(scope.join("tests/conftest.py"), b"import pytest  # edit\n").unwrap();
    let edited = config_fingerprint(&scope, &CovScope::default());
    assert_ne!(added, edited, "editing a conftest must bust");
    std::fs::remove_file(scope.join("tests/conftest.py")).unwrap();
    assert_eq!(
        base,
        config_fingerprint(&scope, &CovScope::default()),
        "removing it returns to base"
    );
    let _ = std::fs::remove_dir_all(&scope);
}

#[test]
fn coverage_requested_detects_cov_flag_only() {
    assert!(coverage_requested(&["--cov=.".to_string()]));
    assert!(coverage_requested(&["--cov".to_string()]));
    assert!(!coverage_requested(&["-n".to_string(), "2".to_string()]));
    // Coverage sub-options alone don't enable coverage collection.
    assert!(!coverage_requested(&["--cov-report=".to_string()]));
    assert!(!coverage_requested(&["--cov-context=test".to_string()]));
}

#[test]
fn config_fingerprint_folds_uncovered_first_party_source() {
    // Under --cov=pkg, editing an out-of-scope first-party module (other/mod.py)
    // must move the config fingerprint so the skip set busts; editing in-scope
    // source (covered per-file) must NOT — coverage already guards it.
    let scope = std::env::temp_dir().join(format!("rstest-uncov-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scope);
    std::fs::create_dir_all(scope.join("pkg")).unwrap();
    std::fs::create_dir_all(scope.join("other")).unwrap();
    std::fs::write(scope.join("pkg/mod.py"), b"x = 1\n").unwrap();
    std::fs::write(scope.join("other/mod.py"), b"y = 1\n").unwrap();
    let scopes = pkg_scope(&scope);
    let base = config_fingerprint(&scope, &scopes);
    // Out-of-scope edit busts.
    std::fs::write(scope.join("other/mod.py"), b"y = 2\n").unwrap();
    let after_out = config_fingerprint(&scope, &scopes);
    assert_ne!(base, after_out, "out-of-scope edit must bust");
    // In-scope edit does NOT change the fingerprint (coverage guards it).
    let before_in = config_fingerprint(&scope, &scopes);
    std::fs::write(scope.join("pkg/mod.py"), b"x = 2\n").unwrap();
    assert_eq!(
        before_in,
        config_fingerprint(&scope, &scopes),
        "in-scope source is coverage-visible, not folded here"
    );
    let _ = std::fs::remove_dir_all(&scope);
}

#[test]
fn config_fingerprint_ignores_uncovered_source_when_whole_tree() {
    // With no narrowed scope (empty cov_scopes), out-of-scope .py is NOT
    // folded — whole-tree coverage measures it per-file already.
    let scope = std::env::temp_dir().join(format!("rstest-uncov-wt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scope);
    std::fs::create_dir_all(scope.join("other")).unwrap();
    std::fs::write(scope.join("other/mod.py"), b"y = 1\n").unwrap();
    let base = config_fingerprint(&scope, &CovScope::default());
    std::fs::write(scope.join("other/mod.py"), b"y = 2\n").unwrap();
    assert_eq!(
        base,
        config_fingerprint(&scope, &CovScope::default()),
        "not folded when whole-tree"
    );
    let _ = std::fs::remove_dir_all(&scope);
}

#[test]
fn is_test_file_matches_pytest_unittest_conventions() {
    assert!(is_test_file("test_foo.py"));
    assert!(is_test_file("foo_test.py"));
    assert!(!is_test_file("mod.py"));
    assert!(!is_test_file("contest.py"));
}

#[test]
fn unmeasured_test_named_files_are_tracked_not_folded() {
    // Under --cov=pkg a test-named file outside pkg moves ConfigState's
    // test_named map, never the fingerprint (a test edit must not bust all).
    let scope = std::env::temp_dir().join(format!("rstest-tf-helper-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scope);
    std::fs::create_dir_all(scope.join("tests")).unwrap();
    std::fs::write(scope.join("tests/test_utils.py"), b"def make(): return 1\n").unwrap();
    let cov = pkg_scope(&scope);
    let before = config_state(&scope, &cov);
    assert!(before.test_named.contains_key("tests/test_utils.py"));
    std::fs::write(scope.join("tests/test_utils.py"), b"def make(): return 2\n").unwrap();
    let after = config_state(&scope, &cov);
    assert_eq!(before.fp, after.fp);
    assert_ne!(before.test_named, after.test_named);
    let _ = std::fs::remove_dir_all(&scope);
}

#[test]
fn helper_change_busts_but_test_module_change_does_not() {
    // A changed test-named file that holds no green test is a shared helper:
    // nothing is skippable. One that IS a recorded test module is left to
    // the per-test own-file guard.
    let idx = index(&[("mod.py", "H", &[(1, &["t_a.py::test_a"])])]);
    let mut b = base(&["t_a.py::test_a"], "TF");
    b.test_named_hashes = [
        ("t_a.py".to_string(), "A1".to_string()),
        ("test_utils.py".to_string(), "U1".to_string()),
    ]
    .into_iter()
    .collect();
    let now = |a: &str, u: Option<&str>| -> HashMap<String, String> {
        let mut m: HashMap<String, String> = [("t_a.py".to_string(), a.to_string())].into();
        if let Some(u) = u {
            m.insert("test_utils.py".to_string(), u.to_string());
        }
        m
    };
    assert!(!helper_changed(&b, &now("A1", Some("U1"))));
    assert!(
        !helper_changed(&b, &now("A2", Some("U1"))),
        "test module edit"
    );
    assert!(helper_changed(&b, &now("A1", Some("U2"))), "helper edit");
    assert!(helper_changed(&b, &now("A1", None)), "helper deleted");
    let state = ConfigState {
        fp: String::new(),
        test_named: now("A1", Some("U2")),
        index_cov_args: Some(Vec::new()),
        index_cov_cwd: None,
    };
    assert!(skippable_now(&idx, &b, &state).is_empty());
}

/// A fresh git repo at a temp path (hold the PATH lock: git is spawned).
fn git_repo(_held: &crate::test_env::Held, name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rstest-fp-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    for args in [
        &["init", "-q"][..],
        &["config", "user.email", "t@example.com"],
        &["config", "user.name", "t"],
        &["config", "commit.gpgsign", "false"],
    ] {
        run_git(&d, args);
    }
    d
}

fn run_git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

#[test]
fn git_mode_folds_ignored_and_honors_staging() {
    // In a git checkout the walk still picks the files; git only hashes
    // them. An ignored first-party .py (generated code a test may import)
    // folds like any other, a tracked edit busts, and staging an edit
    // (content unchanged) does not.
    let held = crate::test_env::lock();
    let scope = git_repo(&held, "git");
    std::fs::create_dir_all(scope.join("pkg")).unwrap();
    std::fs::create_dir_all(scope.join("gen")).unwrap();
    std::fs::write(scope.join(".gitignore"), b"gen/\n").unwrap();
    std::fs::write(scope.join("pkg/mod.py"), b"x = 1\n").unwrap();
    std::fs::write(scope.join("helper.py"), b"y = 1\n").unwrap();
    std::fs::write(scope.join("gen/api_pb2.py"), b"g = 1\n").unwrap();
    run_git(&scope, &["add", "."]);
    run_git(&scope, &["commit", "-q", "-m", "init"]);
    let cov = pkg_scope(&scope);
    let paths = [scope.join("helper.py"), scope.join("gen/api_pb2.py")];
    assert!(
        git_blob_ids(&scope, &paths).is_some(),
        "git mode is in effect"
    );
    let base = config_fingerprint(&scope, &cov);
    std::fs::write(scope.join("gen/api_pb2.py"), b"g = 2\n").unwrap();
    let regen = config_fingerprint(&scope, &cov);
    assert_ne!(base, regen, "ignored first-party .py must fold");
    std::fs::write(scope.join("pkg/mod.py"), b"x = 2\n").unwrap();
    assert_eq!(
        regen,
        config_fingerprint(&scope, &cov),
        "in-scope edit must not bust"
    );
    std::fs::write(scope.join("helper.py"), b"y = 2\n").unwrap();
    let edited = config_fingerprint(&scope, &cov);
    assert_ne!(regen, edited, "tracked edit must bust");
    run_git(&scope, &["add", "helper.py"]);
    assert_eq!(
        edited,
        config_fingerprint(&scope, &cov),
        "staging alone must not bust"
    );
    std::fs::write(scope.join("new_helper.py"), b"z = 1\n").unwrap();
    assert_ne!(
        edited,
        config_fingerprint(&scope, &cov),
        "untracked .py must fold"
    );
    std::fs::remove_file(scope.join("new_helper.py")).unwrap();
    std::fs::remove_file(scope.join("helper.py")).unwrap();
    assert_ne!(
        edited,
        config_fingerprint(&scope, &cov),
        "deleting a tracked .py must bust"
    );
    drop(held);
    let _ = std::fs::remove_dir_all(&scope);
}

#[test]
fn git_ids_match_walk_ids_in_a_repo_subdirectory() {
    // The project may sit in a SUBFOLDER of the repo (monorepo), or even be
    // ignored by a parent repo: hash-object resolves paths from the repo
    // root, so ids must still be right. Git ids equal `git hash-object`'s.
    let held = crate::test_env::lock();
    let repo = git_repo(&held, "subdir");
    let scope = repo.join("services/api");
    std::fs::create_dir_all(&scope).unwrap();
    std::fs::write(scope.join("helper.py"), b"y = 1\n").unwrap();
    std::fs::write(scope.join("tracked.py"), b"t = 1\n").unwrap();
    run_git(&repo, &["add", "services/api/tracked.py"]);
    let paths = [scope.join("helper.py"), scope.join("tracked.py")];
    let ids = git_blob_ids(&scope, &paths).unwrap();
    let expect = |p: &Path| {
        let out = std::process::Command::new("git")
            .args(["hash-object"])
            .arg(p)
            .current_dir(&repo)
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };
    assert_eq!(ids, vec![expect(&paths[0]), expect(&paths[1])]);
    // A parent repo that ignores the whole project still yields ids.
    std::fs::write(repo.join(".gitignore"), b"services/\n").unwrap();
    let cov = pkg_scope(&scope);
    let base = config_fingerprint(&scope, &cov);
    std::fs::write(scope.join("helper.py"), b"y = 2\n").unwrap();
    assert_ne!(
        base,
        config_fingerprint(&scope, &cov),
        "ignored project still folds"
    );
    drop(held);
    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn git_mode_prunes_virtualenv_and_folds_nested_repos() {
    // The walk prunes a venv of any name by pyvenv.cfg, and descends into a
    // nested repo and a submodule checkout (ls-files lists neither's files).
    let held = crate::test_env::lock();
    let scope = git_repo(&held, "venv-nested");
    let sp = scope.join("env/lib/python3.12/site-packages");
    std::fs::create_dir_all(&sp).unwrap();
    std::fs::write(scope.join("env/pyvenv.cfg"), b"home = /usr\n").unwrap();
    std::fs::write(sp.join("dep.py"), b"x = 1\n").unwrap();
    let shared = scope.join("libs/shared");
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::write(shared.join("util.py"), b"u = 1\n").unwrap();
    run_git(&shared, &["init", "-q"]);
    let cov = pkg_scope(&scope);
    let base = config_fingerprint(&scope, &cov);
    std::fs::write(sp.join("dep.py"), b"x = 2\n").unwrap();
    assert_eq!(
        base,
        config_fingerprint(&scope, &cov),
        "venv contents must not fold"
    );
    std::fs::write(shared.join("util.py"), b"u = 22\n").unwrap();
    assert_ne!(
        base,
        config_fingerprint(&scope, &cov),
        "nested-repo .py must fold"
    );
    drop(held);
    let _ = std::fs::remove_dir_all(&scope);
}

#[cfg(unix)]
#[test]
fn symlinked_files_and_dirs_fold_by_target_content() {
    // A tracked symlink's git id hashes the link TEXT; a symlinked package
    // dir isn't a dir to `file_type()`. Editing the target must bust both.
    let held = crate::test_env::lock();
    let repo = git_repo(&held, "symlink");
    let scope = repo.join("proj");
    let shared = repo.join("libs/shared");
    std::fs::create_dir_all(&scope).unwrap();
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::write(shared.join("util.py"), b"u = 1\n").unwrap();
    std::fs::write(repo.join("libs/helper.py"), b"h = 1\n").unwrap();
    std::os::unix::fs::symlink("../libs/shared", scope.join("shared")).unwrap();
    std::os::unix::fs::symlink("../libs/helper.py", scope.join("helper.py")).unwrap();
    // A loop back to an ancestor must not recurse forever.
    std::os::unix::fs::symlink("..", shared.join("up")).unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-q", "-m", "init"]);
    let cov = pkg_scope(&scope);
    let base = config_fingerprint(&scope, &cov);
    std::fs::write(repo.join("libs/helper.py"), b"h = 22\n").unwrap();
    let after_file = config_fingerprint(&scope, &cov);
    assert_ne!(base, after_file, "symlinked file target edit must bust");
    std::fs::write(shared.join("util.py"), b"u = 22\n").unwrap();
    assert_ne!(
        after_file,
        config_fingerprint(&scope, &cov),
        "symlinked dir edit must bust"
    );
    drop(held);
    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn git_mode_hashes_thousands_of_dirty_files_without_deadlock() {
    // Enough untracked paths that both hash-object's stdin (paths) and its
    // stdout (ids) exceed a pipe buffer: writing all input before reading
    // any output would hang here.
    let held = crate::test_env::lock();
    let scope = git_repo(&held, "many");
    let dir = scope.join("a_directory_name_long_enough_to_fill_the_pipe_quickly");
    std::fs::create_dir_all(&dir).unwrap();
    let paths: Vec<PathBuf> = (0..3000)
        .map(|i| {
            let p = dir.join(format!("module_{i:05}.py"));
            std::fs::write(&p, format!("v = {i}\n")).unwrap();
            p
        })
        .collect();
    let ids = git_blob_ids(&scope, &paths).unwrap();
    assert_eq!(ids.len(), 3000);
    assert_ne!(ids[0], ids[1]);
    drop(held);
    let _ = std::fs::remove_dir_all(&scope);
}

#[test]
fn config_fingerprint_does_not_fold_out_of_scope_test_files() {
    // Under --cov=pkg, editing an out-of-scope TEST file must NOT move the
    // config fingerprint: test-file edits are guarded per-test elsewhere, so
    // folding them here would bust the whole skip set.
    let scope = std::env::temp_dir().join(format!("rstest-tf-fold-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scope);
    std::fs::create_dir_all(scope.join("tests")).unwrap();
    std::fs::write(scope.join("tests/test_foo.py"), b"def test_x(): pass\n").unwrap();
    let scopes = pkg_scope(&scope);
    let base = config_fingerprint(&scope, &scopes);
    std::fs::write(
        scope.join("tests/test_foo.py"),
        b"def test_x(): pass  # edit\n",
    )
    .unwrap();
    assert_eq!(
        base,
        config_fingerprint(&scope, &scopes),
        "out-of-scope test-file edit must NOT bust the fingerprint"
    );
    let _ = std::fs::remove_dir_all(&scope);
}

#[test]
fn config_fingerprint_prunes_venv_by_pyvenv_cfg() {
    // A venv of a non-standard name (not in PRUNE_DIRS) is pruned by its
    // pyvenv.cfg marker: its site-packages .py must NOT fold as first-party.
    let scope = std::env::temp_dir().join(format!("rstest-venv-prune-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scope);
    let sp = scope
        .join("env")
        .join("lib")
        .join("python3.12")
        .join("site-packages");
    std::fs::create_dir_all(&sp).unwrap();
    std::fs::write(scope.join("env/pyvenv.cfg"), b"home = /usr\n").unwrap();
    std::fs::write(sp.join("dep.py"), b"x = 1\n").unwrap();
    let scopes = pkg_scope(&scope);
    let base = config_fingerprint(&scope, &scopes);
    // Editing a third-party module inside the venv must not move the fp.
    std::fs::write(sp.join("dep.py"), b"x = 2\n").unwrap();
    assert_eq!(
        base,
        config_fingerprint(&scope, &scopes),
        "venv (pyvenv.cfg) contents must not fold as first-party"
    );
    let _ = std::fs::remove_dir_all(&scope);
}

#[test]
fn config_fingerprint_folds_dot_slash_scoped_source_correctly() {
    // --cov=./pkg must classify pkg/ as in-scope (not fold the whole tree):
    // editing in-scope source stays stable, out-of-scope busts.
    let scope = std::env::temp_dir().join(format!("rstest-dotslash-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scope);
    std::fs::create_dir_all(scope.join("pkg")).unwrap();
    std::fs::create_dir_all(scope.join("other")).unwrap();
    std::fs::write(scope.join("pkg/mod.py"), b"x = 1\n").unwrap();
    std::fs::write(scope.join("other/mod.py"), b"y = 1\n").unwrap();
    let scopes = CovScope::from_sources(&scope, &["./pkg"]);
    let base = config_fingerprint(&scope, &scopes);
    // In-scope edit must NOT bust (proves ./pkg matched, not folded-everything).
    std::fs::write(scope.join("pkg/mod.py"), b"x = 2\n").unwrap();
    assert_eq!(
        base,
        config_fingerprint(&scope, &scopes),
        "in-scope stays stable"
    );
    // Out-of-scope edit still busts.
    std::fs::write(scope.join("other/mod.py"), b"y = 2\n").unwrap();
    assert_ne!(
        base,
        config_fingerprint(&scope, &scopes),
        "out-of-scope busts"
    );
    let _ = std::fs::remove_dir_all(&scope);
}

#[test]
fn config_file_change_busts_config_fingerprint() {
    // A tracked config file's presence and content must fold into the
    // fingerprint (the CONFIG_FILES loop body).
    let scope = std::env::temp_dir().join(format!("rstest-cfgfile-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scope);
    std::fs::create_dir_all(&scope).unwrap();
    let empty = config_fingerprint(&scope, &CovScope::default());
    std::fs::write(scope.join("pyproject.toml"), b"[tool.pytest]\n").unwrap();
    let added = config_fingerprint(&scope, &CovScope::default());
    assert_ne!(empty, added, "adding pyproject.toml must bust");
    std::fs::write(scope.join("pyproject.toml"), b"[tool.pytest]  # edit\n").unwrap();
    assert_ne!(
        added,
        config_fingerprint(&scope, &CovScope::default()),
        "editing it must bust"
    );
    let _ = std::fs::remove_dir_all(&scope);
}

#[test]
fn config_fingerprint_on_missing_scope_is_stable() {
    // Nonexistent scope: read_dir fails (the walk returns early) and
    // no config file hashes → the empty-hash fingerprint, computed twice equal.
    let scope = std::env::temp_dir().join(format!("rstest-nodir-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scope);
    assert!(!scope.exists());
    assert_eq!(
        config_fingerprint(&scope, &CovScope::default()),
        config_fingerprint(&scope, &CovScope::default())
    );
}

#[test]
fn test_file_hashes_hashes_existing_files_only() {
    // An existing test file is hashed once; a nonexistent one is omitted.
    let dir = std::env::temp_dir().join(format!("rstest-tfh-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let real = dir.join("t_real.py");
    std::fs::write(&real, b"def test_x(): pass\n").unwrap();
    let real_id = format!("{}::test_x", real.to_string_lossy());
    let green: HashSet<String> = [real_id.clone(), "does_not_exist.py::test_y".to_string()]
        .into_iter()
        .collect();
    let hashes = test_file_hashes(&green);
    assert!(hashes.contains_key(real.to_string_lossy().as_ref()));
    assert!(!hashes.contains_key("does_not_exist.py"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn record_and_read_round_trip_with_config_gate() {
    let scope = std::env::temp_dir().join(format!("rstest-covskip-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scope);
    std::fs::create_dir_all(&scope).unwrap();
    let green: HashSet<String> = ["t.py::test_a".to_string()].into_iter().collect();
    let lines: HashMap<String, u64> = [("t.py::test_a".to_string(), 7)].into_iter().collect();
    let cfg = |fp: &str| ConfigState {
        fp: fp.to_string(),
        test_named: HashMap::new(),
        index_cov_args: Some(vec!["--cov=pkg".to_string()]),
        index_cov_cwd: None,
    };
    record(&scope, &cfg("cfg-A"), green, lines, &HashSet::new());
    let b = load(&scope, "cfg-A");
    assert!(b.green.contains("t.py::test_a"));
    // The def line round-trips for restoring a cached entry next run.
    assert_eq!(b.test_lines.get("t.py::test_a"), Some(&7));
    // A config change (different fingerprint) disables skipping.
    assert!(load(&scope, "cfg-B").green.is_empty());
    // The index's coverage args round-trip, ungated by the fingerprint.
    assert_eq!(
        stored_index_cov_args(&scope),
        Some(vec!["--cov=pkg".to_string()])
    );
}

#[test]
fn test_file_hashes_dedups_shared_test_file() {
    // Two green nodeids in ONE test file: the file is hashed exactly once
    // (the `contains_key` short-circuit), not re-read per nodeid.
    let dir = std::env::temp_dir().join(format!("rstest-tfh-dedup-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let tf = dir.join("t_shared.py");
    std::fs::write(&tf, b"def test_x(): pass\ndef test_y(): pass\n").unwrap();
    let rel = tf.to_string_lossy().to_string();
    let green: HashSet<String> = [format!("{rel}::test_x"), format!("{rel}::test_y")]
        .into_iter()
        .collect();
    let hashes = test_file_hashes(&green);
    // One distinct file -> one entry, regardless of the two nodeids.
    assert_eq!(hashes.len(), 1);
    assert!(hashes.contains_key(rel.as_str()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn skippable_empty_green_baseline_skips_nothing() {
    // No recorded green set -> the early return: nothing is skippable even
    // when the index and hashes would otherwise match.
    let idx = index(&[("mod.py", "H", &[(1, &["t.py::test_a"])])]);
    let empty = Baseline::default();
    assert!(skippable(&idx, &empty, stub("H", "TF")).is_empty());
}

#[test]
fn skippable_now_hashes_live_files() {
    // Wire [`skippable`] to the real working tree via absolute paths (so it
    // is cwd-independent): a green test whose covered file + own file are
    // byte-identical to the index/baseline hashes is skippable.
    let dir = std::env::temp_dir().join(format!("rstest-skipnow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let modf = dir.join("mod.py");
    let testf = dir.join("t.py");
    std::fs::write(&modf, b"x = 1\n").unwrap();
    std::fs::write(&testf, b"def test_a(): pass\n").unwrap();
    let modrel = modf.to_string_lossy().to_string();
    let testrel = testf.to_string_lossy().to_string();
    let id = format!("{testrel}::test_a");
    let mod_hash = current_sha256(&modf).unwrap();
    let test_hash = current_sha256(&testf).unwrap();
    let idx = index(&[(modrel.as_str(), mod_hash.as_str(), &[(1, &[id.as_str()])])]);
    let green: HashSet<String> = [id.clone()].into_iter().collect();
    let baseline = Baseline {
        green,
        test_file_hashes: [(testrel, test_hash)].into_iter().collect(),
        test_lines: HashMap::new(),
        test_named_hashes: HashMap::new(),
    };
    let cfg = ConfigState {
        index_cov_args: Some(Vec::new()),
        ..ConfigState::default()
    };
    assert!(skippable_now(&idx, &baseline, &cfg).contains(&id));
    // An index of unknown coverage scope is never trusted.
    assert!(skippable_now(&idx, &baseline, &ConfigState::default()).is_empty());
    // Editing the covered file busts it.
    std::fs::write(&modf, b"x = 2\n").unwrap();
    assert!(skippable_now(&idx, &baseline, &cfg).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn collect_fingerprint_inputs_prunes_and_descends() {
    // Real filesystem so the walk sees genuine dir/file FileTypes: a conftest
    // at root and in a nested source dir is collected; one under a pruned dir
    // (.venv), a dot dir, and an Err file_type arm are not reached.
    let root = std::env::temp_dir().join(format!("rstest-conf-walk-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join(".venv")).unwrap();
    std::fs::create_dir_all(root.join(".hidden")).unwrap();
    std::fs::write(root.join("conftest.py"), b"").unwrap();
    std::fs::write(root.join("src/conftest.py"), b"").unwrap();
    std::fs::write(root.join("not_a_conftest.py"), b"").unwrap();
    std::fs::write(root.join(".venv/conftest.py"), b"").unwrap();
    std::fs::write(root.join(".hidden/conftest.py"), b"").unwrap();
    let mut out = FingerprintInputs::default();
    collect_fingerprint_inputs(&root, &root, None, &mut out);
    let mut names: Vec<String> = out
        .conftests
        .iter()
        .map(|p| {
            p.strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    names.sort();
    assert_eq!(names, vec!["conftest.py", "src/conftest.py"]);
    // With no cov scope, no uncovered .py is collected (not_a_conftest.py skipped).
    assert!(out.unmeasured.is_empty());
    let _ = std::fs::remove_dir_all(&root);
}
