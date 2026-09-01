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
    pub(super) fn take(&mut self, want: usize, is_designate: bool) -> Take {
        let mut indices: Vec<u64> = Vec::new();
        while indices.len() < want {
            if let Some(i) = self.requeued.pop_front() {
                indices.push(i);
                continue;
            }
            if self.cursor < self.order.len() {
                match &self.group_ends {
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

pub(super) fn build_dispatch(
    ids: &[String],
    serial: Vec<u64>,
    groups: std::collections::HashMap<String, String>,
    cache: &std::collections::HashMap<String, f64>,
    dist: Dist,
    shuffle: Option<u64>,
    keep: Option<&HashSet<u64>>,
) -> Dispatch {
    // --shard filter: an index not in `keep` is deselected everywhere
    // (parallel order, serial phase, groups). None keeps everything.
    let kept = |i: &u64| keep.is_none_or(|k| k.contains(i));
    let serial: Vec<u64> = serial.into_iter().filter(|i| kept(i)).collect();
    let serial_set: HashSet<u64> = serial.iter().copied().collect();
    let parallel = || (0..ids.len() as u64).filter(|i| !serial_set.contains(i) && kept(i));

    let (order, slow_count, group_ends) = match dist {
        // Each mode never builds a dispatch queue (each worker is seeded
        // with the full suite); run_pool guards the call.
        Dist::Each => unreachable!("--dist each has no dispatch queue"),
        Dist::Load => {
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
    Dispatch {
        order,
        slow_count,
        cursor: 0,
        group_ends,
        requeued: VecDeque::new(),
        serial: serial.into(),
        serial_active: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
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
        let d = build_dispatch(
            &names,
            vec![1],
            HashMap::new(),
            &cache,
            Dist::Load,
            None,
            None,
        );
        // slow item first, serial index 1 absent, rest in collection order
        assert_eq!(d.order, vec![2, 0, 3]);
        assert_eq!(d.slow_count, 1);
        assert_eq!(d.serial, VecDeque::from(vec![1]));
    }

    #[test]
    fn shuffle_is_deterministic_and_defeats_duration_order() {
        let names = ids(&["t/a.py::t1", "t/a.py::t2", "t/b.py::t3", "t/b.py::t4"]);
        let mut cache = HashMap::new();
        cache.insert("t/b.py::t3".to_string(), 5.0);
        let a = build_dispatch(
            &names,
            vec![],
            HashMap::new(),
            &cache,
            Dist::Load,
            Some(7),
            None,
        );
        let b = build_dispatch(
            &names,
            vec![],
            HashMap::new(),
            &cache,
            Dist::Load,
            Some(7),
            None,
        );
        assert_eq!(a.order, b.order); // same seed, same order
        assert_eq!(a.slow_count, 0); // shuffle defeats long-pole-first
        let mut seen = a.order.clone();
        seen.sort_unstable();
        assert_eq!(seen, vec![0, 1, 2, 3]); // a permutation, nothing lost
                                            // Some seed must produce a different order than seed 7.
        assert!((0..20u64).any(|s| {
            build_dispatch(
                &names,
                vec![],
                HashMap::new(),
                &cache,
                Dist::Load,
                Some(s),
                None,
            )
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
            let mut d = build_dispatch(
                &names,
                vec![],
                HashMap::new(),
                &HashMap::new(),
                Dist::Loadfile,
                Some(seed),
                None,
            );
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
        let mut d = build_dispatch(
            &names,
            vec![],
            HashMap::new(),
            &HashMap::new(),
            Dist::Loadfile,
            None,
            None,
        );
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
        let mut d = build_dispatch(
            &names,
            vec![],
            HashMap::new(),
            &HashMap::new(),
            Dist::Loadscope,
            None,
            None,
        );
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
        let mut d = build_dispatch(
            &names,
            vec![],
            groups,
            &HashMap::new(),
            Dist::Loadgroup,
            None,
            None,
        );
        let batches = drain(&mut d, 1, false);
        // marked group lands at its first member's position, cross-file;
        // unmarked tests are singleton units
        assert_eq!(batches, vec![vec![0, 2], vec![1], vec![3]]);
    }

    #[test]
    fn take_serves_requeued_before_queue() {
        let names = ids(&["a.py::t1", "a.py::t2", "a.py::t3"]);
        let mut d = build_dispatch(
            &names,
            vec![],
            HashMap::new(),
            &HashMap::new(),
            Dist::Load,
            None,
            None,
        );
        d.requeued.push_back(2);
        match d.take(2, false) {
            Take::Items(items) => assert_eq!(items, vec![2, 0]),
            Take::Exhausted => panic!("expected items"),
        }
    }

    #[test]
    fn serial_only_for_active_designate() {
        let names = ids(&["a.py::t1", "a.py::t2"]);
        let mut d = build_dispatch(
            &names,
            vec![0, 1],
            HashMap::new(),
            &HashMap::new(),
            Dist::Load,
            None,
            None,
        );
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
}
