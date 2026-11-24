//! Per-run rerun bookkeeping: each item's retry budget (the global `--reruns`
//! or its `@pytest.mark.flaky(reruns=N)`), the attempts it has used, and the
//! `--only-rerun` / `--reruns-only-known-flaky` gates.

use std::collections::{HashMap, HashSet};

use super::nodeid_at;

pub(super) struct Reruns<'a> {
    /// The global `--reruns` budget.
    default: u32,
    pub(super) only_rerun: &'a [regex::Regex],
    known_flaky: Option<&'a HashSet<String>>,
    /// `@pytest.mark.flaky(reruns=N)` budgets, from the designate's payload.
    pub(super) marked: HashMap<u64, u32>,
    /// Retries used so far, per item.
    pub(super) used: HashMap<u64, u32>,
}

impl<'a> Reruns<'a> {
    pub(super) fn new(
        default: u32,
        only_rerun: &'a [regex::Regex],
        known_flaky: Option<&'a HashSet<String>>,
    ) -> Self {
        Self {
            default,
            only_rerun,
            known_flaky,
            marked: HashMap::new(),
            used: HashMap::new(),
        }
    }

    /// Item `i`'s retry budget: its flaky mark's, else the global one.
    pub(super) fn budget(&self, i: u64) -> u32 {
        self.marked.get(&i).copied().unwrap_or(self.default)
    }

    /// Whether `index` is rerun-eligible under `--reruns-only-known-flaky`: no gate
    /// set (feature off) always passes; otherwise an explicit `@mark.flaky` (present
    /// in `marked`) or a prior-flaky nodeid in the set qualifies. Matches on
    /// the clean nodeid from `ids` (not a report id, which may carry a `[gwN]`
    /// suffix in Dist::Each) so the ItemDone and crash gates stay consistent.
    pub(super) fn known_flaky_ok(&self, ids: &Option<Vec<String>>, index: u64) -> bool {
        self.known_flaky.is_none_or(|set| {
            self.marked.contains_key(&index)
                || nodeid_at(ids, index).is_some_and(|id| set.contains(id))
        })
    }
}
