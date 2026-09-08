//! `rstest try`: run the suite under plain pytest and under rstest (-n auto),
//! report whether outcomes are identical and the speedup. The 30-second
//! "should I switch?" proof.

use std::path::Path;

use anyhow::Result;

use super::{Outcomes, Phase};
use crate::reporting::sink::Sink;
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
        super::parse_outcomes(&doc, false)
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

/// Outcome parity between the pytest and rstest runs: how many tests each side
/// has that the other doesn't, how many shared tests ended in a different phase,
/// and whether the two sets are byte-for-byte equivalent.
struct Parity {
    total: usize,
    diffs: usize,
    only_py: usize,
    only_rs: usize,
    identical: bool,
}

/// Compare the two runs' per-test outcomes. Pure: keys present in only one side
/// count as divergence, and shared keys diverge when their phases differ.
fn compute_parity(py: &Outcomes, rs: &Outcomes) -> Parity {
    let pk: std::collections::BTreeSet<&str> = py.keys().map(String::as_str).collect();
    let rk: std::collections::BTreeSet<&str> = rs.keys().map(String::as_str).collect();
    let only_py = pk.difference(&rk).count();
    let only_rs = rk.difference(&pk).count();
    let diffs = pk
        .intersection(&rk)
        .filter(|id| py[**id].phase != rs[**id].phase)
        .count();
    Parity {
        total: pk.union(&rk).count(),
        diffs,
        only_py,
        only_rs,
        identical: only_py == 0 && only_rs == 0 && diffs == 0,
    }
}

/// `rstest try`: run the suite under plain pytest and under rstest (-n auto),
/// report whether outcomes are identical and the speedup. The 30-second
/// "should I switch?" proof.
pub fn run_try(python: &Path, args: &[String], sink: &mut Sink) -> Result<i32> {
    let tmpdir = std::env::temp_dir();
    let pid = std::process::id();
    let py_json = tmpdir.join(format!("rstest-try-pytest-{pid}.json"));
    let rs_json = tmpdir.join(format!("rstest-try-rstest-{pid}.json"));

    sink.warn("rstest try: running your suite under pytest…");
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
        sink.out_line(
            "rstest try: couldn't run pytest (is it installed and your suite collectable?).\n\
             Try `python -m pytest -q` yourself, then re-run `rstest try`.",
        );
        return Ok(2);
    };

    sink.warn("rstest try: running it under rstest (-n auto)…");
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
        sink.out_line(
            "rstest try: rstest produced no run (it may have refused to dispatch — \
             often an unstable parametrize id). Run `rstest migrate-check` to see why.",
        );
        return Ok(2);
    };

    // ---- parity ----
    let parity = compute_parity(&py_out, &rs_out);
    let Parity {
        total,
        diffs,
        only_py,
        only_rs,
        identical,
    } = parity;

    sink.out_line("\n================= rstest try =================");
    if identical {
        sink.out_line(&format!(
            "  ✓ parity:  {total} tests — identical outcomes to pytest"
        ));
    } else {
        sink.out_line(&format!(
            "  ⚠ parity:  {} of {total} tests differ ({diffs} different outcome, \
             {only_py} only in pytest, {only_rs} only in rstest)",
            diffs + only_py + only_rs
        ));
    }

    // ---- speed ----
    let speedup = if rs_wall > 0.0 {
        py_wall / rs_wall
    } else {
        0.0
    };
    sink.out_line(&format!(
        "  ⚡ speed:   pytest {}  →  rstest {}   ({speedup:.1}× at -n auto)",
        fmt_secs(py_wall),
        fmt_secs(rs_wall)
    ));
    let saved = (py_wall - rs_wall).max(0.0);
    if saved >= 1.0 {
        match commits_per_day() {
            // Project over the repo's actual recent activity (commits ≈ CI
            // runs). Monthly total avoids rounding a low cadence to "0/day".
            Some((_, n)) => sink.out_line(&format!(
                "  💸 saves   {} per run — ≈ {} over your last 30 days ({n} commits ≈ CI runs)",
                fmt_secs(saved),
                fmt_secs(saved * n as f64),
            )),
            None => sink.out_line(&format!("  💸 saves   {} per run", fmt_secs(saved))),
        }
    }
    sink.out_line("================================================");

    if py_code != 0 {
        sink.out_line(&format!(
            "  note: your pytest run was already red ({} failing) — that's pre-existing, \
             not caused by rstest.",
            py_out.values().filter(|r| r.phase == Phase::Fail).count()
        ));
    }
    if identical {
        sink.out_line(
            "  → drop-in ready: `rstest` is `pytest`, in parallel. Switch with confidence.",
        );
    } else {
        sink.out_line(
            "  → some tests differ. Could be a pytest-version difference or a real parallel-only\n\
             \x20   issue — run `rstest migrate-check` to classify each and get the fix.",
        );
    }
    Ok(if identical { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::{commits_per_day, compute_parity, fmt_secs};
    use crate::migrate::{Outcomes, Phase, Rec};

    fn outcomes(entries: &[(&str, Phase)]) -> Outcomes {
        entries
            .iter()
            .map(|(id, phase)| {
                (
                    id.to_string(),
                    Rec {
                        phase: *phase,
                        wall: 0.0,
                        cpu: None,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn compute_parity_identical_when_same_ids_and_phases() {
        let py = outcomes(&[("a", Phase::Pass), ("b", Phase::Fail)]);
        let rs = outcomes(&[("a", Phase::Pass), ("b", Phase::Fail)]);
        let p = compute_parity(&py, &rs);
        assert!(p.identical);
        assert_eq!(p.total, 2);
        assert_eq!(p.diffs, 0);
        assert_eq!(p.only_py, 0);
        assert_eq!(p.only_rs, 0);
    }

    #[test]
    fn compute_parity_flags_phase_divergence() {
        // Same id, opposite phase -> one differing outcome, not identical.
        let py = outcomes(&[("a", Phase::Pass)]);
        let rs = outcomes(&[("a", Phase::Fail)]);
        let p = compute_parity(&py, &rs);
        assert!(!p.identical);
        assert_eq!(p.diffs, 1);
        assert_eq!(p.total, 1);
    }

    #[test]
    fn compute_parity_counts_ids_unique_to_each_side() {
        // 'a' shared+agreeing, 'b' only pytest, 'c' only rstest.
        let py = outcomes(&[("a", Phase::Pass), ("b", Phase::Pass)]);
        let rs = outcomes(&[("a", Phase::Pass), ("c", Phase::Pass)]);
        let p = compute_parity(&py, &rs);
        assert!(!p.identical);
        assert_eq!(p.diffs, 0, "the shared id agrees");
        assert_eq!(p.only_py, 1);
        assert_eq!(p.only_rs, 1);
        assert_eq!(p.total, 3, "union of a, b, c");
    }

    #[test]
    fn commits_per_day_is_positive_or_none_and_never_panics() {
        // Runs `git rev-list` over the ambient repo. The count isn't pinned, but
        // the contract holds: Some((per_day, n)) with both > 0, or None. It must
        // parse cleanly and never panic.
        if let Some((per_day, n)) = commits_per_day() {
            assert!(n > 0, "Some is only returned when there are commits");
            assert!(per_day > 0.0, "per-day rate derives from n > 0");
            assert!((per_day - n as f64 / 30.0).abs() < 1e-9);
        }
    }

    #[test]
    fn fmt_secs_sub_minute_is_one_decimal_seconds() {
        assert_eq!(fmt_secs(0.0), "0.0s");
        assert_eq!(fmt_secs(5.2), "5.2s");
        assert_eq!(fmt_secs(59.9), "59.9s");
    }

    #[test]
    fn fmt_secs_minute_and_over_is_zero_padded_minutes_seconds() {
        // Exactly a minute -> the seconds field is zero-padded to two digits.
        assert_eq!(fmt_secs(60.0), "1m00s");
        assert_eq!(fmt_secs(90.0), "1m30s");
        assert_eq!(fmt_secs(125.0), "2m05s");
    }
}
