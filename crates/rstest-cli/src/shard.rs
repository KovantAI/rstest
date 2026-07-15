//! CI sharding: partition the suite into `N` independent buckets and keep
//! bucket `K` (`--shard K/N`, K is 1-based). Each CI job runs one bucket;
//! buckets are disjoint and their union is the whole suite, so merging the
//! per-job JUnit reconstructs the full run. No cross-job orchestration —
//! every job partitions the same list the same way and keeps its slice.
//!
//! Balance uses the duration cache (`.rstest_cache/durations.json`): LPT
//! bin-packing (longest-processing-time-first) drops each test into the
//! currently-lightest bucket, so per-shard wall time stays even even when a
//! few tests dominate. Tests with no cached timing take the average known
//! weight, so a fully cold cache degrades to an even round-robin count
//! split. Deterministic: identical `(ids, cache, k, n)` yields the same
//! bucket on every machine — which is exactly what lets N jobs partition
//! without talking. Restore the SAME duration cache on every job.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Parse a `K/N` shard spec: K 1-based, `1 <= K <= N`, `N >= 1`.
pub fn parse_shard(spec: &str) -> anyhow::Result<(usize, usize)> {
    let (k, n) = spec
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("--shard must be K/N (e.g. 2/4), got '{spec}'"))?;
    let k: usize = k
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("--shard K must be a positive integer, got '{k}'"))?;
    let n: usize = n
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("--shard N must be a positive integer, got '{n}'"))?;
    if n == 0 {
        anyhow::bail!("--shard N must be >= 1");
    }
    if k == 0 || k > n {
        anyhow::bail!("--shard K must be in 1..={n}, got {k}");
    }
    Ok((k, n))
}

/// LPT assignment: return the bucket index (0-based, `0..n`) each item lands
/// in. Heaviest first; each goes to the currently-lightest bucket. Ties
/// (equal weight, equal load) break by lowest index — so equal weights
/// (cold cache) produce a round-robin even split.
fn lpt_assign(weights: &[f64], n: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..weights.len()).collect();
    order.sort_by(|&a, &b| weights[b].total_cmp(&weights[a]).then(a.cmp(&b)));
    let mut loads = vec![0.0f64; n];
    let mut assign = vec![0usize; weights.len()];
    for i in order {
        let b = (0..n)
            .min_by(|&x, &y| loads[x].total_cmp(&loads[y]).then(x.cmp(&y)))
            .unwrap();
        loads[b] += weights[i].max(0.0);
        assign[i] = b;
    }
    assign
}

/// Fill untimed tests with the average known weight so they don't all pile
/// into one bucket; a fully cold cache leaves every weight equal.
fn weights_from(ids: &[String], cache: &HashMap<String, f64>) -> Vec<f64> {
    let known: Vec<f64> = ids.iter().filter_map(|id| cache.get(id).copied()).collect();
    let avg = if known.is_empty() {
        1.0
    } else {
        known.iter().sum::<f64>() / known.len() as f64
    };
    ids.iter()
        .map(|id| cache.get(id).copied().unwrap_or(avg))
        .collect()
}

/// Indices of `ids` assigned to shard `k` of `n` (k 1-based), in collection
/// order. `n <= 1` keeps everything.
pub fn shard_indices(ids: &[String], cache: &HashMap<String, f64>, k: usize, n: usize) -> Vec<u64> {
    if n <= 1 {
        return (0..ids.len() as u64).collect();
    }
    let assign = lpt_assign(&weights_from(ids, cache), n);
    let mut out: Vec<u64> = assign
        .iter()
        .enumerate()
        .filter(|(_, &b)| b == k - 1)
        .map(|(i, _)| i as u64)
        .collect();
    out.sort_unstable();
    out
}

/// Files assigned to shard `k` of `n` (k 1-based). Each file weighs the sum
/// of its tests' cached durations (the lazy pool shards at file grain).
/// Preserves the incoming file order within the kept bucket.
pub fn shard_files(
    files: &[PathBuf],
    cache: &HashMap<String, f64>,
    cwd: &Path,
    k: usize,
    n: usize,
) -> Vec<PathBuf> {
    if n <= 1 {
        return files.to_vec();
    }
    // Cache keys are nodeids relative to the invocation dir; sum per file.
    let mut totals: HashMap<String, f64> = HashMap::new();
    for (id, secs) in cache {
        let file = id.split("::").next().unwrap_or(id);
        *totals.entry(file.to_string()).or_insert(0.0) += secs;
    }
    let rel = |f: &Path| -> String {
        f.strip_prefix(cwd)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| f.to_string_lossy().into_owned())
    };
    let weights: Vec<f64> = {
        // Untimed files get the average known file weight.
        let known: Vec<f64> = files
            .iter()
            .filter_map(|f| totals.get(&rel(f)).copied())
            .collect();
        let avg = if known.is_empty() {
            1.0
        } else {
            known.iter().sum::<f64>() / known.len() as f64
        };
        files
            .iter()
            .map(|f| totals.get(&rel(f)).copied().unwrap_or(avg))
            .collect()
    };
    let assign = lpt_assign(&weights, n);
    files
        .iter()
        .zip(&assign)
        .filter(|(_, &b)| b == k - 1)
        .map(|(f, _)| f.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_valid_and_invalid() {
        assert_eq!(parse_shard("2/4").unwrap(), (2, 4));
        assert_eq!(parse_shard("1/1").unwrap(), (1, 1));
        assert!(parse_shard("0/4").is_err()); // K must be >= 1
        assert!(parse_shard("5/4").is_err()); // K must be <= N
        assert!(parse_shard("2/0").is_err()); // N must be >= 1
        assert!(parse_shard("2").is_err()); // needs a slash
        assert!(parse_shard("a/4").is_err()); // non-integer
    }

    #[test]
    fn buckets_partition_the_suite() {
        // Union of all shards == full set, and shards are disjoint.
        let names: Vec<String> = (0..50).map(|i| format!("t{i}")).collect();
        let cache: HashMap<String, f64> =
            (0..50).map(|i| (format!("t{i}"), (i % 7) as f64)).collect();
        let n = 4;
        let mut seen = Vec::new();
        for k in 1..=n {
            let mut bucket = shard_indices(&names, &cache, k, n);
            seen.append(&mut bucket);
        }
        seen.sort_unstable();
        assert_eq!(seen, (0..50u64).collect::<Vec<_>>());
    }

    #[test]
    fn cold_cache_splits_evenly_by_count() {
        let names: Vec<String> = (0..12).map(|i| format!("t{i}")).collect();
        let empty = HashMap::new();
        let sizes: Vec<usize> = (1..=3)
            .map(|k| shard_indices(&names, &empty, k, 3).len())
            .collect();
        assert_eq!(sizes, vec![4, 4, 4]); // round-robin, no cache
    }

    #[test]
    fn warm_cache_balances_wall_time() {
        // One 100s hog + many 1s tests: the hog's bucket must not also get a
        // disproportionate pile of the small ones.
        let names = ids(&["hog", "a", "b", "c", "d", "e", "f", "g"]);
        let mut cache = HashMap::new();
        cache.insert("hog".to_string(), 100.0);
        for t in ["a", "b", "c", "d", "e", "f", "g"] {
            cache.insert(t.to_string(), 1.0);
        }
        // Bucket with the hog gets only the hog (100 >> 7*1).
        let with_hog: Vec<Vec<u64>> = (1..=2)
            .map(|k| shard_indices(&names, &cache, k, 2))
            .collect();
        let hog_bucket = if with_hog[0].contains(&0) { 0 } else { 1 };
        assert_eq!(with_hog[hog_bucket], vec![0]);
        assert_eq!(with_hog[1 - hog_bucket].len(), 7);
    }

    #[test]
    fn deterministic_across_calls() {
        let names: Vec<String> = (0..30).map(|i| format!("t{i}")).collect();
        let cache: HashMap<String, f64> = (0..30)
            .map(|i| (format!("t{i}"), (i * 3 % 11) as f64))
            .collect();
        assert_eq!(
            shard_indices(&names, &cache, 2, 5),
            shard_indices(&names, &cache, 2, 5)
        );
    }

    #[test]
    fn single_shard_keeps_all() {
        let names = ids(&["a", "b", "c"]);
        assert_eq!(shard_indices(&names, &HashMap::new(), 1, 1), vec![0, 1, 2]);
    }
}
