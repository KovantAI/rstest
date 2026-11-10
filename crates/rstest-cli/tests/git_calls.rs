//! End-to-end guard: a run's git calls (HEAD, the `-dirty` probe) finish
//! before the first test starts. They run on a background thread while setup
//! continues; joined before dispatch, none can still be in flight while a test
//! runs (where it would race the test's own writes to the tree).
//!
//! A `git` shim on PATH logs each call's start and end into a shared log, and
//! sleeps so a call that escapes the join is still running when the test body
//! logs its own start/end lines there.
//!
//! Needs a python with pytest and the rstest worker; skips cleanly otherwise.
//! Local: `RSTEST_TEST_VENV=/path/to/venv cargo test -p rstest-cli --test git_calls`
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;

mod common;

fn real_git() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|d| d.join("git"))
        .find(|p| p.is_file())
        .expect("no git on PATH")
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new(real_git())
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

#[test]
fn no_git_call_in_flight_while_a_test_runs() {
    let Some(venv) = common::pytest_env("pytest, rstest_worker") else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("rstest-git-calls-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("calls.log");
    std::fs::write(
        dir.join("test_a.py"),
        "import os, time\n\ndef test_a():\n    \
         log = open(os.environ['CALLS_LOG'], 'a')\n    \
         log.write('test-start\\n'); log.flush()\n    \
         time.sleep(1)\n    \
         log.write('test-end\\n'); log.close()\n",
    )
    .unwrap();
    git(&dir, &["init", "-q"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "init"]);

    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    // Written and chmod'ed from a child shell, as `test_env::write_executable`
    // does: a write fd held open here while a sibling forks makes exec ETXTBSY.
    let script = format!(
        "#!/bin/sh\necho git-start >> \"$CALLS_LOG\"\nsleep 0.5\n\"{}\" \"$@\"\nrc=$?\n\
         echo git-end >> \"$CALLS_LOG\"\nexit $rc\n",
        real_git().display()
    );
    let ok = Command::new("/bin/sh")
        .args([
            "-c",
            r#"printf '%s' "$1" > "$2" && /bin/chmod 755 "$2""#,
            "sh",
        ])
        .arg(script)
        .arg(bin.join("git"))
        .status()
        .unwrap()
        .success();
    assert!(ok, "write git shim");

    let mut path = vec![bin];
    if let Some(v) = &venv {
        path.push(v.join("bin"));
    }
    path.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rstest"));
    cmd.args(["-n", "2", "-q"])
        .current_dir(&dir)
        .env("PATH", std::env::join_paths(path).unwrap())
        .env("CALLS_LOG", &log)
        .env("PYTHONPATH", common::pythonpath_with_worker(None));
    if let Some(v) = &venv {
        cmd.env("VIRTUAL_ENV", v);
    }
    let out = cmd.output().expect("spawn rstest");
    assert!(
        out.status.success(),
        "rstest failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let lines: Vec<String> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    let at = |m: &str| {
        lines
            .iter()
            .position(|l| l == m)
            .unwrap_or_else(|| panic!("no {m}: {lines:?}"))
    };
    let (start, end) = (at("test-start"), at("test-end"));
    let before = &lines[..start];
    let count = |m: &str| before.iter().filter(|l| *l == m).count();
    assert!(
        count("git-start") > 0,
        "git never called before the test: {lines:?}"
    );
    assert_eq!(
        count("git-start"),
        count("git-end"),
        "git still running when the test started: {lines:?}"
    );
    assert!(
        lines[start..end].iter().all(|l| !l.starts_with("git")),
        "git called while the test ran: {lines:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}
