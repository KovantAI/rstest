//! Absorbed waits: a test that leaves work running when it ends (a request a
//! single-threaded test server keeps serving after the client gave up, a
//! thread still holding a lock) makes whichever test runs next on its worker
//! wait it out. The run history records that wait against the innocent
//! follower. Found by pairing the history's call times with the per-worker
//! order the replay journals keep for the same runs.

use std::collections::HashMap;

use serde::Serialize;

use super::history::{is_pass, History};

/// A follower's call counts as carrying a lump when it is at least this many
/// seconds above the follower's fastest recorded passing call...
pub const LUMP_MIN_SECONDS: f64 = 1.0;
/// ...and at least this many times that fastest call.
const LUMP_FACTOR: f64 = 3.0;
/// A culprit needs a lumped follower in at least this many runs, and in at
/// least two thirds of the runs where it had a follower: one slow neighbour
/// is noise, the same one again and again is not.
const MIN_RUNS: usize = 2;
/// Followers listed per culprit.
const VICTIMS_SHOWN: usize = 3;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct AbsorbedWait {
    /// The test that leaves the wait behind.
    pub culprit: String,
    /// Median extra call time of the test that ran next on its worker, in
    /// seconds (its call time minus its fastest recorded passing call).
    pub lump_seconds: f64,
    /// Recorded runs where the next test carried the lump.
    pub runs: usize,
    /// Recorded runs where the culprit had a next test on its worker.
    pub of_runs: usize,
    /// Tests that carried it, most recent first (up to 3).
    pub absorbed_by: Vec<String>,
}

/// Culprits found across the history runs that have a replay journal.
/// `orders` maps a run uid to that run's per-worker ordered nodeids. Biggest
/// lump first.
pub fn absorbed_waits(
    history: &History,
    orders: &HashMap<String, Vec<Vec<String>>>,
) -> Vec<AbsorbedWait> {
    let fastest: HashMap<&str, f64> = history
        .tests
        .iter()
        .filter_map(|(id, entries)| {
            entries
                .iter()
                .filter(|e| is_pass(e.outcome_name()))
                .filter_map(|e| e.call.map(f64::from))
                .min_by(f64::total_cmp)
                .map(|m| (id.as_str(), m))
        })
        .collect();

    #[derive(Default)]
    struct Seen<'a> {
        lumps: Vec<f64>,
        of_runs: usize,
        victims: Vec<(u64, &'a str)>,
    }
    let mut seen: HashMap<&str, Seen> = HashMap::new();
    for run in &history.runs {
        let Some(workers) = orders.get(&run.id) else {
            continue;
        };
        for pair in workers.iter().flat_map(|w| w.windows(2)) {
            let (culprit, next) = (pair[0].as_str(), pair[1].as_str());
            let call = history
                .tests
                .get(next)
                .and_then(|entries| entries.iter().find(|e| e.run == run.seq))
                .filter(|e| is_pass(e.outcome_name()))
                .and_then(|e| e.call.map(f64::from));
            let (Some(call), Some(&fast)) = (call, fastest.get(next)) else {
                continue;
            };
            let s = seen.entry(culprit).or_default();
            s.of_runs += 1;
            if call - fast >= LUMP_MIN_SECONDS && call >= LUMP_FACTOR * fast {
                s.lumps.push(call - fast);
                s.victims.push((run.epoch, next));
            }
        }
    }

    let mut out: Vec<AbsorbedWait> = seen
        .into_iter()
        .filter(|(_, s)| s.lumps.len() >= MIN_RUNS && 3 * s.lumps.len() >= 2 * s.of_runs)
        .map(|(culprit, mut s)| {
            s.lumps.sort_by(f64::total_cmp);
            s.victims.sort_by_key(|v| std::cmp::Reverse(v.0));
            let mut absorbed_by: Vec<String> = Vec::new();
            for (_, v) in s.victims {
                if absorbed_by.len() < VICTIMS_SHOWN && !absorbed_by.iter().any(|x| x == v) {
                    absorbed_by.push(v.to_string());
                }
            }
            AbsorbedWait {
                culprit: culprit.to_string(),
                lump_seconds: (median(&s.lumps) * 1e4).round() / 1e4,
                runs: s.lumps.len(),
                of_runs: s.of_runs,
                absorbed_by,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        b.lump_seconds
            .total_cmp(&a.lump_seconds)
            .then_with(|| a.culprit.cmp(&b.culprit))
    });
    out
}

/// Median of an ascending, non-empty slice.
fn median(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reporting::history::{outcome_code, Entry, RunMeta};

    /// A history of `runs` runs (ids `r0`, `r1`, ...), with `calls[test][run]`
    /// as each test's passing call time (`None`: absent from that run).
    fn history(runs: usize, calls: &[(&str, &[Option<f64>])]) -> History {
        let mut h = History::default();
        for seq in 0..runs as u64 {
            h.runs.push(RunMeta {
                seq,
                id: format!("r{seq}"),
                epoch: seq,
                commit: None,
                env: String::new(),
            });
        }
        for (id, times) in calls {
            h.tests.insert(
                id.to_string(),
                times
                    .iter()
                    .enumerate()
                    .filter_map(|(run, t)| {
                        t.map(|t| Entry {
                            run: run as u64,
                            outcome: outcome_code("passed").unwrap(),
                            call: Some(t as f32),
                            setup: 0.0,
                        })
                    })
                    .collect(),
            );
        }
        h
    }

    fn orders(per_run: &[&[&[&str]]]) -> HashMap<String, Vec<Vec<String>>> {
        per_run
            .iter()
            .enumerate()
            .map(|(i, workers)| {
                let w = workers
                    .iter()
                    .map(|w| w.iter().map(|s| s.to_string()).collect())
                    .collect();
                (format!("r{i}"), w)
            })
            .collect()
    }

    #[test]
    fn names_the_test_whose_follower_keeps_waiting() {
        // `timeout` leaves the server busy ~10s; whichever test follows it on
        // its worker (a different one each run) waits that out.
        let h = history(
            4,
            &[
                ("timeout", &[Some(0.1), Some(0.1), Some(0.1), Some(0.1)]),
                ("a", &[Some(10.0), Some(0.02), Some(0.02), Some(0.02)]),
                ("b", &[Some(0.03), Some(9.9), Some(0.03), Some(0.03)]),
                ("c", &[Some(0.02), Some(0.02), Some(10.1), Some(0.02)]),
            ],
        );
        let o = orders(&[
            &[&["timeout", "a", "b"], &["c"]],
            &[&["c", "timeout", "b"], &["a"]],
            &[&["a", "timeout", "c"], &["b"]],
        ]);
        let found = absorbed_waits(&h, &o);
        assert_eq!(found.len(), 1, "{found:?}");
        let w = &found[0];
        assert_eq!(w.culprit, "timeout");
        assert_eq!((w.runs, w.of_runs), (3, 3));
        assert!((w.lump_seconds - 9.98).abs() < 0.01, "{w:?}");
        // Most recent first.
        assert_eq!(w.absorbed_by, ["c", "b", "a"]);
    }

    #[test]
    fn stays_quiet_on_noise() {
        let h = history(
            4,
            &[
                ("p", &[Some(0.1), Some(0.1), Some(0.1), Some(0.1)]),
                ("q", &[Some(0.1), Some(0.1), Some(0.1), Some(0.1)]),
                // Slow once, after `p`: one run is not a pattern.
                ("a", &[Some(5.0), Some(0.02), Some(0.02), Some(0.02)]),
                // Slow twice, but after different tests each time.
                ("b", &[Some(0.03), Some(6.0), Some(6.0), Some(0.03)]),
                // A test that is just slow: no lump over its own fastest run.
                ("slow", &[Some(8.0), Some(8.1), Some(8.2), Some(8.0)]),
            ],
        );
        let o = orders(&[
            &[&["p", "a"], &["q", "slow"]],
            &[&["p", "b"], &["q", "slow"]],
            &[&["q", "b"], &["p", "slow"]],
            &[&["p", "a"], &["q", "slow"]],
        ]);
        assert_eq!(absorbed_waits(&h, &o), []);
    }

    #[test]
    fn needs_two_thirds_of_the_culprits_runs() {
        // Lumps after `p` in 2 of its 4 runs: under two thirds.
        let h = history(
            4,
            &[
                ("p", &[Some(0.1), Some(0.1), Some(0.1), Some(0.1)]),
                ("a", &[Some(5.0), Some(5.0), Some(0.02), Some(0.02)]),
            ],
        );
        let o = orders(&[
            &[&["p", "a"]],
            &[&["p", "a"]],
            &[&["p", "a"]],
            &[&["p", "a"]],
        ]);
        assert_eq!(absorbed_waits(&h, &o), []);
        // A run without a journal counts for neither side.
        let o = orders(&[&[&["p", "a"]], &[&["p", "a"]], &[&["p", "a"]]]);
        let o: HashMap<_, _> = o.into_iter().filter(|(k, _)| k != "r2").collect();
        assert_eq!(absorbed_waits(&h, &o).len(), 1);
    }
}
