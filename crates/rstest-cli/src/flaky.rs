//! `rstest flaky`: the suite-wide flake view, read from the per-test run
//! history (`.rstest_cache/history.msgpack`) without running anything. Ranks
//! tests by flips (a `flaky` pass, or a failure right after a pass), shows
//! each one's recent outcome strip and whether it is getting worse, and marks
//! the ones `--auto-quarantine` would catch on the next run.

use anyhow::Result;
use serde::Serialize;

use crate::reporting::history::{flip_positions, History};
use crate::reporting::sink::Sink;

const SCHEMA: u32 = 1;
/// Outcomes shown in the human strip (the JSON carries the whole series).
const STRIP: usize = 20;

/// Envelope metadata for the flaky document.
#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
struct Meta {
    /// Constant producer tag: always `"rstest"`.
    runner: &'static str,
    /// Constant discriminator: always `"flaky"`.
    kind: &'static str,
    /// Document schema version.
    schema: u32,
    /// The rstest version that wrote the document.
    rstest_version: &'static str,
}

/// One test that flipped at least once in its recorded history.
#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
struct FlakyTest {
    /// The test's nodeid.
    nodeid: String,
    /// Flips in the recorded history: a `flaky` pass, or a failure right
    /// after a pass.
    flips: usize,
    /// `failing` when the last 3 pass/fail results all failed; otherwise
    /// `worse` / `better` when the newer half of the history has at least 2
    /// more / fewer flips than the older half, else `steady`.
    trend: &'static str,
    /// Runs that passed only on rerun.
    flaky: usize,
    /// Runs that failed (`failed`, `errors`, `crashed` or `quarantined`).
    failed: usize,
    /// Recorded runs of this test.
    runs: usize,
    /// Outcome per run, oldest first (same values as `explain`'s history).
    outcomes: Vec<String>,
    /// Epoch seconds of the newest flip.
    last_flip_epoch: u64,
    /// Whether `--auto-quarantine` quarantines it at the current threshold.
    auto_quarantined: bool,
}

/// The ranked flake view. Tests are sorted by flips (most first), then by
/// newest flip.
#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub(crate) struct FlakyReport {
    /// Envelope metadata.
    meta: Meta,
    /// Flips at which `--auto-quarantine` catches a test (`0` = off), from
    /// `[tool.rstest] auto-quarantine` or the default.
    auto_quarantine: u32,
    /// Tests with any recorded run.
    tests_in_history: usize,
    /// Tests with at least one flip, before `--top` cut the list.
    flaky_tests: usize,
    /// The ranked tests, at most `--top` of them.
    tests: Vec<FlakyTest>,
}

fn is_fail(outcome: &str) -> bool {
    matches!(outcome, "failed" | "errors" | "crashed" | "quarantined")
}

/// `failing` when the last 3 pass/fail results all failed (it broke, it
/// isn't flapping now). Otherwise compares flips in the newer and older half
/// of the series; a difference under 2 is noise (a flip can't land on the
/// first run, so a steady flapper's older half always trails by one).
fn trend(outcomes: &[String], flips: &[usize]) -> &'static str {
    let tail: Vec<&String> = outcomes
        .iter()
        .rev()
        .filter(|o| is_fail(o) || matches!(o.as_str(), "passed" | "flaky"))
        .take(3)
        .collect();
    if tail.len() == 3 && tail.iter().all(|o| is_fail(o)) {
        return "failing";
    }
    let half = outcomes.len() / 2;
    let older = flips.iter().filter(|&&i| i < half).count();
    let newer = flips.len() - older;
    if newer >= older + 2 {
        "worse"
    } else if older >= newer + 2 {
        "better"
    } else {
        "steady"
    }
}

fn gather(h: &History, auto: u32, top: usize) -> FlakyReport {
    let mut tests: Vec<FlakyTest> = h
        .tests
        .keys()
        .filter_map(|id| {
            let series = h.series(id);
            let pos = flip_positions(series.iter().map(|(r, e)| (r.commit.as_deref(), *e)));
            let last = *pos.last()?;
            let outcomes: Vec<String> = series
                .iter()
                .map(|(_, e)| e.outcome_name().to_string())
                .collect();
            Some(FlakyTest {
                nodeid: id.clone(),
                flips: pos.len(),
                trend: trend(&outcomes, &pos),
                flaky: outcomes.iter().filter(|o| *o == "flaky").count(),
                failed: outcomes.iter().filter(|o| is_fail(o)).count(),
                runs: series.len(),
                last_flip_epoch: series[last].0.epoch,
                auto_quarantined: auto > 0 && pos.len() >= auto as usize,
                outcomes,
            })
        })
        .collect();
    tests.sort_by(|a, b| {
        b.flips
            .cmp(&a.flips)
            .then(b.last_flip_epoch.cmp(&a.last_flip_epoch))
            .then(a.nodeid.cmp(&b.nodeid))
    });
    let flaky_tests = tests.len();
    if top > 0 {
        tests.truncate(top);
    }
    FlakyReport {
        meta: Meta {
            runner: "rstest",
            kind: "flaky",
            schema: SCHEMA,
            rstest_version: env!("CARGO_PKG_VERSION"),
        },
        auto_quarantine: auto,
        tests_in_history: h.tests.len(),
        flaky_tests,
        tests,
    }
}

pub fn run_flaky(sink: &mut Sink, top: usize, json: bool) -> Result<i32> {
    let cwd = std::env::current_dir()?;
    let auto = crate::config::rstest_settings(&cwd, sink.err())
        .auto_quarantine
        .unwrap_or(crate::cli::AUTO_QUARANTINE);
    let report = gather(&crate::reporting::history::load(), auto, top);
    if json {
        let mut bytes = serde_json::to_vec_pretty(&report)?;
        bytes.push(b'\n');
        sink.out().write_all(&bytes)?;
    } else {
        render(sink, &report, crate::time::now_epoch_secs());
    }
    Ok(0)
}

fn render(sink: &mut Sink, r: &FlakyReport, now: u64) {
    let pal = sink.palette();
    if r.tests_in_history == 0 {
        sink.out_line(
            &pal.dim("no run history yet: .rstest_cache/history.msgpack is written by every run"),
        );
        return;
    }
    if r.flaky_tests == 0 {
        sink.out_line(&format!(
            "no flaky tests: none of the {} tests in the history flipped between pass and fail",
            r.tests_in_history
        ));
        return;
    }
    let caught = r.tests.iter().filter(|t| t.auto_quarantined).count();
    let policy = if r.auto_quarantine == 0 {
        "auto-quarantine off".to_string()
    } else {
        format!("auto-quarantine at {}+ flips", r.auto_quarantine)
    };
    sink.out_line(&format!(
        "{} flaky test(s) of {} in the history ({policy})",
        r.flaky_tests, r.tests_in_history
    ));
    sink.out_line("");
    sink.out_line(&format!(
        "  {:>5}  {:<7}  {:<STRIP$}  {:>5}  {:>6}  {:<9}  test",
        "flips", "trend", "recent runs", "flaky", "failed", "last flip"
    ));
    for t in &r.tests {
        let strip: String = t
            .outcomes
            .iter()
            .skip(t.outcomes.len().saturating_sub(STRIP))
            .map(|o| crate::explain::glyph(o))
            .collect();
        let trend = match t.trend {
            "worse" => pal.red("worse  "),
            "failing" => pal.red("failing"),
            "better" => pal.green("better "),
            s => format!("{s:<7}"),
        };
        let mark = if t.auto_quarantined {
            format!("  {}", pal.yellow("[auto-quarantined]"))
        } else {
            String::new()
        };
        sink.out_line(&format!(
            "  {:>5}  {trend}  {strip:<STRIP$}  {:>5}  {:>6}  {:<9}  {}{mark}",
            t.flips,
            t.flaky,
            t.failed,
            crate::explain::ago(t.last_flip_epoch, now),
            t.nodeid,
        ));
    }
    let hidden = r.flaky_tests - r.tests.len();
    if hidden > 0 {
        sink.out_line(&pal.dim(&format!("  ... {hidden} more (--top 0 shows all)")));
    }
    if caught > 0 {
        sink.out_line("");
        sink.out_line(&format!(
            "{caught} shown test(s) will be auto-quarantined on the next run; \
             `rstest explain <nodeid>` shows one test's history"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reporting::history::{outcome_code, Entry, RunMeta};

    /// History with one run per strip character (`p` pass, `f` fail, `~`
    /// flaky, `s` skipped); run `i` has epoch `i`, all on one clean commit.
    fn history(tests: &[(&str, &str)]) -> History {
        let mut h = History::default();
        let runs = tests.iter().map(|(_, s)| s.len()).max().unwrap_or(0);
        for seq in 0..runs as u64 {
            h.runs.push(RunMeta {
                seq,
                epoch: seq,
                commit: Some("abc".into()),
            });
        }
        for (id, strip) in tests {
            let entries = strip
                .chars()
                .enumerate()
                .map(|(i, c)| Entry {
                    run: i as u64,
                    outcome: outcome_code(match c {
                        'p' => "passed",
                        'f' => "failed",
                        '~' => "flaky",
                        _ => "skipped",
                    })
                    .unwrap(),
                    call: None,
                    setup: 0.0,
                })
                .collect();
            h.tests.insert(id.to_string(), entries);
        }
        h
    }

    #[test]
    fn ranks_by_flips_then_recency_and_marks_auto_quarantine() {
        let h = history(&[
            ("old_flapper", "pfpfpfpppppp"), // 3 flips, all in the older half
            ("new_flapper", "pppppppfpfpf"), // 3 flips, newest last
            ("rescued", "p~pp"),             // 1 flip
            ("broke", "ppppffff"),           // 1 flip
            ("green", "pppp"),               // 0: not listed
            ("born_red", "ffff"),            // 0: not listed
        ]);
        let r = gather(&h, 3, 0);
        assert_eq!(r.tests_in_history, 6);
        assert_eq!(r.flaky_tests, 4);
        let ids: Vec<&str> = r.tests.iter().map(|t| t.nodeid.as_str()).collect();
        assert_eq!(ids, ["new_flapper", "old_flapper", "broke", "rescued"]);
        let new = &r.tests[0];
        assert_eq!((new.flips, new.trend, new.failed), (3, "worse", 3));
        assert_eq!(new.last_flip_epoch, 11);
        assert_eq!(r.tests[2].trend, "failing");
        assert!(new.auto_quarantined);
        assert_eq!(r.tests[1].trend, "better");
        assert_eq!(r.tests[3].flaky, 1);
        assert!(!r.tests[3].auto_quarantined);

        // --top cuts the list but keeps the total; threshold 0 marks nothing.
        let r = gather(&h, 0, 1);
        assert_eq!((r.tests.len(), r.flaky_tests), (1, 4));
        assert!(!r.tests[0].auto_quarantined);
    }

    #[test]
    fn trend_labels() {
        let t = |strip: &str| {
            let h = history(&[("x", strip)]);
            gather(&h, 3, 0).tests[0].trend
        };
        assert_eq!(t("pfpfpfpfpf"), "steady"); // steady flapper: 2 vs 3 flips
        assert_eq!(t("pppppppfpfpf"), "worse");
        assert_eq!(t("pfpfpfpppppp"), "better");
        assert_eq!(t("ppppffff"), "failing"); // broke, not flapping
        assert_eq!(t("pfpfpfsfsff"), "failing"); // skips stepped over
        assert_eq!(t("pfpfpff"), "steady"); // only 2 trailing fails
    }

    #[test]
    fn render_table_and_empty_states() {
        let h = history(&[("t.py::a", "pfpfpf"), ("t.py::b", "pp")]);
        let (mut sink, cap) = Sink::captured();
        render(&mut sink, &gather(&h, 3, 0), 5 + 7200);
        let out = cap.out();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[0],
            "1 flaky test(s) of 2 in the history (auto-quarantine at 3+ flips)"
        );
        assert!(lines[2].contains("flips  trend") && lines[2].ends_with("test"));
        assert!(lines[3].contains("✓✗✓✗✓✗"), "{}", lines[3]);
        assert!(lines[3].contains("2h ago"), "{}", lines[3]);
        assert!(
            lines[3].ends_with("t.py::a  [auto-quarantined]"),
            "{}",
            lines[3]
        );
        assert!(out.contains("1 shown test(s) will be auto-quarantined"));

        let (mut sink, cap) = Sink::captured();
        render(&mut sink, &gather(&h, 3, 0).with_top_hidden(), 0);
        assert!(cap.out().contains("... 1 more (--top 0 shows all)"));

        let (mut sink, cap) = Sink::captured();
        render(&mut sink, &gather(&history(&[("x", "pp")]), 3, 0), 0);
        assert!(cap.out().starts_with("no flaky tests: none of the 1 tests"));

        let (mut sink, cap) = Sink::captured();
        render(&mut sink, &gather(&History::default(), 3, 0), 0);
        assert!(cap.out().contains("no run history yet"));
    }

    impl FlakyReport {
        /// Pretend `--top` hid one row.
        fn with_top_hidden(mut self) -> Self {
            self.flaky_tests += 1;
            self
        }
    }

    #[test]
    fn json_is_schema_stamped() {
        let h = history(&[("t.py::a", "p~pf")]);
        let v = serde_json::to_value(gather(&h, 3, 0)).unwrap();
        assert_eq!(v["meta"]["kind"], "flaky");
        assert_eq!(v["meta"]["schema"], SCHEMA);
        assert_eq!(v["auto_quarantine"], 3);
        assert_eq!(v["tests"][0]["flips"], 2);
        assert_eq!(v["tests"][0]["outcomes"][1], "flaky");
        assert_eq!(v["tests"][0]["auto_quarantined"], false);
    }
}
