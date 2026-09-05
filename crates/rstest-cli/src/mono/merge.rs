//! Folding per-project report-json files into one root-relative document.

use std::path::Path;

/// Merge per-project report-json files into one document. Test keys become
/// ROOT-relative nodeids (as pytest would name them from the root);
/// `meta.projects` carries per-project exit/skip status. Additive to schema 2.
pub fn merge_reports(
    parts: &[(String, Option<std::path::PathBuf>, Option<i32>, bool)],
    run_meta: &crate::reporting::report::RunMeta,
    out: &Path,
) -> anyhow::Result<()> {
    let mut tests = serde_json::Map::new();
    let mut collect_errors: Vec<serde_json::Value> = Vec::new();
    let mut projects = serde_json::Map::new();
    let mut totals: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    for (rel, path, status, skipped) in parts {
        let mut entry = serde_json::Map::new();
        if *skipped {
            entry.insert("skipped".into(), true.into());
        } else if let Some(code) = status {
            entry.insert("exitstatus".into(), (*code).into());
        }
        let mut part_doc: Option<serde_json::Value> = None;
        if let Some(path) = path {
            if let Ok(bytes) = std::fs::read(path) {
                part_doc = serde_json::from_slice(&bytes).ok();
            }
        }
        if let Some(doc) = &part_doc {
            if let Some(t) = doc.get("tests").and_then(|t| t.as_object()) {
                for (nodeid, v) in t {
                    tests.insert(format!("{rel}/{nodeid}"), v.clone());
                }
            }
            if let Some(errs) = doc.get("collect_errors").and_then(|e| e.as_array()) {
                for e in errs {
                    let prefixed = e
                        .as_str()
                        .map(|p| serde_json::Value::String(format!("{rel}/{p}")))
                        .unwrap_or_else(|| e.clone());
                    collect_errors.push(prefixed);
                }
            }
            // Per-project counts ride into meta.projects; grand totals
            // aggregate across projects.
            if let Some(counts) = doc
                .get("meta")
                .and_then(|m| m.get("counts"))
                .and_then(|c| c.as_object())
            {
                entry.insert("counts".into(), counts.clone().into());
                for (k, v) in counts {
                    *totals.entry(k.clone()).or_default() += v.as_u64().unwrap_or(0);
                }
            }
        }
        projects.insert(rel.clone(), entry.into());
    }
    let doc = serde_json::json!({
        "meta": {
            "runner": "rstest",
            "schema": 4,
            "exitstatus": run_meta.exitstatus,
            "counts": totals,
            "duration_seconds": (run_meta.duration_seconds * 100.0).round() / 100.0,
            "started_at_epoch": run_meta.started_at_epoch,
            "workers": run_meta.workers,
            "argv": run_meta.argv,
            "projects": projects,
        },
        "collect_errors": collect_errors,
        "tests": tests,
    });
    std::fs::write(out, serde_json::to_vec_pretty(&doc)?)?;
    Ok(())
}

#[cfg(test)]
mod merge_tests {
    use super::*;

    #[test]
    fn merges_with_root_relative_keys() {
        let dir = std::env::temp_dir().join(format!("rstest-merge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.json");
        std::fs::write(
            &a,
            r#"{"meta":{"schema":3,"exitstatus":0,"counts":{"passed":1,"failed":0}},
               "collect_errors":[],
               "tests":{"tests/test_x.py::t1":{"call":"passed"}}}"#,
        )
        .unwrap();
        let b = dir.join("b.json");
        std::fs::write(
            &b,
            r#"{"meta":{"schema":3,"exitstatus":1,"counts":{"passed":0,"failed":1}},
               "collect_errors":["broken.py"],
               "tests":{"tests/test_y.py::t2":{"call":"failed"}}}"#,
        )
        .unwrap();
        let out = dir.join("merged.json");
        merge_reports(
            &[
                ("libs/a".into(), Some(a), Some(0), false),
                ("libs/b".into(), Some(b), Some(1), false),
                ("libs/c".into(), None, None, true), // skipped by --changed
            ],
            &crate::reporting::report::RunMeta {
                exitstatus: 1,
                duration_seconds: 12.345,
                started_at_epoch: 1_750_000_000,
                workers: 4,
                argv: vec!["rstest".into()],
            },
            &out,
        )
        .unwrap();
        let doc: serde_json::Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
        assert_eq!(doc["meta"]["exitstatus"], 1);
        assert_eq!(doc["meta"]["schema"], 4);
        assert_eq!(doc["meta"]["counts"]["passed"], 1);
        assert_eq!(doc["meta"]["counts"]["failed"], 1);
        assert_eq!(doc["meta"]["projects"]["libs/b"]["counts"]["failed"], 1);
        assert_eq!(doc["meta"]["duration_seconds"], 12.35);
        assert_eq!(doc["meta"]["projects"]["libs/c"]["skipped"], true);
        assert_eq!(doc["meta"]["projects"]["libs/b"]["exitstatus"], 1);
        assert_eq!(doc["tests"]["libs/a/tests/test_x.py::t1"]["call"], "passed");
        assert_eq!(doc["tests"]["libs/b/tests/test_y.py::t2"]["call"], "failed");
        assert_eq!(doc["collect_errors"][0], "libs/b/broken.py");
    }
}
