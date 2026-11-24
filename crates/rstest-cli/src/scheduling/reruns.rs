//! Per-run rerun bookkeeping shared by the eager and lazy pools: each item's
//! retry budget (the global `--reruns` or its `@pytest.mark.flaky(reruns=N)`),
//! the attempts it has used, and the `--only-rerun` /
//! `--reruns-only-known-flaky` gates. `K` is how the pool names an item: its
//! collection index (eager) or its nodeid (lazy).

use std::collections::{HashMap, HashSet};
use std::hash::Hash;

pub(crate) struct Reruns<'a, K> {
    /// The global `--reruns` budget.
    default: u32,
    pub(crate) only_rerun: &'a [regex::Regex],
    known_flaky: Option<&'a HashSet<String>>,
    /// `@pytest.mark.flaky(reruns=N)` budgets, from the collection payload.
    pub(crate) marked: HashMap<K, u32>,
    /// Retries used so far, per item.
    pub(crate) used: HashMap<K, u32>,
}

impl<'a, K: Hash + Eq> Reruns<'a, K> {
    pub(crate) fn new(
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

    /// The item's retry budget: its flaky mark's, else the global one.
    pub(crate) fn budget(&self, key: &K) -> u32 {
        self.marked.get(key).copied().unwrap_or(self.default)
    }

    /// Whether the item is rerun-eligible under `--reruns-only-known-flaky`: no
    /// gate set (feature off) always passes; otherwise an explicit
    /// `@mark.flaky` (present in `marked`) or a prior-flaky nodeid in the set
    /// qualifies. `nodeid` is the CLEAN nodeid (not a report id, which may
    /// carry a `[gwN]` suffix under `--dist each`), so the finished-item and
    /// crash gates stay consistent.
    pub(crate) fn known_flaky_ok(&self, key: &K, nodeid: Option<&str>) -> bool {
        self.known_flaky.is_none_or(|set| {
            self.marked.contains_key(key) || nodeid.is_some_and(|id| set.contains(id))
        })
    }
}
