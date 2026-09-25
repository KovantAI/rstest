//! End-to-end tests for `rstest bisect <nodeid>`. Each builds a tiny fixture
//! suite in a temp dir and runs the real binary against it, asserting the exit
//! code, the human report, and the `--bisect-json` doc.
//!
//! These need a python with pytest. They skip cleanly otherwise. To run them:
//!
//! - CI: have `python3` on PATH with pytest importable, or
//! - local: `RSTEST_TEST_VENV=/path/to/venv cargo test -p rstest-cli --test bisect`

use std::path::{Path, PathBuf};
use std::process::Command;

/// A venv dir to expose as VIRTUAL_ENV, or None to use the ambient PATH
/// python3. Returns None to SKIP if no pytest is reachable.
fn pytest_env() -> Option<Option<PathBuf>> {
    if let Ok(venv) = std::env::var("RSTEST_TEST_VENV") {
        let py = Path::new(&venv).join("bin").join("python");
        if import_pytest(&py) {
            return Some(Some(PathBuf::from(venv)));
        }
        return None;
    }
    if import_pytest(Path::new("python3")) {
        return Some(None);
    }
    None
}

fn import_pytest(py: &Path) -> bool {
    Command::new(py)
        .args(["-c", "import pytest"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn fresh_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rstest-bisect-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run(venv: &Option<PathBuf>, dir: &Path, extra: &[&str]) -> (i32, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rstest"));
    cmd.args(extra).current_dir(dir);
    if let Some(v) = venv {
        let bin = v.join("bin");
        let path = std::env::var("PATH").unwrap_or_default();
        cmd.env("VIRTUAL_ENV", v)
            .env("PATH", format!("{}:{}", bin.display(), path));
    }
    let out = cmd.output().expect("run rstest");
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), s)
}

fn read_json(path: &Path) -> serde_json::Value {
    let txt = std::fs::read_to_string(path).unwrap_or_default();
    serde_json::from_str(&txt).unwrap_or_else(|e| panic!("bad bisect json ({e}):\n{txt}"))
}

/// One polluter among clean predecessors: `test_poison` flips a module-level
/// flag the victim asserts is untouched. Serially, only the polluter matters.
const POLLUTED: &str = "\
import os

def test_a():
    assert True

def test_b():
    assert True

def test_poison():
    os.environ['RSTEST_BISECT_POISON'] = '1'

def test_c():
    assert True

def test_d():
    assert True

def test_victim():
    assert 'RSTEST_BISECT_POISON' not in os.environ
";

#[test]
fn finds_the_single_polluter() {
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("single");
    std::fs::write(dir.join("test_pol.py"), POLLUTED).unwrap();
    let jpath = dir.join("b.json");
    let (code, out) = run(
        &venv,
        &dir,
        &[
            "bisect",
            "test_pol.py::test_victim",
            "--bisect-json",
            jpath.to_str().unwrap(),
        ],
    );
    let doc = read_json(&jpath);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 0, "a polluter should be found (exit 0)\n{out}");
    assert!(out.contains("rstest bisect"), "{out}");
    assert!(out.contains("culprit: 1 predecessor"), "{out}");
    assert!(
        out.contains(
            "rstest -n 0 -p no:randomly test_pol.py::test_poison test_pol.py::test_victim"
        ),
        "{out}"
    );
    assert_eq!(doc["order_dependent"], true);
    assert_eq!(
        doc["culprits"],
        serde_json::json!(["test_pol.py::test_poison"])
    );
    assert_eq!(
        doc["reproduce_command"],
        "rstest -n 0 -p no:randomly test_pol.py::test_poison test_pol.py::test_victim"
    );
}

#[test]
fn finds_an_interacting_pair() {
    // The victim fails only when BOTH polluters ran before it: neither alone
    // reproduces, so ddmin must keep the pair.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("pair");
    std::fs::write(
        dir.join("test_pair.py"),
        "import os\n\
         def test_x1():\n    assert True\n\n\
         def test_p():\n    os.environ['RSTEST_BISECT_P'] = '1'\n\n\
         def test_x2():\n    assert True\n\n\
         def test_q():\n    os.environ['RSTEST_BISECT_Q'] = '1'\n\n\
         def test_x3():\n    assert True\n\n\
         def test_victim():\n    assert not ('RSTEST_BISECT_P' in os.environ and 'RSTEST_BISECT_Q' in os.environ)\n",
    )
    .unwrap();
    let (code, out) = run(&venv, &dir, &["bisect", "test_pair.py::test_victim"]);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("culprits: 2 predecessor"), "{out}");
    assert!(
        out.contains(
            "rstest -n 0 -p no:randomly test_pair.py::test_p test_pair.py::test_q test_pair.py::test_victim"
        ),
        "{out}"
    );
}

#[test]
fn fails_in_isolation_is_not_order_dependent() {
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("alone");
    std::fs::write(
        dir.join("test_alone.py"),
        "def test_a():\n    assert True\n\ndef test_bad():\n    assert False\n",
    )
    .unwrap();
    let jpath = dir.join("b.json");
    let (code, out) = run(
        &venv,
        &dir,
        &[
            "bisect",
            "test_alone.py::test_bad",
            "--bisect-json",
            jpath.to_str().unwrap(),
        ],
    );
    let doc = read_json(&jpath);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("fails in isolation"), "{out}");
    assert_eq!(doc["order_dependent"], false);
    assert!(doc["reproduce_command"].is_null());
}

#[test]
fn first_in_collection_has_no_predecessors() {
    // Passes alone and is collected first: nothing precedes it to bisect.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("first");
    std::fs::write(
        dir.join("test_first.py"),
        "def test_first():\n    assert True\n\ndef test_later():\n    assert True\n",
    )
    .unwrap();
    let jpath = dir.join("b.json");
    let (code, out) = run(
        &venv,
        &dir,
        &[
            "bisect",
            "test_first.py::test_first",
            "--bisect-json",
            jpath.to_str().unwrap(),
        ],
    );
    let doc = read_json(&jpath);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("first in collection order"), "{out}");
    assert_eq!(doc["order_dependent"], false);
}

#[test]
fn passing_after_predecessors_does_not_reproduce() {
    // Passes alone AND after the whole prefix: not an ordering bug here.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("noreprod");
    std::fs::write(
        dir.join("test_ok.py"),
        "def test_a():\n    assert True\n\ndef test_b():\n    assert True\n",
    )
    .unwrap();
    let jpath = dir.join("b.json");
    let (code, out) = run(
        &venv,
        &dir,
        &[
            "bisect",
            "test_ok.py::test_b",
            "--bisect-json",
            jpath.to_str().unwrap(),
        ],
    );
    let doc = read_json(&jpath);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("does not reproduce"), "{out}");
    assert!(out.contains("migrate-check"), "{out}");
    assert_eq!(doc["order_dependent"], false);
    assert_eq!(doc["culprits"], serde_json::json!([]));
}

#[test]
fn unknown_nodeid_exits_2() {
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("unknown");
    std::fs::write(dir.join("test_u.py"), "def test_a():\n    assert True\n").unwrap();
    let jpath = dir.join("b.json");
    let (code, out) = run(
        &venv,
        &dir,
        &[
            "bisect",
            "test_u.py::test_nope",
            "--bisect-json",
            jpath.to_str().unwrap(),
        ],
    );
    let doc = read_json(&jpath);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("was not collected"), "{out}");
    // A doc is still written, recording the run as verdict-less.
    assert!(doc["error"].is_string(), "{doc}");
}

#[test]
fn works_from_a_subdirectory_with_a_cwd_relative_nodeid() {
    // Rootdir (pytest.ini) is the project root; the user runs from `tests/`
    // and types the nodeid cwd-relative. Collected ids are rootdir-relative,
    // so both the lookup and every child run must re-root them, or the victim
    // silently never runs and reads as "passes".
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("subdir");
    std::fs::write(dir.join("pytest.ini"), "[pytest]\n").unwrap();
    std::fs::create_dir_all(dir.join("tests")).unwrap();
    std::fs::write(dir.join("tests").join("test_pol.py"), POLLUTED).unwrap();
    let jpath = dir.join("b.json");
    let (code, out) = run(
        &venv,
        &dir.join("tests"),
        &[
            "bisect",
            "test_pol.py::test_victim",
            "--bisect-json",
            jpath.to_str().unwrap(),
        ],
    );
    let doc = read_json(&jpath);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        doc["culprits"],
        serde_json::json!(["tests/test_pol.py::test_poison"])
    );
    // Runnable in place: ids are re-expressed relative to the cwd, no `cd`.
    assert_eq!(
        doc["reproduce_command"],
        "rstest -n 0 -p no:randomly test_pol.py::test_poison test_pol.py::test_victim"
    );
    assert!(doc["cwd"].as_str().unwrap().ends_with("tests"), "{doc}");
}

#[test]
fn a_polluter_outside_the_invocation_subdir_is_found() {
    // pytest run from `tests/unit/` collects only that subtree; the polluter
    // lives in `tests/int/`, which a full run from the rootdir runs first.
    // bisect must collect the whole suite, as from the rootdir.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("sibling");
    std::fs::write(dir.join("pytest.ini"), "[pytest]\ntestpaths = tests\n").unwrap();
    for sub in ["int", "unit"] {
        std::fs::create_dir_all(dir.join("tests").join(sub)).unwrap();
    }
    std::fs::write(
        dir.join("tests/int/test_pollute.py"),
        "import os\ndef test_ok():\n    assert True\n\n\
         def test_poison():\n    os.environ['RSTEST_BISECT_SIBLING'] = '1'\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("tests/unit/test_victim.py"),
        "import os\ndef test_victim():\n    assert 'RSTEST_BISECT_SIBLING' not in os.environ\n",
    )
    .unwrap();
    let jpath = dir.join("b.json");
    let (code, out) = run(
        &venv,
        &dir.join("tests").join("unit"),
        &[
            "bisect",
            "test_victim.py::test_victim",
            "--bisect-json",
            jpath.to_str().unwrap(),
        ],
    );
    let doc = read_json(&jpath);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        doc["culprits"],
        serde_json::json!(["tests/int/test_pollute.py::test_poison"])
    );
    assert_eq!(
        doc["reproduce_command"],
        "rstest -n 0 -p no:randomly ../int/test_pollute.py::test_poison test_victim.py::test_victim"
    );
}

#[test]
fn child_runs_use_the_python_given_with_the_flag() {
    // Only `--python` points at the pytest venv: no VIRTUAL_ENV, venv not on
    // PATH. Children must inherit the flag, not re-resolve an interpreter.
    let Ok(venv) = std::env::var("RSTEST_TEST_VENV") else {
        return;
    };
    let py = Path::new(&venv).join("bin").join("python");
    if !import_pytest(&py) {
        return;
    }
    let dir = fresh_dir("pyflag");
    std::fs::write(dir.join("test_pol.py"), POLLUTED).unwrap();
    let jpath = dir.join("b.json");
    let out = Command::new(env!("CARGO_BIN_EXE_rstest"))
        .args([
            "bisect",
            "test_pol.py::test_victim",
            "--python",
            py.to_str().unwrap(),
            "--bisect-json",
            jpath.to_str().unwrap(),
        ])
        .current_dir(&dir)
        .env_remove("VIRTUAL_ENV")
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let doc = read_json(&jpath);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.status.code(), Some(0), "{text}");
    let cmd = doc["reproduce_command"].as_str().unwrap();
    assert!(
        cmd.starts_with(&format!(
            "rstest -n 0 --python {} -p no:randomly",
            py.display()
        )),
        "{cmd}"
    );
}

#[test]
fn parametrized_ids_are_shell_quoted_in_the_reproduce_command() {
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("param");
    std::fs::write(
        dir.join("test_param.py"),
        "import os, pytest\n\
         def test_a():\n    assert True\n\n\
         @pytest.mark.parametrize('x', ['1 x'])\n\
         def test_poison(x):\n    os.environ['RSTEST_BISECT_PARAM'] = x\n\n\
         @pytest.mark.parametrize('y', ['v'])\n\
         def test_victim(y):\n    assert 'RSTEST_BISECT_PARAM' not in os.environ\n",
    )
    .unwrap();
    let (code, out) = run(&venv, &dir, &["bisect", "test_param.py::test_victim[v]"]);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains(
            "rstest -n 0 -p no:randomly 'test_param.py::test_poison[1 x]' 'test_param.py::test_victim[v]'"
        ),
        "{out}"
    );
}

#[test]
fn pytest_args_after_double_dash_reach_collection_and_children() {
    // `-p no:rstest_bisect_gate` is harmless; `-o` sets an ini value the suite
    // reads back. The victim only fails after the polluter when that option is
    // live in the child runs, so a found culprit proves it was forwarded.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("args");
    std::fs::write(
        dir.join("conftest.py"),
        "def pytest_addoption(parser):\n    parser.addini('bisect_flag', 'x', default='off')\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("test_args.py"),
        "import os\n\
         def test_poison(pytestconfig):\n    \
             if pytestconfig.getini('bisect_flag') == 'on':\n        \
                 os.environ['RSTEST_BISECT_ARGS'] = '1'\n\n\
         def test_victim():\n    assert 'RSTEST_BISECT_ARGS' not in os.environ\n",
    )
    .unwrap();
    let jpath = dir.join("b.json");
    let (code, out) = run(
        &venv,
        &dir,
        &[
            "bisect",
            "test_args.py::test_victim",
            "--bisect-json",
            jpath.to_str().unwrap(),
            "--",
            "-o",
            "bisect_flag=on",
        ],
    );
    let doc = read_json(&jpath);
    // Without the option the victim never fails: not order-dependent.
    let (plain_code, plain_out) = run(&venv, &dir, &["bisect", "test_args.py::test_victim"]);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        doc["reproduce_command"],
        "rstest -n 0 -p no:randomly -o bisect_flag=on test_args.py::test_poison test_args.py::test_victim"
    );
    assert_eq!(plain_code, 1, "{plain_out}");
}

#[test]
fn a_test_path_after_double_dash_is_rejected() {
    // Even right after a flag that takes no value (`-v`), where a token-level
    // guess would read the path as the flag's value.
    // A path would be unioned into every child selection; refuse it up front.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("stray");
    std::fs::write(dir.join("test_s.py"), "def test_a():\n    assert True\n").unwrap();
    let (code, out) = run(
        &venv,
        &dir,
        &["bisect", "test_s.py::test_a", "--", "-v", "test_s.py"],
    );
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("include a test selection"), "{out}");
}

#[test]
fn a_prefix_past_the_argv_limit_still_bisects() {
    // ~2000 predecessors with ~600-byte ids: ~1.2MB of nodeids, over macOS's
    // 1MB ARG_MAX. The selection rides an @argsfile, so the child still starts.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("argmax");
    std::fs::write(
        dir.join("test_big.py"),
        "import os, pytest\n\
         @pytest.mark.parametrize('p', [f'{i:04d}' + 'x' * 600 for i in range(2000)])\n\
         def test_filler(p):\n    assert True\n\n\
         def test_poison():\n    os.environ['RSTEST_BISECT_BIG'] = '1'\n\n\
         def test_victim():\n    assert 'RSTEST_BISECT_BIG' not in os.environ\n",
    )
    .unwrap();
    let jpath = dir.join("b.json");
    let (code, out) = run(
        &venv,
        &dir,
        &[
            "bisect",
            "test_big.py::test_victim",
            "--bisect-json",
            jpath.to_str().unwrap(),
        ],
    );
    let doc = read_json(&jpath);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        doc["culprits"],
        serde_json::json!(["test_big.py::test_poison"])
    );
}

/// A victim/polluter pair for the layout tests below: `test_poison` leaks an
/// env var `test_victim` asserts is absent.
fn write_pair(dir: &Path, tag: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("test_pair.py"),
        format!(
            "import os\n\
             def test_poison():\n    os.environ['RSTEST_BISECT_{tag}'] = '1'\n\n\
             def test_victim():\n    assert 'RSTEST_BISECT_{tag}' not in os.environ\n"
        ),
    )
    .unwrap();
}

#[test]
fn a_nested_config_does_not_reroot_the_child_runs() {
    // The suite is rooted at the top pytest.ini, but `pkg/` has its own. A
    // child selecting only `pkg/` tests would re-root there (result keys lose
    // the `pkg/` prefix) unless pinned to the suite's rootdir and config.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("nested");
    std::fs::write(dir.join("pytest.ini"), "[pytest]\n").unwrap();
    std::fs::write(
        dir.join("test_top.py"),
        "def test_top():\n    assert True\n",
    )
    .unwrap();
    write_pair(&dir.join("pkg"), "NESTED");
    std::fs::write(dir.join("pkg").join("pytest.ini"), "[pytest]\n").unwrap();
    let jpath = dir.join("b.json");
    let (code, out) = run(
        &venv,
        &dir,
        &[
            "bisect",
            "pkg/test_pair.py::test_victim",
            "--bisect-json",
            jpath.to_str().unwrap(),
        ],
    );
    let doc = read_json(&jpath);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        doc["culprits"],
        serde_json::json!(["pkg/test_pair.py::test_poison"])
    );
    // The printed command carries the same pins, so it reproduces too.
    let cmd = doc["reproduce_command"].as_str().unwrap();
    assert!(cmd.contains("--rootdir ") && cmd.contains(" -c "), "{cmd}");
}

#[test]
fn a_configless_monorepo_root_keeps_child_runs_on_the_selection() {
    // No pytest config at the root, two configured subprojects: a plain
    // `rstest` here fans out per project. The children's @argsfile selection
    // must keep them single-project, running only what bisect asked for.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("mono");
    write_pair(&dir.join("a"), "MONO");
    std::fs::write(dir.join("a").join("pytest.ini"), "[pytest]\n").unwrap();
    // Log each victim run's cwd: monorepo mode would run it from `a/` (and
    // again from `b/`), a single-project child from the root.
    std::fs::write(
        dir.join("a").join("conftest.py"),
        "import os\n\
         def pytest_runtest_call(item):\n    \
             if item.name == 'test_victim':\n        \
                 log = os.path.join(os.path.dirname(__file__), '..', 'cwd.log')\n        \
                 open(log, 'a').write(os.getcwd() + '\\n')\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("b")).unwrap();
    std::fs::write(dir.join("b").join("pytest.ini"), "[pytest]\n").unwrap();
    std::fs::write(
        dir.join("b").join("test_b.py"),
        "def test_b():\n    assert True\n",
    )
    .unwrap();
    let jpath = dir.join("b.json");
    let (code, out) = run(
        &venv,
        &dir,
        &[
            "bisect",
            "a/test_pair.py::test_victim",
            "--bisect-json",
            jpath.to_str().unwrap(),
        ],
    );
    let doc = read_json(&jpath);
    let log = std::fs::read_to_string(dir.join("cwd.log")).unwrap_or_default();
    let root = dir.canonicalize().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        doc["culprits"],
        serde_json::json!(["a/test_pair.py::test_poison"])
    );
    assert!(!log.is_empty(), "the victim never ran");
    for cwd in log.lines() {
        // os.getcwd() is already symlink-resolved, like `root`.
        assert_eq!(Path::new(cwd), root, "{log}");
    }
}

#[test]
fn a_reordering_plugin_named_randomly_is_disabled() {
    // A stand-in for pytest-randomly, registered under its plugin name, that
    // reverses every session. Left active, the victim would run before the
    // polluter in every child and bisect would call it not order-dependent.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("randomly");
    std::fs::write(dir.join("pytest.ini"), "[pytest]\n").unwrap();
    std::fs::write(
        dir.join("randomly.py"),
        "def pytest_collection_modifyitems(items):\n    items.reverse()\n",
    )
    .unwrap();
    std::fs::write(dir.join("conftest.py"), "pytest_plugins = ['randomly']\n").unwrap();
    write_pair(&dir, "RANDOMLY");
    let (code, out) = run(&venv, &dir, &["bisect", "test_pair.py::test_victim"]);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains(
            "rstest -n 0 -p no:randomly test_pair.py::test_poison test_pair.py::test_victim"
        ),
        "{out}"
    );
}

#[test]
fn a_path_in_ini_addopts_is_named_as_the_source_and_can_be_overridden() {
    // `addopts = test_pair.py` makes pytest report positional args with
    // nothing after `--`. It would be added to every child run, so bisect
    // refuses, but must blame the config (not the user's `--` args) and let
    // `-o addopts=...` clear it.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("addopts");
    std::fs::write(dir.join("pytest.ini"), "[pytest]\naddopts = test_pair.py\n").unwrap();
    write_pair(&dir, "ADDOPTS");
    let (code, out) = run(&venv, &dir, &["bisect", "test_pair.py::test_victim"]);
    let (fixed_code, fixed_out) = run(
        &venv,
        &dir,
        &[
            "bisect",
            "test_pair.py::test_victim",
            "--",
            "-o",
            "addopts=",
        ],
    );
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("`addopts`"), "{out}");
    assert!(!out.contains("after `--`"), "{out}");
    assert_eq!(fixed_code, 0, "{fixed_out}");
}

/// Run a printed reproduce command through the shell, from `dir`.
fn run_repro(venv: &Option<PathBuf>, dir: &Path, cmd: &str) -> (i32, String) {
    let bin = Path::new(env!("CARGO_BIN_EXE_rstest")).parent().unwrap();
    let mut path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut sh = Command::new("sh");
    sh.args(["-c", cmd]).current_dir(dir);
    if let Some(v) = venv {
        path = format!("{}:{path}", v.join("bin").display());
        sh.env("VIRTUAL_ENV", v);
    }
    let out = sh.env("PATH", path).output().unwrap();
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), s)
}

/// Bisect `dir`'s `pkg/test_pair.py::test_victim` and run the printed
/// command: it must reproduce (the victim fails, exit 1) the way the verified
/// child runs did.
fn assert_repro_reproduces(tag: &str, layout: impl Fn(&Path)) {
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir(tag);
    // A root conftest fixture the tests need: a re-rooted run skips it.
    std::fs::write(
        dir.join("conftest.py"),
        "import pytest\n@pytest.fixture\ndef rootfix():\n    return 1\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("pkg")).unwrap();
    std::fs::write(
        dir.join("pkg").join("test_pair.py"),
        format!(
            "import os\n\
             def test_poison(rootfix):\n    os.environ['RSTEST_BISECT_{tag}'] = '1'\n\n\
             def test_victim(rootfix):\n    assert 'RSTEST_BISECT_{tag}' not in os.environ\n"
        ),
    )
    .unwrap();
    layout(&dir);
    let jpath = dir.join("b.json");
    let (code, out) = run(
        &venv,
        &dir,
        &[
            "bisect",
            "pkg/test_pair.py::test_victim",
            "--bisect-json",
            jpath.to_str().unwrap(),
        ],
    );
    let doc = read_json(&jpath);
    let cmd = doc["reproduce_command"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let (rc, rout) = run_repro(&venv, &dir, &cmd);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 0, "{out}");
    assert_eq!(rc, 1, "the printed command must reproduce:\n{cmd}\n{rout}");
    assert!(rout.contains("1 failed, 1 passed"), "{cmd}\n{rout}");
}

#[test]
fn repro_reproduces_under_a_nested_config_with_no_root_config() {
    assert_repro_reproduces("reproini", |dir| {
        std::fs::write(dir.join("pkg").join("pytest.ini"), "[pytest]\n").unwrap();
    });
}

#[test]
fn repro_reproduces_under_a_nearer_setup_py_with_no_config() {
    assert_repro_reproduces("reprosetup", |dir| {
        std::fs::write(dir.join("pkg").join("setup.py"), "").unwrap();
    });
}

#[test]
fn failed_first_in_addopts_neither_reorders_nor_touches_the_users_cache() {
    // `addopts = --ff` with the victim fresh in the user's lastfailed: left to
    // the user's cache, every run would move the victim first. bisect's runs
    // use a private cache, leave the user's untouched, and the printed command
    // brings its own fresh cache so it reproduces too.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("ff");
    std::fs::write(dir.join("pytest.ini"), "[pytest]\naddopts = --ff\n").unwrap();
    write_pair(&dir, "FF");
    // Make the victim the user's last failure.
    run(&venv, &dir, &["-n", "0"]);
    let lastfailed = dir.join(".pytest_cache/v/cache/lastfailed");
    let before = std::fs::read_to_string(&lastfailed).unwrap_or_default();
    let jpath = dir.join("b.json");
    let (code, out) = run(
        &venv,
        &dir,
        &[
            "bisect",
            "test_pair.py::test_victim",
            "--bisect-json",
            jpath.to_str().unwrap(),
        ],
    );
    let after = std::fs::read_to_string(&lastfailed).unwrap_or_default();
    let doc = read_json(&jpath);
    let cmd = doc["reproduce_command"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let (rc, rout) = run_repro(&venv, &dir, &cmd);
    let (rc2, _) = run_repro(&venv, &dir, &cmd);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        before.contains("test_victim"),
        "setup: victim in lastfailed\n{before}"
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        doc["culprits"],
        serde_json::json!(["test_pair.py::test_poison"])
    );
    assert_eq!(before, after, "bisect must not rewrite the user's cache");
    assert!(cmd.contains("--cache-clear"), "{cmd}");
    assert_eq!(rc, 1, "the printed command must reproduce:\n{cmd}\n{rout}");
    assert_eq!(rc2, 1, "and keep reproducing when re-run:\n{cmd}");
}

#[test]
fn new_first_in_addopts_is_refused_by_name() {
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("nf");
    std::fs::write(dir.join("pytest.ini"), "[pytest]\naddopts = --nf\n").unwrap();
    write_pair(&dir, "NF");
    let jpath = dir.join("b.json");
    let (code, out) = run(
        &venv,
        &dir,
        &[
            "bisect",
            "test_pair.py::test_victim",
            "--bisect-json",
            jpath.to_str().unwrap(),
        ],
    );
    let doc = read_json(&jpath);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("--nf"), "{out}");
    assert!(doc["error"].as_str().unwrap().contains("--nf"), "{doc}");
}

#[test]
fn exitfirst_in_addopts_does_not_stop_a_child_before_the_victim() {
    // An unrelated failure runs first. With `-x` live in the child runs the
    // session would stop there and the victim would never run.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("exitfirst");
    std::fs::write(dir.join("pytest.ini"), "[pytest]\naddopts = -x\n").unwrap();
    std::fs::write(
        dir.join("test_0_broken.py"),
        "def test_broken():\n    assert False\n",
    )
    .unwrap();
    write_pair(&dir, "EXITFIRST");
    let jpath = dir.join("b.json");
    let (code, out) = run(
        &venv,
        &dir,
        &[
            "bisect",
            "test_pair.py::test_victim",
            "--bisect-json",
            jpath.to_str().unwrap(),
        ],
    );
    let doc = read_json(&jpath);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        doc["culprits"],
        serde_json::json!(["test_pair.py::test_poison"])
    );
}

#[test]
fn a_stale_json_doc_is_replaced_on_exit_2() {
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("stale");
    write_pair(&dir, "STALE");
    let jpath = dir.join("b.json");
    std::fs::write(&jpath, r#"{"order_dependent": true, "culprits": ["old"]}"#).unwrap();
    let (code, out) = run(
        &venv,
        &dir,
        &[
            "bisect",
            "test_pair.py::test_nope",
            "--bisect-json",
            jpath.to_str().unwrap(),
        ],
    );
    let doc = read_json(&jpath);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 2, "{out}");
    assert_eq!(doc["order_dependent"], false);
    assert_eq!(doc["culprits"], serde_json::json!([]));
    assert!(
        doc["error"].as_str().unwrap().contains("was not collected"),
        "{doc}"
    );
}

#[test]
fn a_failing_culprit_under_exitfirst_still_reproduces_from_the_printed_command() {
    // The polluter leaks state AND fails its own assertion (a common shape:
    // it dies before its cleanup). With `-x` in addopts the pasted command
    // must lift it the way bisect's runs did, or it stops before the victim.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("failculprit");
    std::fs::write(dir.join("pytest.ini"), "[pytest]\naddopts = -x\n").unwrap();
    std::fs::write(
        dir.join("test_pair.py"),
        "import os\n\
         def test_poison():\n    os.environ['RSTEST_BISECT_FAILCULPRIT'] = '1'\n    assert False\n\n\
         def test_victim():\n    assert 'RSTEST_BISECT_FAILCULPRIT' not in os.environ\n",
    )
    .unwrap();
    let jpath = dir.join("b.json");
    let (code, out) = run(
        &venv,
        &dir,
        &[
            "bisect",
            "test_pair.py::test_victim",
            "--bisect-json",
            jpath.to_str().unwrap(),
        ],
    );
    let doc = read_json(&jpath);
    let cmd = doc["reproduce_command"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let (rc, rout) = run_repro(&venv, &dir, &cmd);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 0, "{out}");
    assert!(cmd.contains("--maxfail=0"), "{cmd}");
    // Both ran and failed: the culprit on its own assert, the victim polluted.
    assert_eq!(rc, 1, "{cmd}\n{rout}");
    assert!(
        rout.contains("2 failed"),
        "the victim must run too:\n{cmd}\n{rout}"
    );
}

#[test]
fn exitfirst_after_double_dash_does_not_stop_a_child_before_the_victim() {
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("userx");
    std::fs::write(
        dir.join("test_0_broken.py"),
        "def test_broken():\n    assert False\n",
    )
    .unwrap();
    write_pair(&dir, "USERX");
    let (code, out) = run(
        &venv,
        &dir,
        &["bisect", "test_pair.py::test_victim", "--", "-x"],
    );
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("culprit: 1 predecessor"), "{out}");
}

#[test]
fn a_cwd_relative_nodeid_wins_over_a_same_named_rootdir_file() {
    // From `tests/`, `test_pair.py::test_victim` is the polluted one under
    // `tests/`. The rootdir's own `test_pair.py` (always passing) must not be
    // picked instead.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("clash");
    std::fs::write(dir.join("pytest.ini"), "[pytest]\n").unwrap();
    std::fs::write(
        dir.join("test_pair.py"),
        "def test_victim():\n    assert True\n",
    )
    .unwrap();
    write_pair(&dir.join("tests"), "CLASH");
    // A package, so the two `test_pair` modules don't collide on import.
    std::fs::write(dir.join("tests").join("__init__.py"), "").unwrap();
    let jpath = dir.join("b.json");
    let (code, out) = run(
        &venv,
        &dir.join("tests"),
        &[
            "bisect",
            "test_pair.py::test_victim",
            "--bisect-json",
            jpath.to_str().unwrap(),
        ],
    );
    let doc = read_json(&jpath);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 0, "{out}");
    assert_eq!(doc["nodeid"], "tests/test_pair.py::test_victim");
    assert_eq!(
        doc["culprits"],
        serde_json::json!(["tests/test_pair.py::test_poison"])
    );
}

#[test]
fn a_disabled_cacheprovider_does_not_break_bisect() {
    // `-p no:cacheprovider`: no `--cache-clear`, no cache to pin. Plain, and
    // under `--strict-config`, where even the cache_dir override is rejected
    // and bisect must fall back to no cache pin at all.
    let Some(venv) = pytest_env() else { return };
    for (tag, ini) in [
        ("nocache", "[pytest]\naddopts = -p no:cacheprovider\n"),
        (
            "nocachestrict",
            "[pytest]\naddopts = -p no:cacheprovider --strict-config\n",
        ),
    ] {
        let dir = fresh_dir(tag);
        std::fs::write(dir.join("pytest.ini"), ini).unwrap();
        write_pair(&dir, "NOCACHE");
        let (code, out) = run(&venv, &dir, &["bisect", "test_pair.py::test_victim"]);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(code, 0, "{tag}:\n{out}");
        assert!(out.contains("culprit: 1 predecessor"), "{tag}:\n{out}");
    }
}

#[test]
fn rstest_reruns_in_config_do_not_mask_the_victims_failure() {
    // The victim fails only on its first attempt after the polluter (it
    // consumes the leaked flag). With `[tool.rstest] reruns` live in the
    // child runs, the rerun would pass and hide the order dependency.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("reruns");
    std::fs::write(
        dir.join("pyproject.toml"),
        "[tool.pytest.ini_options]\n\n[tool.rstest]\nreruns = 2\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("test_pair.py"),
        "import os\n\
         def test_poison():\n    os.environ['RSTEST_BISECT_RERUNS'] = '1'\n\n\
         def test_victim():\n    assert os.environ.pop('RSTEST_BISECT_RERUNS', None) is None\n",
    )
    .unwrap();
    let (code, out) = run(&venv, &dir, &["bisect", "test_pair.py::test_victim"]);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("culprit: 1 predecessor"), "{out}");
}

#[test]
fn a_users_confcutdir_in_addopts_is_kept_in_the_child_runs() {
    // The fixture lives in a conftest ABOVE the rootdir, reachable only via
    // `addopts = --confcutdir=..`. Pinning pytest's default cutoff instead
    // would drop it and misread the victim as failing in isolation.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("confcut");
    std::fs::write(
        dir.join("conftest.py"),
        "import pytest\n@pytest.fixture\ndef parentfix():\n    return 1\n",
    )
    .unwrap();
    let proj = dir.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(
        proj.join("pytest.ini"),
        "[pytest]\naddopts = --confcutdir=..\n",
    )
    .unwrap();
    std::fs::write(
        proj.join("test_pair.py"),
        "import os\n\
         def test_poison(parentfix):\n    os.environ['RSTEST_BISECT_CONFCUT'] = '1'\n\n\
         def test_victim(parentfix):\n    assert 'RSTEST_BISECT_CONFCUT' not in os.environ\n",
    )
    .unwrap();
    let (code, out) = run(&venv, &proj, &["bisect", "test_pair.py::test_victim"]);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("culprit: 1 predecessor"), "{out}");
}

#[test]
fn a_collection_that_fails_outright_errors_and_records_it_in_the_json() {
    // An option pytest rejects fails every collection (with or without the
    // private cache pin). bisect must stop with an error naming the failed
    // collection and still leave a verdict-less doc, not a stale or no doc.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("badopt");
    write_pair(&dir, "BADOPT");
    let jpath = dir.join("b.json");
    let (code, out) = run(
        &venv,
        &dir,
        &[
            "bisect",
            "test_pair.py::test_victim",
            "--bisect-json",
            jpath.to_str().unwrap(),
            "--",
            "--no-such-pytest-option",
        ],
    );
    let doc = read_json(&jpath);
    let _ = std::fs::remove_dir_all(&dir);
    assert_ne!(code, 0, "{out}");
    assert!(out.contains("the collection failed"), "{out}");
    assert!(
        doc["error"]
            .as_str()
            .unwrap_or_default()
            .contains("the collection failed"),
        "{doc}"
    );
    assert_eq!(doc["culprits"], serde_json::json!([]));
}

#[test]
fn an_addopts_path_is_blamed_on_the_config_even_with_user_options() {
    // User options after `--` (no path among them) plus a path in addopts:
    // the re-check without the user's args still sees the path, so the
    // config is named, not the user's `--` args.
    let Some(venv) = pytest_env() else { return };
    let dir = fresh_dir("addoptsuser");
    std::fs::write(dir.join("pytest.ini"), "[pytest]\naddopts = test_pair.py\n").unwrap();
    write_pair(&dir, "ADDOPTSUSER");
    let (code, out) = run(
        &venv,
        &dir,
        &["bisect", "test_pair.py::test_victim", "--", "-v"],
    );
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("`addopts`"), "{out}");
    assert!(!out.contains("after `--` include"), "{out}");
}
