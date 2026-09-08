//! Splitting the worker budget across concurrently-running projects.

use std::path::Path;

/// Per-project worker shares honoring fixed pins: a project whose own
/// `[tool.rstest] numprocesses` is a number keeps it (clamped to budget;
/// 0/1 = single-worker exact mode). The rest split the remainder by weight.
pub fn plan_shares_with_fixed(
    costs: &[Option<f64>],
    fixed: &[Option<usize>],
    budget: usize,
) -> Vec<usize> {
    let n = costs.len();
    let fixed_spend: usize = fixed
        .iter()
        .flatten()
        // -n 0 still occupies one worker process
        .map(|&f| f.clamp(1, budget))
        .sum();
    let free_idx: Vec<usize> = (0..n).filter(|&i| fixed[i].is_none()).collect();
    let free_costs: Vec<Option<f64>> = free_idx.iter().map(|&i| costs[i]).collect();
    let free_budget = budget.saturating_sub(fixed_spend).max(free_idx.len());
    let free_shares = plan_shares(&free_costs, free_budget);
    let mut shares = vec![0usize; n];
    for (slot, &i) in free_idx.iter().enumerate() {
        shares[i] = free_shares[slot];
    }
    for i in 0..n {
        if let Some(f) = fixed[i] {
            shares[i] = f.min(budget);
        }
    }
    shares
}

/// A project's own `[tool.rstest] numprocesses`, when it is a NUMBER
/// ("auto" or absent leaves the planner in charge).
pub fn project_fixed_n(project: &Path) -> Option<usize> {
    let settings = crate::config::rstest_settings(project, &mut std::io::stderr());
    settings.numprocesses.as_deref()?.parse().ok()
}

/// Per-project worker shares for concurrent groups: weighted by each
/// project's duration-cache total, minimum 1, summing to at most `budget`.
/// Unknown projects get the average known weight (first runs split evenly).
pub fn plan_shares(costs: &[Option<f64>], budget: usize) -> Vec<usize> {
    let n = costs.len();
    if n == 0 {
        return Vec::new();
    }
    let budget = budget.max(n); // every project gets at least one worker
    let known: Vec<f64> = costs.iter().flatten().copied().collect();
    let avg = if known.is_empty() {
        1.0
    } else {
        (known.iter().sum::<f64>() / known.len() as f64).max(0.001)
    };
    let weights: Vec<f64> = costs.iter().map(|c| c.unwrap_or(avg).max(0.001)).collect();
    let total: f64 = weights.iter().sum();
    // Reserve the 1-worker minimum for everyone, split the REST by weight:
    // floors can never overshoot the budget that way.
    let extra = budget - n;
    let mut shares: Vec<usize> = weights
        .iter()
        .map(|w| 1 + ((w / total) * extra as f64).floor() as usize)
        .collect();
    // Distribute the flooring remainder to the heaviest projects.
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| weights[b].total_cmp(&weights[a]));
    let mut remaining = budget - shares.iter().sum::<usize>().min(budget);
    for &i in order.iter().cycle().take(n * budget) {
        if remaining == 0 {
            break;
        }
        shares[i] += 1;
        remaining -= 1;
    }
    shares
}

/// A project's last-run cost for worker-share weighting. Prefers the recorded
/// whole-suite wall (fixture setup/teardown included), falling back to the sum
/// of call durations for caches written before wall tracking existed. Weighting
/// by call time alone starves a fixture-bound project (near-zero call time, tens
/// of seconds of fixtures) to a single worker on the warm run.
pub fn project_cost(project: &Path) -> Option<f64> {
    if let Some(wall) = crate::scheduling::durations::load_wall_in(project) {
        return Some(wall);
    }
    let bytes = std::fs::read(crate::cache::file_in(
        project,
        crate::scheduling::durations::FILE,
    ))
    .ok()?;
    let map: std::collections::HashMap<String, f64> = serde_json::from_slice(&bytes).ok()?;
    Some(map.values().sum())
}

#[cfg(test)]
mod share_tests {
    use super::plan_shares;

    #[test]
    fn cold_start_splits_evenly() {
        assert_eq!(plan_shares(&[None, None, None, None], 8), vec![2, 2, 2, 2]);
    }

    #[test]
    fn weighted_by_cost_with_minimum_one() {
        // 100s + 4 tiny projects on 14 workers: the heavy one dominates,
        // everyone still gets a worker.
        let shares = plan_shares(
            &[Some(100.0), Some(2.0), Some(1.0), Some(1.0), Some(1.0)],
            14,
        );
        assert_eq!(shares.iter().sum::<usize>(), 14);
        assert!(shares[0] >= 9, "{shares:?}");
        assert!(shares.iter().all(|&s| s >= 1), "{shares:?}");
    }

    #[test]
    fn unknown_projects_get_average_weight() {
        let shares = plan_shares(&[Some(10.0), None], 4);
        assert_eq!(shares.iter().sum::<usize>(), 4);
        assert_eq!(shares, vec![2, 2]); // unknown assumed average (=10)
    }

    #[test]
    fn more_projects_than_budget_still_one_each() {
        let shares = plan_shares(&[None, None, None], 2);
        assert!(shares.iter().all(|&s| s >= 1));
    }
}

#[cfg(test)]
mod cost_tests {
    use super::project_cost;
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rstest_cost_{}_{}", std::process::id(), tag));
        std::fs::create_dir_all(dir.join(".rstest_cache")).unwrap();
        dir
    }

    #[test]
    fn prefers_wall_over_call_durations() {
        // A fixture-bound project: near-zero call time, tens of seconds of wall.
        // The cost must reflect the wall, or the planner starves it.
        let dir = scratch("wall");
        std::fs::write(
            dir.join(".rstest_cache/durations.json"),
            br#"{"t::a":0.05,"t::b":0.05}"#,
        )
        .unwrap();
        std::fs::write(dir.join(".rstest_cache/wall.json"), b"35.3").unwrap();
        assert_eq!(project_cost(&dir), Some(35.3));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn falls_back_to_call_durations_without_wall() {
        // Pre-wall caches (durations.json only) still weight by call-time sum.
        let dir = scratch("nowall");
        std::fs::write(
            dir.join(".rstest_cache/durations.json"),
            br#"{"t::a":1.5,"t::b":0.5}"#,
        )
        .unwrap();
        assert_eq!(project_cost(&dir), Some(2.0));
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod fixed_tests {
    use super::plan_shares_with_fixed;

    #[test]
    fn pinned_projects_keep_their_n() {
        // project 1 pins -n 0 (single-worker exact mode)
        let shares =
            plan_shares_with_fixed(&[Some(50.0), Some(50.0), None], &[None, Some(0), None], 8);
        assert_eq!(shares[1], 0);
        // the others split the remaining budget
        assert_eq!(shares[0] + shares[2], 7);
        assert!(shares[0] >= 1 && shares[2] >= 1);
    }

    #[test]
    fn pin_clamps_to_budget() {
        let shares = plan_shares_with_fixed(&[None, None], &[Some(64), None], 4);
        assert_eq!(shares[0], 4);
        assert!(shares[1] >= 1);
    }

    #[test]
    fn all_pinned() {
        let shares = plan_shares_with_fixed(&[None, None], &[Some(2), Some(3)], 8);
        assert_eq!(shares, vec![2, 3]);
    }
}
