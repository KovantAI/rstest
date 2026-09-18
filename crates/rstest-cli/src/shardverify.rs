//! `rstest shard-verify`: prove a sharded run covered the whole suite.
//!
//! `--shard K/N` partitions the suite locally in each CI job with zero
//! coordination, so a divergent duration cache (or a differently-collected
//! suite) across jobs can silently drop or double-run tests and still exit 0.
//! Nothing detects that at runtime.
//!
//! This subcommand reconciles the per-shard `--report-json` files after the
//! matrix finishes. Each report written under `--shard` carries a `meta.shard`
//! block `{k, n, collection_hash, collection_size}`; the `tests` map is keyed by
//! the nodeids that shard actually ran. Coverage is complete iff every shard
//! agrees on one collection and the union of what they ran equals that
//! collection with no test on two shards.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::reporting::color::Palette;
use crate::reporting::sink::Sink;

#[derive(Deserialize)]
struct ReportDoc {
    #[serde(default)]
    meta: Meta,
    #[serde(default)]
    tests: HashMap<String, serde_json::Value>,
}

#[derive(Deserialize, Default)]
struct Meta {
    shard: Option<ShardMeta>,
}

#[derive(Deserialize, Clone, Debug)]
struct ShardMeta {
    k: usize,
    n: usize,
    collection_hash: String,
    collection_size: u64,
}

/// One loaded shard report: label (for messages), its shard meta, and the set
/// of nodeids it ran.
#[derive(Debug)]
struct Shard {
    label: String,
    meta: ShardMeta,
    ran: BTreeSet<String>,
}

/// Parse one report-json body into a `Shard`. Fails if it is not valid JSON or
/// was not produced by a `--shard` run (no `meta.shard`). Pure (no IO) so the
/// parse + missing-stamp error paths are unit-tested directly.
fn parse_report(label: String, bytes: &[u8]) -> Result<Shard> {
    let doc: ReportDoc = serde_json::from_slice(bytes)
        .with_context(|| format!("parsing {label} as rstest report-json"))?;
    let meta = doc.meta.shard.ok_or_else(|| {
        anyhow::anyhow!(
            "{label}: no shard metadata. Run each shard with `--shard K/N --report-json`; \
             shard-verify needs the full-collection stamp those runs write \
             (lazy-collection shard runs are not covered)"
        )
    })?;
    Ok(Shard {
        label,
        meta,
        ran: doc.tests.into_keys().collect(),
    })
}

/// Read + parse the per-shard reports. Fails loudly if a file is missing,
/// unparseable, or was not produced by a `--shard` run (no `meta.shard`).
fn load(reports: &[PathBuf]) -> Result<Vec<Shard>> {
    let mut shards = Vec::with_capacity(reports.len());
    for path in reports {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        shards.push(parse_report(path.display().to_string(), &bytes)?);
    }
    Ok(shards)
}

/// The pure reconciliation: returns a list of human-readable problems (empty =
/// complete coverage). Split from IO/printing so it is unit-tested directly.
fn verify(shards: &[Shard]) -> Vec<String> {
    let mut errors = Vec::new();
    let first = &shards[0].meta;
    let (ref_hash, ref_n, ref_size) = (&first.collection_hash, first.n, first.collection_size);

    // 1. Every shard must have partitioned the identical collection. A mismatch
    //    here is the divergent-cache / differently-collected failure.
    for s in shards {
        if &s.meta.collection_hash != ref_hash
            || s.meta.n != ref_n
            || s.meta.collection_size != ref_size
        {
            errors.push(format!(
                "{}: disagrees on the collection (its n/size/hash differ from {}). \
                 The shards were built from different test lists or duration caches, \
                 so their partition is not a valid split.",
                s.label, shards[0].label
            ));
        }
    }
    // A divergent collection makes the coverage counts below meaningless.
    if !errors.is_empty() {
        return errors;
    }

    // 2. The shard set must be exactly 1..=n, once each.
    let mut counts: HashMap<usize, usize> = HashMap::new();
    for s in shards {
        *counts.entry(s.meta.k).or_default() += 1;
    }
    for k in 1..=ref_n {
        match counts.get(&k) {
            None => errors.push(format!("missing shard {k}/{ref_n}")),
            Some(&c) if c > 1 => errors.push(format!("shard {k}/{ref_n} supplied {c} times")),
            _ => {}
        }
    }
    for &k in counts.keys() {
        if k == 0 || k > ref_n {
            errors.push(format!("shard {k}/{ref_n} is out of range"));
        }
    }

    // 3. Union of what ran: no test on two shards (overlap), and the union
    //    covers the whole collection (no drop).
    let mut union: BTreeSet<&str> = BTreeSet::new();
    let mut overlap: BTreeSet<&str> = BTreeSet::new();
    for s in shards {
        for id in &s.ran {
            if !union.insert(id) {
                overlap.insert(id);
            }
        }
    }
    if !overlap.is_empty() {
        errors.push(format!(
            "{} test(s) ran on more than one shard{}",
            overlap.len(),
            sample(overlap.iter().copied()),
        ));
    }
    if (union.len() as u64) != ref_size {
        errors.push(format!(
            "shards ran {} of {} collected tests; {} were dropped (ran on no shard)",
            union.len(),
            ref_size,
            ref_size.saturating_sub(union.len() as u64),
        ));
    }
    errors
}

/// Up to three example nodeids, for a problem message.
fn sample<'a>(ids: impl Iterator<Item = &'a str>) -> String {
    let shown: Vec<&str> = ids.take(3).collect();
    if shown.is_empty() {
        String::new()
    } else {
        format!(", e.g. {}", shown.join(", "))
    }
}

pub fn run_shard_verify(sink: &mut Sink, reports: &[PathBuf]) -> Result<i32> {
    let shards = load(reports)?;
    let pal = sink.palette();
    let errors = verify(&shards);
    if errors.is_empty() {
        let m = &shards[0].meta;
        sink.out_line(&format!(
            "{} shard-verify: {} shards cover all {} collected tests (no drops, no overlap)",
            pal.outcome("ok"),
            m.n,
            m.collection_size,
        ));
        Ok(0)
    } else {
        report_failures(sink, pal, &errors);
        Ok(1)
    }
}

fn report_failures(sink: &mut Sink, pal: Palette, errors: &[String]) {
    sink.out_line(&format!(
        "{} shard-verify: coverage is INCOMPLETE",
        pal.outcome("FAILED")
    ));
    for e in errors {
        sink.warn(&format!("  - {e}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shard(k: usize, n: usize, hash: &str, size: u64, ran: &[&str]) -> Shard {
        Shard {
            label: format!("shard.{k}.json"),
            meta: ShardMeta {
                k,
                n,
                collection_hash: hash.into(),
                collection_size: size,
            },
            ran: ran.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn parse_report_reads_shard_stamp_and_ran_ids() {
        let body = br#"{
            "meta": {"runner":"rstest","schema":5,
                     "shard":{"k":1,"n":2,"collection_hash":"abc","collection_size":3}},
            "tests": {"t/a.py::x": {"call":"passed"}, "t/a.py::y": {"setup":"skipped"}}
        }"#;
        let s = parse_report("shard.1.json".into(), body).unwrap();
        assert_eq!((s.meta.k, s.meta.n, s.meta.collection_size), (1, 2, 3));
        assert_eq!(s.meta.collection_hash, "abc");
        assert_eq!(
            s.ran,
            ["t/a.py::x", "t/a.py::y"]
                .iter()
                .map(|s| s.to_string())
                .collect()
        );
    }

    #[test]
    fn parse_report_rejects_a_report_without_shard_meta() {
        // A plain (non-shard) run, or a lazy shard run: no meta.shard stamp.
        let body = br#"{"meta":{"runner":"rstest","schema":5},"tests":{}}"#;
        let err = parse_report("plain.json".into(), body)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no shard metadata"), "{err}");
    }

    #[test]
    fn parse_report_rejects_malformed_json() {
        let err = parse_report("junk.json".into(), b"not json")
            .unwrap_err()
            .to_string();
        assert!(err.contains("parsing junk.json"), "{err}");
    }

    #[test]
    fn complete_coverage_passes() {
        let s = vec![
            shard(1, 2, "h", 4, &["a", "b"]),
            shard(2, 2, "h", 4, &["c", "d"]),
        ];
        assert!(verify(&s).is_empty());
    }

    #[test]
    fn a_dropped_test_is_caught() {
        // 4 collected, but only 3 ran across the shards: one was dropped.
        let s = vec![
            shard(1, 2, "h", 4, &["a", "b"]),
            shard(2, 2, "h", 4, &["c"]),
        ];
        let errs = verify(&s);
        assert!(errs.iter().any(|e| e.contains("dropped")), "{errs:?}");
    }

    #[test]
    fn an_overlapping_test_is_caught() {
        let s = vec![
            shard(1, 2, "h", 3, &["a", "b"]),
            shard(2, 2, "h", 3, &["b", "c"]),
        ];
        let errs = verify(&s);
        assert!(
            errs.iter().any(|e| e.contains("more than one shard")),
            "{errs:?}"
        );
    }

    #[test]
    fn divergent_collection_is_caught_first() {
        // Same test count, but different collection_hash: shards partitioned
        // different suites/caches. Flagged, and coverage counts suppressed.
        let s = vec![
            shard(1, 2, "h1", 4, &["a", "b"]),
            shard(2, 2, "h2", 4, &["c", "d"]),
        ];
        let errs = verify(&s);
        assert!(
            errs.iter()
                .any(|e| e.contains("disagrees on the collection")),
            "{errs:?}"
        );
        assert!(
            !errs.iter().any(|e| e.contains("dropped")),
            "counts suppressed: {errs:?}"
        );
    }

    #[test]
    fn a_missing_shard_is_caught() {
        let s = vec![shard(1, 2, "h", 4, &["a", "b"])];
        let errs = verify(&s);
        assert!(
            errs.iter().any(|e| e.contains("missing shard 2/2")),
            "{errs:?}"
        );
    }

    #[test]
    fn a_duplicated_shard_is_caught() {
        let s = vec![
            shard(1, 2, "h", 4, &["a", "b"]),
            shard(1, 2, "h", 4, &["a", "b"]),
        ];
        let errs = verify(&s);
        assert!(
            errs.iter().any(|e| e.contains("supplied 2 times")),
            "{errs:?}"
        );
    }
}
