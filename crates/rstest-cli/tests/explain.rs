//! End-to-end tests for `rstest explain <nodeid>`. Each writes hand-built cache
//! files into a temp project's `.rstest_cache/` and runs the real binary against
//! it, asserting the exit code, the human report, and the `--json` dossier.
//!
//! `explain` is interpreter-free, so these need no python: every run clears
//! `PATH`/`VIRTUAL_ENV`, which also proves the subcommand never probes one.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

const ID: &str = "tests/test_x.py::TestC::test_m[a::b]";

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn fresh_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rstest-explain-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join(".rstest_cache")).unwrap();
    d
}

fn write_cache(dir: &Path, name: &str, doc: &Value) {
    std::fs::write(
        dir.join(".rstest_cache").join(name),
        serde_json::to_vec(doc).unwrap(),
    )
    .unwrap();
}

fn write_raw(dir: &Path, name: &str, bytes: &str) {
    std::fs::write(dir.join(".rstest_cache").join(name), bytes).unwrap();
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

fn run_env(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Out {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rstest"));
    cmd.arg("explain")
        .args(args)
        .current_dir(dir)
        .env_remove("VIRTUAL_ENV")
        .env_remove("RSTEST_CACHE")
        .env_remove("RSTEST_FLAKE_RETENTION_DAYS")
        .env("PATH", "")
        .env("NO_COLOR", "1");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run rstest");
    Out {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn run(dir: &Path, args: &[&str]) -> Out {
    run_env(dir, args, &[])
}

fn run_json(dir: &Path, nodeid: &str) -> Value {
    let o = run(dir, &[nodeid, "--json"]);
    assert_eq!(o.code, 0, "stdout={} stderr={}", o.stdout, o.stderr);
    serde_json::from_str(&o.stdout)
        .unwrap_or_else(|e| panic!("bad explain json ({e}):\n{}", o.stdout))
}

/// Every cache populated for `ID`, plus neighbours that must not leak into it.
fn populate_all(dir: &Path) {
    write_cache(
        dir,
        "durations.json",
        &json!({ ID: 0.25, "tests/test_x.py::test_other": 9.0 }),
    );
    write_cache(
        dir,
        "flakes.json",
        &json!({
            ID: { "flaky": 2, "failed": 1, "last_epoch": now() - 2 * 24 * 60 * 60 },
            "tests/test_x.py::test_other": { "flaky": 7, "last_epoch": now() },
        }),
    );
    write_cache(
        dir,
        "incremental_outcomes.json",
        &json!({
            "schema": 2,
            "config_fp": "fp",
            "green": [ID, "tests/test_x.py::test_other"],
            "test_lines": { ID: 17, "tests/test_x.py::test_other": 3 },
        }),
    );
    write_cache(
        dir,
        "coverage_index.json",
        &json!({
            "schema": 2,
            "files": {
                "src/b.py": { "hash": "h", "lines": { "1": [ID], "2": [ID, "x::y"] } },
                "src/a.py": { "hash": "h", "lines": { "5": [ID] } },
                "src/c.py": { "hash": "h", "lines": { "5": ["tests/test_x.py::test_other"] } },
            },
        }),
    );
}

#[test]
fn human_report_merges_every_cache() {
    let dir = fresh_dir("human-all");
    populate_all(&dir);
    let o = run(&dir, &[ID]);
    assert_eq!(o.code, 0, "stderr={}", o.stderr);
    let expected = format!(
        "test: {ID}\n  tests/test_x.py:18\n\n  duration   0.2500s (last recorded)\n  \
         outcome    passed last incremental run\n  \
         flakes     flaked 2x, failed 1x (last 2d ago)\n  \
         coverage   covers 2 file(s), 3 line(s):\n               src/a.py\n               src/b.py\n"
    );
    assert_eq!(o.stdout, expected);
    assert!(o.stderr.is_empty(), "stderr={}", o.stderr);
}

#[test]
fn json_report_merges_every_cache() {
    let dir = fresh_dir("json-all");
    populate_all(&dir);
    let v = run_json(&dir, ID);
    assert_eq!(v["meta"]["runner"], "rstest");
    assert_eq!(v["meta"]["kind"], "explain");
    assert_eq!(v["meta"]["schema"], 1);
    assert_eq!(v["meta"]["rstest_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(v["nodeid"], ID);
    assert_eq!(v["found"], true);
    assert_eq!(v["duration_seconds"], 0.25);
    assert_eq!(v["last_outcome"], "passed");
    assert_eq!(v["source_line"], 18);
    assert_eq!(v["flakes"]["flaky"], 2);
    assert_eq!(v["flakes"]["failed"], 1);
    assert_eq!(v["coverage"]["file_count"], 2);
    assert_eq!(v["coverage"]["line_count"], 3);
    assert_eq!(v["coverage"]["files"], json!(["src/a.py", "src/b.py"]));
}

#[test]
fn json_flag_before_nodeid_is_accepted() {
    let dir = fresh_dir("json-first");
    populate_all(&dir);
    let o = run(&dir, &["--json", ID]);
    assert_eq!(o.code, 0, "stderr={}", o.stderr);
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["nodeid"], ID);
}

#[test]
fn unknown_nodeid_human_exits_1_with_suggestions() {
    let dir = fresh_dir("unknown-human");
    // Suggestions union the duration + flake keys, dedupe, sort, cap at 5.
    write_cache(
        &dir,
        "durations.json",
        &json!({
            "t/test_q.py::test_f": 1.0, "t/test_q.py::test_b": 1.0,
            "t/test_q.py::test_d": 1.0, "t/test_q.py::test_a": 1.0,
            "t/unrelated.py::test_z": 1.0,
        }),
    );
    write_cache(
        &dir,
        "flakes.json",
        &json!({
            "t/test_q.py::test_a": { "flaky": 1, "last_epoch": now() },
            "t/test_q.py::test_c": { "flaky": 1, "last_epoch": now() },
            "t/test_q.py::test_e": { "failed": 1, "last_epoch": now() },
        }),
    );
    let o = run(&dir, &["test_q.py::test_"]);
    assert_eq!(o.code, 1, "stdout={} stderr={}", o.stdout, o.stderr);
    assert!(o.stdout.is_empty(), "stdout={}", o.stdout);
    assert!(
        o.stderr
            .contains("explain: no cached data for test_q.py::test_"),
        "stderr={}",
        o.stderr
    );
    let hints: Vec<&str> = o
        .stderr
        .lines()
        .skip_while(|l| !l.contains("did you mean"))
        .skip(1)
        .map(str::trim)
        .collect();
    assert_eq!(
        hints,
        [
            "t/test_q.py::test_a",
            "t/test_q.py::test_b",
            "t/test_q.py::test_c",
            "t/test_q.py::test_d",
            "t/test_q.py::test_e",
        ]
    );
}

#[test]
fn unknown_nodeid_without_near_matches_has_no_hint_block() {
    let dir = fresh_dir("unknown-nohint");
    write_cache(&dir, "durations.json", &json!({ "t/a.py::test_a": 1.0 }));
    let o = run(&dir, &["zzz"]);
    assert_eq!(o.code, 1);
    assert!(!o.stderr.contains("did you mean"), "stderr={}", o.stderr);
}

#[test]
fn unknown_nodeid_json_exits_0_with_found_false() {
    let dir = fresh_dir("unknown-json");
    populate_all(&dir);
    let v = run_json(&dir, "tests/test_x.py::nope");
    assert_eq!(v["found"], false);
    for k in [
        "duration_seconds",
        "last_outcome",
        "source_line",
        "flakes",
        "coverage",
    ] {
        assert!(v[k].is_null(), "{k}: {v}");
    }
}

#[test]
fn no_cache_dir_at_all() {
    let dir = fresh_dir("nocache");
    std::fs::remove_dir_all(dir.join(".rstest_cache")).unwrap();
    let o = run(&dir, &[ID]);
    assert_eq!(o.code, 1, "stdout={} stderr={}", o.stdout, o.stderr);
    assert!(o.stderr.contains("no cached data"), "stderr={}", o.stderr);
    let v = run_json(&dir, ID);
    assert_eq!(v["found"], false);
}

#[test]
fn each_cache_alone_marks_the_test_found() {
    // `found` is an OR over the sources: any single one suffices.
    let cases: [(&str, Value); 4] = [
        ("durations.json", json!({ ID: 1.5 })),
        (
            "flakes.json",
            json!({ ID: { "failed": 1, "last_epoch": now() } }),
        ),
        (
            "incremental_outcomes.json",
            json!({ "schema": 2, "green": [ID] }),
        ),
        (
            "coverage_index.json",
            json!({ "schema": 2, "files": { "src/a.py": { "lines": { "1": [ID] } } } }),
        ),
    ];
    for (i, (file, doc)) in cases.iter().enumerate() {
        let dir = fresh_dir(&format!("alone-{i}"));
        write_cache(&dir, file, doc);
        let o = run(&dir, &[ID]);
        assert_eq!(o.code, 0, "{file}: stderr={}", o.stderr);
        assert!(o.stdout.starts_with(&format!("test: {ID}\n")), "{file}");
    }
}

#[test]
fn source_line_alone_marks_found_but_not_passed() {
    // A recorded def line without green membership: found, yet no pass claim.
    // The store is 0-based (pytest `location`); explain reports it 1-based.
    let dir = fresh_dir("line-only");
    write_cache(
        &dir,
        "incremental_outcomes.json",
        &json!({ "schema": 2, "green": [], "test_lines": { ID: 8 } }),
    );
    let v = run_json(&dir, ID);
    assert_eq!(v["found"], true);
    assert_eq!(v["source_line"], 9);
    assert!(v["last_outcome"].is_null());
    let o = run(&dir, &[ID]);
    assert!(o.stdout.contains("  tests/test_x.py:9\n"), "{}", o.stdout);
    assert!(o.stdout.contains("no last-green record"), "{}", o.stdout);
}

#[test]
fn outcomes_are_reported_even_when_config_fingerprint_differs() {
    // `load_raw` is deliberately ungated: a changed config disables skipping,
    // but the last-green history is still true and still reported.
    let dir = fresh_dir("fp-mismatch");
    write_cache(
        &dir,
        "incremental_outcomes.json",
        &json!({ "schema": 2, "config_fp": "stale-fingerprint", "green": [ID] }),
    );
    let v = run_json(&dir, ID);
    assert_eq!(v["last_outcome"], "passed");
}

#[test]
fn schema_mismatched_caches_are_ignored() {
    let dir = fresh_dir("schema");
    write_cache(&dir, "durations.json", &json!({ ID: 1.0 }));
    write_cache(
        &dir,
        "incremental_outcomes.json",
        &json!({ "schema": 1, "green": [ID], "test_lines": { ID: 4 } }),
    );
    write_cache(
        &dir,
        "coverage_index.json",
        &json!({ "schema": 1, "files": { "src/a.py": { "lines": { "1": [ID] } } } }),
    );
    let v = run_json(&dir, ID);
    assert_eq!(v["found"], true);
    assert!(v["last_outcome"].is_null(), "{v}");
    assert!(v["source_line"].is_null(), "{v}");
    assert!(v["coverage"].is_null(), "{v}");
}

#[test]
fn corrupt_caches_degrade_to_absent_not_error() {
    let dir = fresh_dir("corrupt");
    write_cache(&dir, "durations.json", &json!({ ID: 2.0 }));
    write_raw(&dir, "flakes.json", "{not json");
    write_raw(&dir, "incremental_outcomes.json", "[]");
    write_raw(&dir, "coverage_index.json", "\u{0}\u{1}garbage");
    let o = run(&dir, &[ID]);
    assert_eq!(o.code, 0, "stderr={}", o.stderr);
    assert!(o.stdout.contains("duration   2.0000s"), "{}", o.stdout);
    assert!(o.stdout.contains("no flake/fail history"), "{}", o.stdout);
    assert!(o.stdout.contains("no last-green record"), "{}", o.stdout);
    assert!(o.stdout.contains("coverage   cold"), "{}", o.stdout);
}

#[test]
fn all_corrupt_means_not_found() {
    let dir = fresh_dir("all-corrupt");
    for f in [
        "durations.json",
        "flakes.json",
        "incremental_outcomes.json",
        "coverage_index.json",
    ] {
        write_raw(&dir, f, "oops");
    }
    let o = run(&dir, &[ID]);
    assert_eq!(o.code, 1, "stdout={}", o.stdout);
    assert!(o.stderr.contains("no cached data"), "stderr={}", o.stderr);
}

#[test]
fn flakes_past_retention_are_aged_out() {
    let dir = fresh_dir("retention");
    let old = now() - 10 * 24 * 60 * 60;
    write_cache(
        &dir,
        "flakes.json",
        &json!({ ID: { "flaky": 3, "last_epoch": old } }),
    );
    // Default 90-day window keeps a 10-day-old event.
    let v = run_json(&dir, ID);
    assert_eq!(v["flakes"]["flaky"], 3);
    // A 5-day window drops it, and with nothing else cached the test vanishes.
    let o = run_env(&dir, &[ID], &[("RSTEST_FLAKE_RETENTION_DAYS", "5")]);
    assert_eq!(o.code, 1, "stdout={}", o.stdout);
}

#[test]
fn rstest_cache_env_redirects_the_run_scoped_caches() {
    // Durations / flakes / coverage honour RSTEST_CACHE; the incremental
    // outcomes are project-scoped (`<cwd>/.rstest_cache`), matching the writer.
    let dir = fresh_dir("envcache");
    let alt = dir.join("alt-cache");
    std::fs::create_dir_all(&alt).unwrap();
    std::fs::write(
        alt.join("durations.json"),
        serde_json::to_vec(&json!({ ID: 3.0 })).unwrap(),
    )
    .unwrap();
    // The cwd cache holds a different duration that must be ignored.
    write_cache(&dir, "durations.json", &json!({ ID: 99.0 }));
    write_cache(
        &dir,
        "incremental_outcomes.json",
        &json!({ "schema": 2, "green": [ID] }),
    );
    let o = run_env(
        &dir,
        &[ID, "--json"],
        &[("RSTEST_CACHE", alt.to_str().unwrap())],
    );
    assert_eq!(o.code, 0, "stderr={}", o.stderr);
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["duration_seconds"], 3.0);
    assert_eq!(v["last_outcome"], "passed");
}

#[test]
fn missing_nodeid_is_a_usage_error() {
    let dir = fresh_dir("usage");
    let o = run(&dir, &[]);
    assert_eq!(o.code, 2, "stdout={} stderr={}", o.stdout, o.stderr);
    assert!(o.stderr.contains("NODEID"), "stderr={}", o.stderr);
}

#[test]
fn json_output_is_a_single_pretty_document_with_trailing_newline() {
    let dir = fresh_dir("json-format");
    populate_all(&dir);
    let o = run(&dir, &[ID, "--json"]);
    assert!(o.stdout.ends_with("}\n"), "{:?}", o.stdout);
    assert!(o.stdout.starts_with("{\n  \"meta\""), "{:?}", o.stdout);
    assert!(o.stderr.is_empty(), "stderr={}", o.stderr);
}
