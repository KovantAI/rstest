//! `rstest --try`: run the suite under plain pytest and under rstest (-n auto),
//! report whether outcomes are identical and the speedup. The 30-second
//! "should I switch?" proof.

use std::path::Path;

use anyhow::Result;

use super::{is_fail, Outcomes, Phase, Rec};
use crate::scheduling::worker;

/// Run a command, return (parsed outcomes from its --report-json/recorder
/// snapshot, wall seconds, exit code). `record_path` is where the run wrote its
/// JSON.
fn time_run(mut cmd: std::process::Command, record_path: &Path) -> (Option<Outcomes>, f64, i32) {
    let t0 = std::time::Instant::now();
    let code = cmd.status().ok().and_then(|s| s.code()).unwrap_or(-1);
    let wall = t0.elapsed().as_secs_f64();
    let outcomes = std::fs::read_to_string(record_path).ok().and_then(|txt| {
        let doc: serde_json::Value = serde_json::from_str(&txt).ok()?;
        let tests = doc.get("tests")?.as_object()?;
        let mut out = Outcomes::new();
        for (nodeid, e) in tests {
            out.insert(
                nodeid.clone(),
                Rec {
                    phase: if is_fail(e) { Phase::Fail } else { Phase::Pass },
                    wall: e.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0),
                    cpu: None,
                },
            );
        }
        Some(out)
    });
    (outcomes, wall, code)
}

/// Estimate CI runs/day from git history: commits in the last 30 days ÷ 30
/// (CI typically runs once per push ≈ per commit). Returns (per_day, count) or
/// None outside a git repo / with no recent history.
fn commits_per_day() -> Option<(f64, u64)> {
    let out = std::process::Command::new("git")
        .args(["rev-list", "--count", "--since=30.days.ago", "HEAD"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let n: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    (n > 0).then_some((n as f64 / 30.0, n))
}

fn fmt_secs(s: f64) -> String {
    if s >= 60.0 {
        format!("{}m{:02.0}s", (s / 60.0).floor(), s % 60.0)
    } else {
        format!("{s:.1}s")
    }
}

/// `rstest --try`: run the suite under plain pytest and under rstest (-n auto),
/// report whether outcomes are identical and the speedup. The 30-second
/// "should I switch?" proof.
pub fn run_try(python: &Path, args: &[String]) -> Result<i32> {
    let tmpdir = std::env::temp_dir();
    let pid = std::process::id();
    let py_json = tmpdir.join(format!("rstest-try-pytest-{pid}.json"));
    let rs_json = tmpdir.join(format!("rstest-try-rstest-{pid}.json"));

    eprintln!("rstest --try: running your suite under pytest…");
    let mut py = std::process::Command::new(python);
    py.args(["-m", "pytest", "-p", "rstest_worker.recorder", "-q"])
        .args(args)
        .env("RSTEST_RECORD", &py_json)
        .env("PYTHONPATH", worker::worker_pythonpath())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let (py_out, py_wall, py_code) = time_run(py, &py_json);
    let _ = std::fs::remove_file(&py_json);

    let Some(py_out) = py_out else {
        println!(
            "rstest --try: couldn't run pytest (is it installed and your suite collectable?).\n\
             Try `python -m pytest -q` yourself, then re-run `rstest --try`."
        );
        return Ok(2);
    };

    eprintln!("rstest --try: running it under rstest (-n auto)…");
    let exe = std::env::current_exe()?;
    let mut rs = std::process::Command::new(exe);
    rs.arg("-n")
        .arg("auto")
        .args(args)
        .arg("--report-json")
        .arg(&rs_json)
        .args(["-q", "--output", "dots"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let (rs_out, rs_wall, _rs_code) = time_run(rs, &rs_json);
    let _ = std::fs::remove_file(&rs_json);

    let Some(rs_out) = rs_out else {
        println!(
            "rstest --try: rstest produced no run (it may have refused to dispatch — \
             often an unstable parametrize id). Run `rstest --migrate-check` to see why."
        );
        return Ok(2);
    };

    // ---- parity ----
    let pk: std::collections::BTreeSet<&str> = py_out.keys().map(String::as_str).collect();
    let rk: std::collections::BTreeSet<&str> = rs_out.keys().map(String::as_str).collect();
    let only_py = pk.difference(&rk).count();
    let only_rs = rk.difference(&pk).count();
    let mut diffs = 0usize;
    for id in pk.intersection(&rk) {
        if py_out[*id].phase != rs_out[*id].phase {
            diffs += 1;
        }
    }
    let identical = only_py == 0 && only_rs == 0 && diffs == 0;
    let total = pk.union(&rk).count();

    println!("\n================= rstest --try =================");
    if identical {
        println!("  ✓ parity:  {total} tests — identical outcomes to pytest");
    } else {
        println!(
            "  ⚠ parity:  {} of {total} tests differ ({diffs} different outcome, \
             {only_py} only in pytest, {only_rs} only in rstest)",
            diffs + only_py + only_rs
        );
    }

    // ---- speed ----
    let speedup = if rs_wall > 0.0 {
        py_wall / rs_wall
    } else {
        0.0
    };
    println!(
        "  ⚡ speed:   pytest {}  →  rstest {}   ({speedup:.1}× at -n auto)",
        fmt_secs(py_wall),
        fmt_secs(rs_wall)
    );
    let saved = (py_wall - rs_wall).max(0.0);
    if saved >= 1.0 {
        match commits_per_day() {
            // Project over the repo's actual recent activity (commits ≈ CI
            // runs). Monthly total avoids rounding a low cadence to "0/day".
            Some((_, n)) => println!(
                "  💸 saves   {} per run — ≈ {} over your last 30 days ({n} commits ≈ CI runs)",
                fmt_secs(saved),
                fmt_secs(saved * n as f64),
            ),
            None => println!("  💸 saves   {} per run", fmt_secs(saved)),
        }
    }
    println!("================================================");

    if py_code != 0 {
        println!(
            "  note: your pytest run was already red ({} failing) — that's pre-existing, \
             not caused by rstest.",
            py_out.values().filter(|r| r.phase == Phase::Fail).count()
        );
    }
    if identical {
        println!("  → drop-in ready: `rstest` is `pytest`, in parallel. Switch with confidence.");
    } else {
        println!(
            "  → some tests differ. Could be a pytest-version difference or a real parallel-only\n\
             \x20   issue — run `rstest --migrate-check` to classify each and get the fix."
        );
    }
    Ok(if identical { 0 } else { 1 })
}
