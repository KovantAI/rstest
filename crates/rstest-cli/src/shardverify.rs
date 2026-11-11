//! `rstest shard-verify`: prove a sharded run covered the whole suite.
//!
//! `--shard K/N` partitions the suite locally in each CI job with zero
//! coordination, so a divergent duration cache (or a differently-collected
//! suite) across jobs can silently drop or double-run tests and still exit 0.
//! Nothing detects that at runtime.
//!
//! This subcommand reconciles the per-shard `--report-json` files after the
//! matrix finishes. Each report written under `--shard` carries a `meta.shard`
//! block `{k, n, collection_hash, collection_size}`, plus `budget_skipped` when
//! `--budget` left part of the bucket out; the `tests` map is keyed by the
//! nodeids that shard actually ran. A shard's bucket is what it ran plus what
//! its budget skipped. Coverage is complete iff every shard agrees on one
//! collection and the union of the buckets equals that collection with no test
//! in two buckets.

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
    #[serde(default)]
    budget_skipped: Vec<String>,
}

/// One loaded shard report: label (for messages), its shard meta, and its
/// bucket: the nodeids it ran plus those its `--budget` left out.
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
    let mut ran: BTreeSet<String> = doc.tests.into_keys().collect();
    ran.extend(meta.budget_skipped.iter().cloned());
    Ok(Shard { label, meta, ran })
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
        let skipped: usize = shards.iter().map(|s| s.meta.budget_skipped.len()).sum();
        let budget = if skipped > 0 {
            format!("; {skipped} left out by --budget")
        } else {
            String::new()
        };
        sink.out_line(&format!(
            "{} shard-verify: {} shards cover all {} collected tests (no drops, no overlap){budget}",
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
                budget_skipped: Vec::new(),
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
    fn parse_report_counts_budget_skipped_tests_in_the_bucket() {
        let body = br#"{
            "meta": {"shard":{"k":1,"n":2,"collection_hash":"h","collection_size":4,
                              "budget_skipped":["b"]}},
            "tests": {"a": {"call":"passed"}}
        }"#;
        let s1 = parse_report("shard.1.json".into(), body).unwrap();
        assert_eq!(s1.ran, ["a", "b"].iter().map(|s| s.to_string()).collect());
        // Covered, not dropped: the other shard ran the other half.
        let s2 = shard(2, 2, "h", 4, &["c", "d"]);
        assert!(verify(&[s1, s2]).is_empty());
        // A skipped test is still in exactly one bucket: overlap is caught.
        let body = br#"{"meta":{"shard":{"k":1,"n":2,"collection_hash":"h",
            "collection_size":4,"budget_skipped":["c"]}},"tests":{"a":{},"b":{}}}"#;
        let s1 = parse_report("shard.1.json".into(), body).unwrap();
        let errs = verify(&[s1, shard(2, 2, "h", 4, &["c", "d"])]);
        assert!(
            errs.iter().any(|e| e.contains("more than one shard")),
            "{errs:?}"
        );
    }

    #[test]
    fn run_reports_what_the_budget_left_out() {
        let body = br#"{"meta":{"shard":{"k":1,"n":2,"collection_hash":"h",
            "collection_size":2,"budget_skipped":["b"]}},"tests":{}}"#;
        let dir = std::env::temp_dir().join(format!("rstest-sv-budget-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (p1, p2) = (dir.join("1.json"), dir.join("2.json"));
        std::fs::write(&p1, body).unwrap();
        std::fs::write(
            &p2,
            br#"{"meta":{"shard":{"k":2,"n":2,"collection_hash":"h","collection_size":2}},
                "tests":{"a":{}}}"#,
        )
        .unwrap();
        let (mut sink, out) = Sink::captured();
        assert_eq!(run_shard_verify(&mut sink, &[p1, p2]).unwrap(), 0);
        assert!(out.out().contains(
            "cover all 2 collected tests (no drops, no overlap); 1 left out by --budget"
        ));
        std::fs::remove_dir_all(&dir).ok();
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

    #[test]
    fn out_of_range_shards_are_caught() {
        // k=0 and k>n are not valid shard numbers; 1/2 and 2/2 are still
        // missing, since nothing supplied them.
        let s = vec![shard(0, 2, "h", 2, &["a"]), shard(3, 2, "h", 2, &["b"])];
        let mut errs = verify(&s);
        errs.sort();
        assert_eq!(
            errs,
            [
                "missing shard 1/2",
                "missing shard 2/2",
                "shard 0/2 is out of range",
                "shard 3/2 is out of range",
            ]
        );
    }

    #[test]
    fn exact_messages_for_overlap_and_drop() {
        // 4 overlapping ids (only 3 sampled, in sorted order) and one drop.
        let s = vec![
            shard(1, 2, "h", 7, &["a", "b", "c", "d"]),
            shard(2, 2, "h", 7, &["a", "b", "c", "d", "e"]),
        ];
        assert_eq!(
            verify(&s),
            [
                "4 test(s) ran on more than one shard, e.g. a, b, c",
                "shards ran 5 of 7 collected tests; 2 were dropped (ran on no shard)",
            ]
        );
        assert_eq!(sample(std::iter::empty()), "");
    }

    #[test]
    fn each_mismatched_meta_field_flags_divergence() {
        // n, size and hash each independently break agreement.
        for other in [
            shard(2, 3, "h", 4, &["c", "d"]),
            shard(2, 2, "h", 5, &["c", "d"]),
            shard(2, 2, "x", 4, &["c", "d"]),
        ] {
            let errs = verify(&[shard(1, 2, "h", 4, &["a", "b"]), other]);
            assert_eq!(errs.len(), 1, "{errs:?}");
            assert!(
                errs[0].starts_with("shard.2.json: disagrees on the collection")
                    && errs[0].contains("differ from shard.1.json"),
                "{errs:?}"
            );
        }
    }

    fn write_report(dir: &std::path::Path, k: usize, ids: &[&str]) -> PathBuf {
        let tests: serde_json::Map<String, serde_json::Value> = ids
            .iter()
            .map(|id| (id.to_string(), serde_json::json!({"call": "passed"})))
            .collect();
        let doc = serde_json::json!({
            "meta": {"shard": {"k": k, "n": 2, "collection_hash": "h", "collection_size": 3}},
            "tests": tests,
        });
        let p = dir.join(format!("shard{k}.json"));
        std::fs::write(&p, doc.to_string()).unwrap();
        p
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "rstest-shardverify-{tag}-{}-{}",
            std::process::id(),
            crate::time::now_epoch_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn run_shard_verify_reports_complete_coverage_and_exits_zero() {
        let dir = temp_dir("ok");
        let reports = [
            write_report(&dir, 2, &["c"]),
            write_report(&dir, 1, &["a", "b"]),
        ];
        let (mut sink, cap) = Sink::captured();
        let code = run_shard_verify(&mut sink, &reports).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(code, 0);
        assert_eq!(
            cap.out(),
            "ok shard-verify: 2 shards cover all 3 collected tests (no drops, no overlap)\n"
        );
        assert_eq!(cap.err(), "");
    }

    #[test]
    fn run_shard_verify_lists_problems_on_stderr_and_exits_one() {
        let dir = temp_dir("bad");
        let reports = [write_report(&dir, 1, &["a"])];
        let (mut sink, cap) = Sink::captured();
        let code = run_shard_verify(&mut sink, &reports).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(code, 1);
        assert_eq!(cap.out(), "FAILED shard-verify: coverage is INCOMPLETE\n");
        assert_eq!(
            cap.err(),
            "  - missing shard 2/2\n  - shards ran 1 of 3 collected tests; 2 were dropped (ran on no shard)\n"
        );
    }

    #[test]
    fn run_shard_verify_fails_loudly_on_unreadable_or_unstamped_files() {
        let dir = temp_dir("err");
        let missing = dir.join("nope.json");
        let (mut sink, _cap) = Sink::captured();
        let err = format!(
            "{:#}",
            run_shard_verify(&mut sink, std::slice::from_ref(&missing)).unwrap_err()
        );
        assert!(
            err.starts_with(&format!("reading {}", missing.display())),
            "{err}"
        );
        let plain = dir.join("plain.json");
        std::fs::write(&plain, r#"{"tests":{}}"#).unwrap();
        let err = run_shard_verify(&mut sink, std::slice::from_ref(&plain))
            .unwrap_err()
            .to_string();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            err.starts_with(&format!("{}: no shard metadata", plain.display())),
            "{err}"
        );
    }
}
