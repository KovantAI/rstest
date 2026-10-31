//! Dispatch queue and scheduling policy: build the ordered work queue from
//! the designated worker's id list (long-pole-first, affinity groups, shard
//! filter, shuffle) and hand out chunks. Pure scheduling; no worker I/O.

use std::collections::{HashSet, VecDeque};

use super::Dist;

/// Items per dispatch message (load mode). Contiguous ranges keep module
/// locality and cut protocol round-trips; sized so each worker sees ~16
/// refills.
pub(crate) fn chunk_size(total: usize, workers: usize) -> usize {
    (total / (workers * 16)).clamp(1, 64)
}

/// Dispatch queue, built from the designated worker's id list.
pub(super) struct Dispatch {
    /// Parallel-phase indices in dispatch order: cached long-poles first.
    pub(super) order: Vec<u64>,
    /// Length of the long-pole prefix (dispatched one at a time).
    pub(super) slow_count: usize,
    /// Unit end-positions inside that prefix when it holds fixture groups
    /// (see [`fixture_lead`]): each dispatch takes one whole unit. Empty: one
    /// item per dispatch.
    pub(super) lead_ends: Vec<usize>,
    pub(super) cursor: usize,
    /// Group end-positions in `order` (loadfile mode): a dispatch never
    /// splits a group.
    pub(super) group_ends: Option<Vec<usize>>,
    /// Items reclaimed from crashed workers; served before `order`.
    pub(super) requeued: VecDeque<u64>,
    /// @pytest.mark.serial items: run on the designate, exclusively,
    /// after all other workers are Done.
    pub(super) serial: VecDeque<u64>,
    pub(super) serial_active: bool,
}

pub(super) enum Take {
    Items(Vec<u64>),
    Exhausted,
}

impl Dispatch {
    /// The next take is a single long pole (head of `order`, nothing requeued
    /// ahead of it).
    pub(super) fn in_slow_zone(&self) -> bool {
        self.cursor < self.slow_count && self.requeued.is_empty()
    }

    /// Parallel-phase items not yet handed out (queue remainder + requeued).
    pub(super) fn pending(&self) -> usize {
        self.order.len() - self.cursor + self.requeued.len()
    }

    pub(super) fn take(&mut self, want: usize, is_designate: bool) -> Take {
        let mut indices: Vec<u64> = Vec::new();
        while indices.len() < want {
            if let Some(i) = self.requeued.pop_front() {
                indices.push(i);
                continue;
            }
            if self.cursor < self.order.len() {
                match &self.group_ends {
                    None if self.cursor < self.slow_count && !self.lead_ends.is_empty() => {
                        // One lead unit per dispatch: a fixture group (or a
                        // long pole) goes to one worker whole.
                        if !indices.is_empty() {
                            break;
                        }
                        let end =
                            self.lead_ends[self.lead_ends.partition_point(|&e| e <= self.cursor)];
                        indices.extend_from_slice(&self.order[self.cursor..end]);
                        self.cursor = end;
                        break;
                    }
                    None => {
                        indices.push(self.order[self.cursor]);
                        self.cursor += 1;
                    }
                    Some(ends) => {
                        // Whole-file group: take it all, regardless of `want`.
                        if !indices.is_empty() {
                            break; // one group per dispatch
                        }
                        // First boundary STRICTLY past cursor (a boundary
                        // can equal cursor when a group just ended).
                        let end = match ends.binary_search(&self.cursor) {
                            Ok(pos) => ends[pos + 1],
                            Err(pos) => ends[pos],
                        };
                        indices.extend_from_slice(&self.order[self.cursor..end]);
                        self.cursor = end;
                    }
                }
                continue;
            }
            break;
        }
        if !indices.is_empty() {
            return Take::Items(indices);
        }
        if !self.serial.is_empty() && self.serial_active && is_designate {
            let n = want.max(1).min(self.serial.len());
            return Take::Items(self.serial.drain(..n).collect());
        }
        // Serial items not yet runnable (or not ours): exhausted FOR NOW.
        // Workers keep listening after their queue release, so the serial
        // phase reaches the designate later as ordinary run_items.
        Take::Exhausted
    }
}

/// SplitMix64: tiny, deterministic, platform-stable RNG for --shuffle.
/// The seed is printed for reproduction, so the permutation must be a
/// pure function of it: no std RandomState / platform variance.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// Seeded Fisher-Yates.
fn shuffle_slice<T>(v: &mut [T], seed: u64) {
    let mut rng = SplitMix64(seed);
    for i in (1..v.len()).rev() {
        let j = (rng.next() % (i as u64 + 1)) as usize;
        v.swap(i, j);
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn build_dispatch(
    ids: &[String],
    serial: Vec<u64>,
    groups: std::collections::HashMap<String, String>,
    cache: &std::collections::HashMap<String, f64>,
    flakes: &std::collections::HashMap<String, crate::reporting::flakes::FlakeStats>,
    scope_costs: &std::collections::HashMap<String, f64>,
    workers: usize,
    dist: Dist,
    order: super::Order,
    shuffle: Option<u64>,
    keep: Option<&HashSet<u64>>,
) -> Option<Dispatch> {
    // --shard filter: an index not in `keep` is deselected everywhere
    // (parallel order, serial phase, groups). None keeps everything.
    let kept = |i: &u64| keep.is_none_or(|k| k.contains(i));
    let serial: Vec<u64> = serial.into_iter().filter(|i| kept(i)).collect();
    let serial_set: HashSet<u64> = serial.iter().copied().collect();
    let parallel = || (0..ids.len() as u64).filter(|i| !serial_set.contains(i) && kept(i));

    let mut lead_ends = Vec::new();
    let (order, slow_count, group_ends) = match dist {
        // Each mode has no dispatch queue (each worker is seeded with the
        // full suite), so there is nothing to build.
        Dist::Each => return None,
        Dist::Load => match order {
            // Shuffle defeats any lead order (see below), so it never groups.
            super::Order::Throughput if shuffle.is_none() && workers > 1 => {
                let parallel: Vec<u64> = parallel().collect();
                match fixture_lead(ids, &parallel, cache, scope_costs, workers) {
                    Some((order, ends)) => {
                        let slow_count = ends.last().copied().unwrap_or(0);
                        lead_ends = ends;
                        (order, slow_count, None)
                    }
                    None => throughput(ids, cache, &serial_set, &kept),
                }
            }
            super::Order::Throughput => throughput(ids, cache, &serial_set, &kept),
            // Fail-fast: suspects (failed/flaky) first, then the throughput
            // order. The single-dispatch prefix covers the suspects AND the
            // long poles, so reds spread across workers at once and slow
            // tests still don't pile onto one worker's chunk.
            super::Order::FailFast => {
                use crate::scheduling::durations::{
                    failfast_order, is_suspect, FAILFAST_SUSPECT_CAP, SLOW_THRESHOLD_SECS,
                };
                let order =
                    failfast_order(ids, cache, flakes, |i| !serial_set.contains(&i) && kept(&i));
                // Single-dispatch prefix: the (capped) suspect lead, then the
                // long poles. The order is already filtered, so the cap here
                // matches the one failfast_order applied.
                let lead = order
                    .iter()
                    .take(FAILFAST_SUSPECT_CAP)
                    .take_while(|&&i| is_suspect(&ids[i as usize], flakes))
                    .count();
                let slow_count = lead
                    + order[lead..]
                        .iter()
                        .take_while(|&&i| {
                            cache
                                .get(&ids[i as usize])
                                .is_some_and(|&d| d >= SLOW_THRESHOLD_SECS)
                        })
                        .count();
                (order, slow_count, None)
            }
        },
        // Affinity modes: collection order, grouped by a key; a dispatch
        // never splits a group. Duration reordering is off; affinity is
        // the point.
        Dist::Loadfile | Dist::Loadscope => {
            let key = |i: u64| -> &str {
                let id = ids[i as usize].as_str();
                match dist {
                    // whole file
                    Dist::Loadfile => id.split("::").next().unwrap_or(id),
                    // fixture scope: drop the last segment (test name);
                    // class methods key on file::Class, module functions
                    // on the file.
                    _ => id.rsplit_once("::").map(|(head, _)| head).unwrap_or(id),
                }
            };
            let order: Vec<u64> = parallel().collect();
            let mut ends = Vec::new();
            for w in 1..order.len() {
                if key(order[w]) != key(order[w - 1]) {
                    ends.push(w);
                }
            }
            ends.push(order.len());
            (order, 0, Some(ends))
        }
        Dist::Loadgroup => {
            // Consolidate marked groups (possibly spanning files) into
            // contiguous units at the first member's position; unmarked
            // tests are singleton units (≈ load behavior).
            let mut units: Vec<Vec<u64>> = Vec::new();
            let mut unit_of: std::collections::HashMap<&str, usize> =
                std::collections::HashMap::new();
            for i in parallel() {
                match groups.get(&i.to_string()) {
                    Some(name) => match unit_of.get(name.as_str()) {
                        Some(&u) => units[u].push(i),
                        None => {
                            unit_of.insert(name.as_str(), units.len());
                            units.push(vec![i]);
                        }
                    },
                    None => units.push(vec![i]),
                }
            }
            let mut order = Vec::new();
            let mut ends = Vec::new();
            for unit in units {
                order.extend(unit);
                ends.push(order.len());
            }
            (order, 0, Some(ends))
        }
    };
    let (mut order, mut slow_count, mut group_ends, mut serial) =
        (order, slow_count, group_ends, serial);
    if let Some(seed) = shuffle {
        match group_ends.take() {
            // Affinity modes: shuffle GROUP order, keep each group's
            // internal order intact: in-group order is the affinity
            // contract (loadfile is the order-dependent-suite remedy).
            Some(ends) => {
                let mut units: Vec<&[u64]> = Vec::new();
                let mut start = 0;
                for &end in &ends {
                    units.push(&order[start..end]);
                    start = end;
                }
                shuffle_slice(&mut units, seed);
                let mut new_order = Vec::with_capacity(order.len());
                let mut new_ends = Vec::with_capacity(units.len());
                for unit in units {
                    new_order.extend_from_slice(unit);
                    new_ends.push(new_order.len());
                }
                order = new_order;
                group_ends = Some(new_ends);
            }
            // Load mode: the shuffle IS the order; duration-aware
            // long-pole-first sequencing is deliberately defeated.
            None => {
                shuffle_slice(&mut order, seed);
                slow_count = 0;
            }
        }
        // A different stream position for the serial phase, so it isn't
        // the same permutation pattern as the parallel one.
        shuffle_slice(&mut serial, seed.wrapping_add(1));
    }
    Some(Dispatch {
        order,
        slow_count,
        cursor: 0,
        group_ends,
        requeued: VecDeque::new(),
        serial: serial.into(),
        serial_active: false,
        lead_ends,
    })
}

/// `--dist load`'s throughput order: cached long poles first (longest first),
/// then the rest in collection order. Returns (order, slow_count, None).
fn throughput(
    ids: &[String],
    cache: &std::collections::HashMap<String, f64>,
    serial_set: &HashSet<u64>,
    kept: &impl Fn(&u64) -> bool,
) -> (Vec<u64>, usize, Option<Vec<usize>>) {
    let full = crate::scheduling::durations::dispatch_order(ids, cache);
    let order: Vec<u64> = full
        .into_iter()
        .filter(|i| !serial_set.contains(i) && kept(i))
        .collect();
    let slow_count = order
        .iter()
        .take_while(|&&i| {
            cache
                .get(&ids[i as usize])
                .is_some_and(|&d| d >= crate::scheduling::durations::SLOW_THRESHOLD_SECS)
        })
        .count();
    (order, slow_count, None)
}

/// A module or class whose module/class-scoped fixtures take at least this
/// long to set up keeps its tests on one worker.
pub(crate) const GROUP_MIN_SETUP_SECS: f64 = 0.5;

/// Fixture-cost-aware lead for `--dist load`. Chunked dispatch spreads a
/// module over several workers, and each one pays its module fixtures again.
/// Here the tests of every module (or class) whose fixture setup costs at
/// least [`GROUP_MIN_SETUP_SECS`] form one unit sent to one worker, split only
/// where a piece more saves more wall time than the setup it repeats. The units
/// and the remaining long poles lead the queue, biggest first; the rest
/// follows in collection order. Returns (order, unit ends), or `None` when no
/// scope is costly enough (the plain throughput order then applies).
fn fixture_lead(
    ids: &[String],
    parallel: &[u64],
    cache: &std::collections::HashMap<String, f64>,
    scope_costs: &std::collections::HashMap<String, f64>,
    workers: usize,
) -> Option<(Vec<u64>, Vec<usize>)> {
    use crate::scheduling::durations::SLOW_THRESHOLD_SECS;
    // The widest costly scope wins: a costly module groups its classes too.
    let scope_of = |id: &str| {
        id.match_indices("::").find_map(|(pos, _)| {
            scope_costs
                .get(&id[..pos])
                .filter(|&&c| c >= GROUP_MIN_SETUP_SECS)
                .map(|&c| (pos, c))
        })
    };
    let dur = |i: u64| cache.get(&ids[i as usize]).copied().unwrap_or(0.0);
    let mut groups: Vec<(f64, Vec<u64>)> = Vec::new();
    let mut group_of: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    let mut rest = Vec::new();
    for &i in parallel {
        let id = ids[i as usize].as_str();
        match scope_of(id) {
            Some((pos, cost)) => {
                let g = *group_of.entry(&id[..pos]).or_insert_with(|| {
                    groups.push((cost, Vec::new()));
                    groups.len() - 1
                });
                groups[g].1.push(i);
            }
            None => rest.push(i),
        }
    }
    if groups.is_empty() {
        return None;
    }
    let total: f64 =
        parallel.iter().map(|&i| dur(i)).sum::<f64>() + groups.iter().map(|g| g.0).sum::<f64>();
    let pool = workers as f64;
    // (expected seconds, items): fixture groups, then the ungrouped long poles.
    let mut units: Vec<(f64, Vec<u64>)> = Vec::new();
    for (cost, members) in groups {
        let calls: f64 = members.iter().map(|&i| dur(i)).sum();
        // Wall-time floor with the group in k pieces: its longest piece, or
        // the pool's average once each extra piece repeats the setup.
        let wall = |k: usize| (cost + calls / k as f64).max((total + (k - 1) as f64 * cost) / pool);
        // Another piece only when it saves more wall time than the setup it
        // repeats.
        let mut pieces = 1;
        while pieces < workers.min(members.len()) && wall(pieces) - wall(pieces + 1) > cost {
            pieces += 1;
        }
        // ponytail: pieces split by count; weight by duration if they come out uneven.
        for piece in members.chunks(members.len().div_ceil(pieces)) {
            units.push((
                cost + piece.iter().map(|&i| dur(i)).sum::<f64>(),
                piece.to_vec(),
            ));
        }
    }
    let mut tail = Vec::new();
    for i in rest {
        let d = dur(i);
        if d >= SLOW_THRESHOLD_SECS {
            units.push((d, vec![i]));
        } else {
            tail.push(i);
        }
    }
    units.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut order = Vec::with_capacity(parallel.len());
    let mut ends = Vec::with_capacity(units.len());
    for (_, unit) in units {
        order.extend(unit);
        ends.push(order.len());
    }
    order.extend(tail);
    Some((order, ends))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// Throughput-order build_dispatch with no flake history — the shape the
    /// pre-fail-fast tests exercised. Fail-fast has its own tests below.
    #[allow(clippy::too_many_arguments)]
    fn dispatch(
        ids: &[String],
        serial: Vec<u64>,
        groups: HashMap<String, String>,
        cache: &HashMap<String, f64>,
        dist: Dist,
        shuffle: Option<u64>,
        keep: Option<&HashSet<u64>>,
    ) -> Option<Dispatch> {
        build_dispatch(
            ids,
            serial,
            groups,
            cache,
            &HashMap::new(),
            &HashMap::new(),
            4,
            dist,
            super::super::Order::Throughput,
            shuffle,
            keep,
        )
    }

    fn drain(d: &mut Dispatch, want: usize, designate: bool) -> Vec<Vec<u64>> {
        let mut batches = Vec::new();
        loop {
            match d.take(want, designate) {
                Take::Items(items) => batches.push(items),
                Take::Exhausted => return batches,
            }
        }
    }

    #[test]
    fn chunk_sizes() {
        // Small suites floor at 1; never exceed 64.
        assert_eq!(chunk_size(4, 8), 1);
        assert_eq!(chunk_size(1600, 2), 50);
        assert_eq!(chunk_size(1_000_000, 4), 64);
    }

    #[test]
    fn load_orders_slow_first_and_excludes_serial() {
        let names = ids(&["t/a.py::t1", "t/a.py::t2", "t/b.py::t3", "t/b.py::t4"]);
        let mut cache = HashMap::new();
        cache.insert("t/b.py::t3".to_string(), 5.0); // long pole
        let d = dispatch(
            &names,
            vec![1],
            HashMap::new(),
            &cache,
            Dist::Load,
            None,
            None,
        )
        .unwrap();
        // slow item first, serial index 1 absent, rest in collection order
        assert_eq!(d.order, vec![2, 0, 3]);
        assert_eq!(d.slow_count, 1);
        assert_eq!(d.serial, VecDeque::from(vec![1]));
    }

    #[test]
    fn failfast_dispatch_puts_red_first_and_singles_red_and_long_poles() {
        use crate::reporting::flakes::FlakeStats;
        let names = ids(&["t/a.py::ok_slow", "t/a.py::red", "t/b.py::ok_fast"]);
        let cache = HashMap::from([
            ("t/a.py::ok_slow".to_string(), 9.0),
            ("t/b.py::ok_fast".to_string(), 0.1),
        ]);
        let flakes = HashMap::from([(
            "t/a.py::red".to_string(),
            FlakeStats {
                failed: 1,
                ..Default::default()
            },
        )]);
        let d = build_dispatch(
            &names,
            vec![],
            HashMap::new(),
            &cache,
            &flakes,
            &HashMap::new(),
            4,
            Dist::Load,
            super::super::Order::FailFast,
            None,
            None,
        )
        .unwrap();
        // red (index 1) first, then the throughput tail: long pole, then rest.
        assert_eq!(d.order, vec![1, 0, 2]);
        // Red and the long pole both dispatch one at a time.
        assert_eq!(d.slow_count, 2);
    }

    #[test]
    fn failfast_dispatch_caps_single_dispatch_suspect_prefix() {
        use crate::reporting::flakes::FlakeStats;
        use crate::scheduling::durations::FAILFAST_SUSPECT_CAP;
        // Mass failure: more suspects than the cap, none slow.
        let n = FAILFAST_SUSPECT_CAP + 50;
        let owned: Vec<String> = (0..n).map(|i| format!("t/a.py::t{i}")).collect();
        let flakes: HashMap<String, FlakeStats> = owned
            .iter()
            .map(|id| {
                (
                    id.clone(),
                    FlakeStats {
                        failed: 1,
                        last_epoch: 1,
                        ..Default::default()
                    },
                )
            })
            .collect();
        let d = build_dispatch(
            &owned,
            vec![],
            HashMap::new(),
            &HashMap::new(),
            &flakes,
            &HashMap::new(),
            4,
            Dist::Load,
            super::super::Order::FailFast,
            None,
            None,
        )
        .unwrap();
        assert_eq!(d.slow_count, FAILFAST_SUSPECT_CAP);
    }

    #[test]
    fn shuffle_is_deterministic_and_defeats_duration_order() {
        let names = ids(&["t/a.py::t1", "t/a.py::t2", "t/b.py::t3", "t/b.py::t4"]);
        let mut cache = HashMap::new();
        cache.insert("t/b.py::t3".to_string(), 5.0);
        let a = dispatch(
            &names,
            vec![],
            HashMap::new(),
            &cache,
            Dist::Load,
            Some(7),
            None,
        )
        .unwrap();
        let b = dispatch(
            &names,
            vec![],
            HashMap::new(),
            &cache,
            Dist::Load,
            Some(7),
            None,
        )
        .unwrap();
        assert_eq!(a.order, b.order); // same seed, same order
        assert_eq!(a.slow_count, 0); // shuffle defeats long-pole-first
        let mut seen = a.order.clone();
        seen.sort_unstable();
        assert_eq!(seen, vec![0, 1, 2, 3]); // a permutation, nothing lost
                                            // Some seed must produce a different order than seed 7.
        assert!((0..20u64).any(|s| {
            dispatch(
                &names,
                vec![],
                HashMap::new(),
                &cache,
                Dist::Load,
                Some(s),
                None,
            )
            .unwrap()
            .order
                != a.order
        }));
    }

    #[test]
    fn shuffle_keeps_groups_contiguous() {
        let names = ids(&[
            "t/a.py::t1",
            "t/a.py::t2",
            "t/a.py::t3",
            "t/b.py::t4",
            "t/b.py::t5",
        ]);
        for seed in 0..10u64 {
            let mut d = dispatch(
                &names,
                vec![],
                HashMap::new(),
                &HashMap::new(),
                Dist::Loadfile,
                Some(seed),
                None,
            )
            .unwrap();
            let batches = drain(&mut d, 1, false);
            // Whole files, in-file order intact; only group ORDER varies.
            assert!(
                batches == vec![vec![0, 1, 2], vec![3, 4]]
                    || batches == vec![vec![3, 4], vec![0, 1, 2]],
                "seed {seed}: {batches:?}"
            );
        }
    }

    #[test]
    fn loadfile_never_splits_a_file() {
        let names = ids(&[
            "t/a.py::t1",
            "t/a.py::t2",
            "t/a.py::t3",
            "t/b.py::t4",
            "t/b.py::t5",
        ]);
        let mut d = dispatch(
            &names,
            vec![],
            HashMap::new(),
            &HashMap::new(),
            Dist::Loadfile,
            None,
            None,
        )
        .unwrap();
        // want=1 but whole groups must come out regardless
        let batches = drain(&mut d, 1, false);
        assert_eq!(batches, vec![vec![0, 1, 2], vec![3, 4]]);
    }

    #[test]
    fn loadscope_groups_by_class() {
        let names = ids(&[
            "t/a.py::TestX::t1",
            "t/a.py::TestX::t2",
            "t/a.py::TestY::t3",
            "t/a.py::t4",
        ]);
        let mut d = dispatch(
            &names,
            vec![],
            HashMap::new(),
            &HashMap::new(),
            Dist::Loadscope,
            None,
            None,
        )
        .unwrap();
        let batches = drain(&mut d, 1, false);
        // TestX together; TestY alone; module-level function keys on the file
        assert_eq!(batches, vec![vec![0, 1], vec![2], vec![3]]);
    }

    #[test]
    fn loadgroup_consolidates_marks_across_files() {
        let names = ids(&[
            "t/a.py::t1", // group g
            "t/a.py::t2",
            "t/b.py::t3", // group g (different file)
            "t/b.py::t4",
        ]);
        let mut groups = HashMap::new();
        groups.insert("0".to_string(), "g".to_string());
        groups.insert("2".to_string(), "g".to_string());
        let mut d = dispatch(
            &names,
            vec![],
            groups,
            &HashMap::new(),
            Dist::Loadgroup,
            None,
            None,
        )
        .unwrap();
        let batches = drain(&mut d, 1, false);
        // marked group lands at its first member's position, cross-file;
        // unmarked tests are singleton units
        assert_eq!(batches, vec![vec![0, 2], vec![1], vec![3]]);
    }

    #[test]
    fn take_serves_requeued_before_queue() {
        let names = ids(&["a.py::t1", "a.py::t2", "a.py::t3"]);
        let mut d = dispatch(
            &names,
            vec![],
            HashMap::new(),
            &HashMap::new(),
            Dist::Load,
            None,
            None,
        )
        .unwrap();
        d.requeued.push_back(2);
        match d.take(2, false) {
            Take::Items(items) => assert_eq!(items, vec![2, 0]),
            Take::Exhausted => panic!("expected items"),
        }
    }

    #[test]
    fn serial_only_for_active_designate() {
        let names = ids(&["a.py::t1", "a.py::t2"]);
        let mut d = dispatch(
            &names,
            vec![0, 1],
            HashMap::new(),
            &HashMap::new(),
            Dist::Load,
            None,
            None,
        )
        .unwrap();
        // parallel queue is empty (all serial); inactive phase = exhausted
        assert!(matches!(d.take(2, true), Take::Exhausted));
        d.serial_active = true;
        // non-designate never receives serial items
        assert!(matches!(d.take(2, false), Take::Exhausted));
        match d.take(2, true) {
            Take::Items(items) => assert_eq!(items, vec![0, 1]),
            Take::Exhausted => panic!("designate should get serial items"),
        }
    }

    #[test]
    fn each_mode_builds_no_dispatch() {
        let names = ids(&["a.py::t1", "a.py::t2"]);
        let d = dispatch(
            &names,
            vec![],
            HashMap::new(),
            &HashMap::new(),
            Dist::Each,
            None,
            None,
        );
        assert!(d.is_none());
    }

    /// Throughput build with saved module/class setup costs and a pool size.
    fn grouped(
        ids: &[String],
        cache: &HashMap<String, f64>,
        scope_costs: &HashMap<String, f64>,
        workers: usize,
        shuffle: Option<u64>,
    ) -> Dispatch {
        build_dispatch(
            ids,
            vec![],
            HashMap::new(),
            cache,
            &HashMap::new(),
            scope_costs,
            workers,
            Dist::Load,
            super::super::Order::Throughput,
            shuffle,
            None,
        )
        .unwrap()
    }

    fn costs(entries: &[(&str, f64)]) -> HashMap<String, f64> {
        entries.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn a_costly_module_goes_to_one_worker_whole() {
        // 8 cheap tests in a.py, 6 in db.py whose module fixture takes 3s.
        let names: Vec<String> = (0..8)
            .map(|i| format!("a.py::t{i}"))
            .chain((0..6).map(|i| format!("db.py::t{i}")))
            .collect();
        let cache: HashMap<String, f64> = names.iter().map(|n| (n.clone(), 0.1)).collect();
        let mut d = grouped(&names, &cache, &costs(&[("db.py", 3.0)]), 4, None);
        // db.py leads as ONE dispatch, however small the chunk; then a.py.
        let batches = drain(&mut d, 2, false);
        assert_eq!(batches[0], vec![8, 9, 10, 11, 12, 13]);
        assert_eq!(batches[1..].concat(), (0..8).collect::<Vec<u64>>());
        assert!(batches[1..].iter().all(|b| b.len() <= 2));
    }

    #[test]
    fn a_cheap_or_unknown_scope_keeps_the_throughput_order() {
        let names = ids(&["a.py::t0", "a.py::t1", "b.py::t0", "b.py::t1"]);
        let cache: HashMap<String, f64> = HashMap::from([("b.py::t1".to_string(), 2.0)]);
        let plain = dispatch(
            &names,
            vec![],
            HashMap::new(),
            &cache,
            Dist::Load,
            None,
            None,
        )
        .unwrap();
        // Below the threshold, and a costly scope no test belongs to.
        for sc in [costs(&[("a.py", 0.1)]), costs(&[("gone.py", 9.0)])] {
            let d = grouped(&names, &cache, &sc, 4, None);
            assert_eq!(d.order, plain.order);
            assert_eq!(d.slow_count, plain.slow_count);
            assert!(d.lead_ends.is_empty());
        }
    }

    #[test]
    fn a_costly_class_groups_only_its_own_tests() {
        let names = ids(&[
            "m.py::TestDb::t0",
            "m.py::test_free",
            "m.py::TestDb::t1",
            "m.py::TestOther::t0",
        ]);
        let mut d = grouped(
            &names,
            &HashMap::new(),
            &costs(&[("m.py::TestDb", 2.0)]),
            2,
            None,
        );
        let batches = drain(&mut d, 1, false);
        assert_eq!(batches[0], vec![0, 2]);
        assert_eq!(batches[1..].concat(), vec![1, 3]);
    }

    #[test]
    fn a_costly_module_beats_its_costly_class() {
        let names = ids(&["m.py::TestA::t0", "m.py::t1", "n.py::t0"]);
        let sc = costs(&[("m.py", 1.0), ("m.py::TestA", 5.0)]);
        let mut d = grouped(&names, &HashMap::new(), &sc, 2, None);
        assert_eq!(drain(&mut d, 1, false)[0], vec![0, 1]);
    }

    #[test]
    fn a_group_bigger_than_a_workers_share_is_split_into_balanced_pieces() {
        // One 1s setup over 40 tests of 1s: 41s of work for 4 workers (share
        // ~10.25s) splits into 4 pieces of 10, each paying the setup again.
        let names: Vec<String> = (0..40).map(|i| format!("db.py::t{i}")).collect();
        let cache: HashMap<String, f64> = names.iter().map(|n| (n.clone(), 1.0)).collect();
        let mut d = grouped(&names, &cache, &costs(&[("db.py", 1.0)]), 4, None);
        let batches = drain(&mut d, 64, false);
        assert_eq!(batches.len(), 4);
        assert!(batches.iter().all(|b| b.len() == 10));
    }

    #[test]
    fn units_and_long_poles_lead_biggest_first() {
        // db.py: 2s setup + 2 x 0.5s = 3s; slow.py::t: a lone 5s long pole.
        let names = ids(&["db.py::t0", "db.py::t1", "slow.py::t", "x.py::t"]);
        let cache = costs(&[
            ("db.py::t0", 0.5),
            ("db.py::t1", 0.5),
            ("slow.py::t", 5.0),
            ("x.py::t", 0.1),
        ]);
        let mut d = grouped(&names, &cache, &costs(&[("db.py", 2.0)]), 4, None);
        assert_eq!(drain(&mut d, 8, false), vec![vec![2], vec![0, 1], vec![3]]);
    }

    #[test]
    fn shuffle_and_a_single_worker_never_group() {
        let names = ids(&["db.py::t0", "a.py::t0", "db.py::t1"]);
        let sc = costs(&[("db.py", 3.0)]);
        assert!(grouped(&names, &HashMap::new(), &sc, 4, Some(7))
            .lead_ends
            .is_empty());
        assert!(grouped(&names, &HashMap::new(), &sc, 1, None)
            .lead_ends
            .is_empty());
    }

    #[test]
    fn a_requeued_item_is_not_merged_into_a_lead_unit() {
        let names = ids(&["db.py::t0", "db.py::t1", "a.py::t0"]);
        let mut d = grouped(&names, &HashMap::new(), &costs(&[("db.py", 3.0)]), 2, None);
        d.requeued.push_back(2);
        assert_eq!(drain(&mut d, 1, false), vec![vec![2], vec![0, 1], vec![2]]);
    }
}
