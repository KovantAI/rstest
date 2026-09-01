//! Interpreter discovery + validation, uv-style: build an ordered candidate
//! list, probe each (confirm it runs and imports the worker shim), commit to
//! the first that passes. Positive probes are cached on disk keyed by mtime.

mod cache;
mod candidates;
mod probe;
mod request;

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

use candidates::{discovery_candidates, python_version_arg, venv_python};
use probe::{cached_probe, Probe};
use request::{matches, parse_pyarg, PyArg, Request};

/// Minimum interpreter we'll run workers on.
const MIN_VERSION: (u8, u8) = (3, 9);

/// Resolve the interpreter to run workers with. `scope` anchors the upward
/// `.venv` / `.python-version` walk. An explicit `--python` is authoritative:
/// still probed, but never silently replaced by a different interpreter.
pub fn resolve(scope: &Path, explicit: Option<&str>) -> Result<PathBuf> {
    // Explicit --python: its version request filters the pool with no fallback
    // to a mismatching interpreter.
    if let Some(s) = explicit {
        return match parse_pyarg(s) {
            PyArg::Path(p) => resolve_with(&[p], None, cached_probe),
            PyArg::Request(r) => resolve_with(&discovery_candidates(scope), Some(&r), cached_probe),
        };
    }
    // A `.python-version` found up-tree sets the request, but only as a soft
    // pin: an active virtualenv wins over it (see resolve_versioned).
    match python_version_arg(scope) {
        // Concrete path: the one candidate, authoritative.
        Some(PyArg::Path(p)) => resolve_with(&[p], None, cached_probe),
        Some(PyArg::Request(r)) => {
            let active = std::env::var_os("VIRTUAL_ENV").and_then(|v| venv_python(Path::new(&v)));
            resolve_versioned(active, &discovery_candidates(scope), &r, cached_probe)
        }
        // Nothing requested: first usable interpreter in discovery order.
        None => resolve_with(&discovery_candidates(scope), None, cached_probe),
    }
}

/// Resolve an implicit `.python-version` request. The pin is *soft*: a usable
/// active virtualenv wins over it, since a stale pin must not reject the env
/// the user is running in. Only with no usable active venv is the pin a filter.
fn resolve_versioned<F>(
    active_venv: Option<PathBuf>,
    candidates: &[PathBuf],
    req: &Request,
    probe_fn: F,
) -> Result<PathBuf>
where
    F: FnMut(&Path) -> Option<Probe> + Copy,
{
    if let Some(py) = active_venv {
        if let Ok(p) = resolve_with(&[py], None, probe_fn) {
            return Ok(p);
        }
    }
    resolve_with(candidates, Some(req), probe_fn)
}

/// Walk the candidate list, probing each; return the first usable interpreter's
/// canonical executable that also satisfies `request` when one is given (uv's
/// "first-compatible among system interpreters"). `probe_fn` is injected for tests.
fn resolve_with<F>(
    candidates: &[PathBuf],
    request: Option<&Request>,
    mut probe_fn: F,
) -> Result<PathBuf>
where
    F: FnMut(&Path) -> Option<Probe>,
{
    let mut rejected: Vec<String> = Vec::new();
    for cand in candidates {
        match probe_fn(cand) {
            None => rejected.push(format!(
                "  {}: not runnable as a Python interpreter",
                cand.display()
            )),
            Some(p) if (p.version.0, p.version.1) < MIN_VERSION => rejected.push(format!(
                "  {}: Python {}.{}.{} is older than the required {}.{}",
                cand.display(),
                p.version.0,
                p.version.1,
                p.version.2,
                MIN_VERSION.0,
                MIN_VERSION.1,
            )),
            Some(p) if !p.worker_importable => rejected.push(format!(
                "  {}: cannot import the rstest worker shim (is rstest installed in it?)",
                cand.display()
            )),
            Some(p) if request.is_some_and(|r| !matches(&p, r)) => rejected.push(format!(
                "  {}: Python {}.{}.{} ({}) does not satisfy '{}'",
                cand.display(),
                p.version.0,
                p.version.1,
                p.version.2,
                p.implementation,
                request.unwrap(),
            )),
            Some(p) => return Ok(p.executable),
        }
    }
    let hint = match request {
        Some(r) => format!("No interpreter satisfied '{r}'. Tried:"),
        None => "no usable Python interpreter found. Tried:".to_string(),
    };
    bail!(
        "{hint}\n{}\n\nPass one explicitly with --python PATH-OR-VERSION.",
        rejected.join("\n")
    );
}

#[cfg(test)]
mod tests {
    use super::cache::{read_cache, write_cache, CacheEntry, DiskCache};
    #[cfg(not(windows))]
    use super::candidates::{discovery_candidates, managed_in, py_launcher_candidates};
    use super::candidates::{parse_dir_version, parse_py_list_paths, path_python_names};
    use super::probe::{probe, Probe};
    use super::request::{matches, parse_pyarg, PyArg, Request};
    use super::{resolve_versioned, resolve_with};
    use std::path::{Path, PathBuf};

    fn probe_at(executable: &str, version: (u8, u8, u8), worker_importable: bool) -> Probe {
        Probe {
            executable: PathBuf::from(executable),
            version,
            implementation: "cpython".into(),
            freethreaded: false,
            worker_importable,
        }
    }

    fn probe_full(version: (u8, u8, u8), implementation: &str, freethreaded: bool) -> Probe {
        Probe {
            executable: PathBuf::from("/x"),
            version,
            implementation: implementation.into(),
            freethreaded,
            worker_importable: true,
        }
    }

    fn req(s: &str) -> Request {
        match parse_pyarg(s) {
            PyArg::Request(r) => r,
            PyArg::Path(p) => panic!("{s} parsed as path {p:?}, expected a request"),
        }
    }

    #[test]
    fn first_usable_candidate_wins() {
        let cands = [PathBuf::from("bad"), PathBuf::from("good")];
        let chosen = resolve_with(&cands, None, |c| {
            (c == Path::new("good")).then(|| probe_at("/usr/bin/good", (3, 12, 0), true))
        })
        .unwrap();
        assert_eq!(chosen, PathBuf::from("/usr/bin/good"));
    }

    #[test]
    fn returns_canonical_executable_not_candidate_name() {
        let cands = [PathBuf::from("python3")];
        let chosen = resolve_with(&cands, None, |_| {
            Some(probe_at("/opt/py/bin/python3.12", (3, 12, 4), true))
        })
        .unwrap();
        assert_eq!(chosen, PathBuf::from("/opt/py/bin/python3.12"));
    }

    #[test]
    fn too_old_is_rejected_with_reason() {
        let cands = [PathBuf::from("python3")];
        let err =
            resolve_with(&cands, None, |_| Some(probe_at("/x", (3, 7, 0), true))).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("3.7.0"), "{msg}");
        assert!(msg.contains("3.9"), "{msg}");
    }

    #[test]
    fn missing_shim_is_rejected_with_reason() {
        let cands = [PathBuf::from("python3")];
        let err =
            resolve_with(&cands, None, |_| Some(probe_at("/x", (3, 12, 0), false))).unwrap_err();
        assert!(err.to_string().contains("worker shim"), "{err}");
    }

    #[test]
    fn no_candidates_lists_everything_tried() {
        let cands = [PathBuf::from("python3.12"), PathBuf::from("python3")];
        let err = resolve_with(&cands, None, |_| None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("python3.12"), "{msg}");
        assert!(msg.contains("python3"), "{msg}");
        assert!(msg.contains("--python"), "{msg}");
    }

    #[test]
    fn falls_through_old_and_shimless_to_usable() {
        let cands = [
            PathBuf::from("old"),
            PathBuf::from("noshim"),
            PathBuf::from("ok"),
        ];
        let chosen = resolve_with(&cands, None, |c| match c.to_str().unwrap() {
            "old" => Some(probe_at("/old", (3, 8, 0), true)),
            "noshim" => Some(probe_at("/noshim", (3, 12, 0), false)),
            "ok" => Some(probe_at("/ok", (3, 11, 0), true)),
            _ => None,
        })
        .unwrap();
        assert_eq!(chosen, PathBuf::from("/ok"));
    }

    #[cfg(unix)]
    #[test]
    fn venv_walk_finds_ancestor_and_stops_at_repo_root() {
        use std::fs;
        let tmp = std::env::temp_dir().join(format!("rstest-disc-{}", std::process::id()));
        let repo = tmp.join("repo");
        let nested = repo.join("a/b/c");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(repo.join(".venv/bin")).unwrap();
        fs::write(repo.join(".venv/bin/python"), "").unwrap();
        // A .venv above the repo root must NOT be reached.
        fs::create_dir_all(tmp.join(".venv/bin")).unwrap();
        fs::write(tmp.join(".venv/bin/python"), "").unwrap();

        let cands = discovery_candidates(&nested);
        let venv = repo.join(".venv/bin/python");
        let outside = tmp.join(".venv/bin/python");
        assert!(cands.contains(&venv), "expected repo .venv in {cands:?}");
        assert!(
            !cands.contains(&outside),
            "walk leaked past repo root: {cands:?}"
        );

        fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn path_names_are_version_specific_first() {
        let names = path_python_names();
        let generic = names
            .iter()
            .position(|n| n == "python3" || n == "python.exe");
        assert!(generic.is_some());
        // At least one more specific name precedes the generic fallback.
        assert!(generic.unwrap() > 0 || cfg!(windows));
    }

    // ---- T2: version-request grammar ----

    #[test]
    fn parses_bare_and_ranged_versions() {
        assert!(matches(
            &probe_full((3, 12, 4), "cpython", false),
            &req("3.12")
        ));
        assert!(matches(
            &probe_full((3, 12, 0), "cpython", false),
            &req("3")
        ));
        assert!(!matches(
            &probe_full((3, 11, 9), "cpython", false),
            &req("3.12")
        ));
        let range = req(">=3.12,<3.13");
        assert!(matches(&probe_full((3, 12, 7), "cpython", false), &range));
        assert!(!matches(&probe_full((3, 13, 0), "cpython", false), &range));
        assert!(!matches(&probe_full((3, 11, 0), "cpython", false), &range));
    }

    #[test]
    fn exact_micro_must_match() {
        assert!(matches(
            &probe_full((3, 12, 4), "cpython", false),
            &req("==3.12.4")
        ));
        assert!(!matches(
            &probe_full((3, 12, 5), "cpython", false),
            &req("==3.12.4")
        ));
    }

    #[test]
    fn implementation_and_freethreaded_filter() {
        assert!(matches(
            &probe_full((3, 10, 0), "pypy", false),
            &req("pypy@3.10")
        ));
        assert!(!matches(
            &probe_full((3, 10, 0), "cpython", false),
            &req("pypy@3.10")
        ));
        assert!(matches(
            &probe_full((3, 12, 0), "pypy", false),
            &req("pypy")
        ));
        // `3.13t` requires a free-threaded build; a regular 3.13 must not match.
        assert!(matches(
            &probe_full((3, 13, 0), "cpython", true),
            &req("3.13t")
        ));
        assert!(!matches(
            &probe_full((3, 13, 0), "cpython", false),
            &req("3.13t")
        ));
        // A plain request tolerates either build.
        assert!(matches(
            &probe_full((3, 13, 0), "cpython", true),
            &req("3.13")
        ));
    }

    #[test]
    fn non_version_strings_are_paths_not_requests() {
        assert_eq!(
            parse_pyarg("/usr/bin/python3"),
            PyArg::Path("/usr/bin/python3".into())
        );
        assert_eq!(
            parse_pyarg("./my-python"),
            PyArg::Path("./my-python".into())
        );
    }

    #[test]
    fn request_selects_first_compatible_in_order() {
        let cands = [PathBuf::from("a"), PathBuf::from("b"), PathBuf::from("c")];
        let want = req(">=3.12");
        let chosen = resolve_with(&cands, Some(&want), |c| match c.to_str().unwrap() {
            "a" => Some(probe_at("/a", (3, 11, 0), true)), // too old for request
            "b" => Some(probe_at("/b", (3, 12, 5), true)), // first match
            "c" => Some(probe_at("/c", (3, 13, 0), true)),
            _ => None,
        })
        .unwrap();
        assert_eq!(chosen, PathBuf::from("/b"));
    }

    #[test]
    fn unsatisfiable_request_reports_spec_and_mismatches() {
        let cands = [PathBuf::from("a")];
        let want = req(">=3.13");
        let err = resolve_with(&cands, Some(&want), |_| {
            Some(probe_at("/a", (3, 11, 0), true))
        })
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains(">=3.13"), "{msg}");
        assert!(msg.contains("does not satisfy"), "{msg}");
    }

    // ---- T3: uv-managed interpreters + disk cache ----

    #[test]
    fn parses_managed_dir_versions() {
        assert_eq!(
            parse_dir_version("cpython-3.12.4-macos-aarch64-none"),
            (3, 12, 4)
        );
        assert_eq!(
            parse_dir_version("cpython-3.13.0+freethreaded-linux-x86_64-gnu"),
            (3, 13, 0)
        );
        assert_eq!(
            parse_dir_version("pypy-3.10-macos-aarch64-none"),
            (3, 10, 0)
        );
        assert_eq!(parse_dir_version("garbage"), (0, 0, 0));
    }

    #[cfg(unix)]
    #[test]
    fn managed_installs_sorted_newest_first() {
        use std::fs;
        let root = std::env::temp_dir().join(format!("rstest-managed-{}", std::process::id()));
        for name in ["cpython-3.11.9-x", "cpython-3.13.1-x", "cpython-3.12.4-x"] {
            let bin = root.join(name).join("bin");
            fs::create_dir_all(&bin).unwrap();
            fs::write(bin.join("python3"), "").unwrap();
        }
        // A dir without an interpreter is ignored.
        fs::create_dir_all(root.join("cpython-9.9.9-empty")).unwrap();

        let found = managed_in(&root);
        let versions: Vec<&str> = found
            .iter()
            .map(|p| {
                p.parent()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
            })
            .collect();
        assert_eq!(
            versions,
            ["cpython-3.13.1-x", "cpython-3.12.4-x", "cpython-3.11.9-x"]
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn disk_cache_roundtrips_and_gates_on_mtime_and_size() {
        let dir = std::env::temp_dir().join(format!("rstest-cache-{}", std::process::id()));
        let path = dir.join("probes.json");
        let mut cache = DiskCache::default();
        cache.entries.insert(
            "/opt/py/bin/python3".into(),
            CacheEntry {
                mtime: 1000,
                size: 4096,
                probe: probe_at("/opt/py/bin/python3", (3, 12, 4), true),
            },
        );
        write_cache(&path, &cache).unwrap();

        let loaded = read_cache(&std::fs::read(&path).unwrap()).unwrap();
        let e = loaded.entries.get("/opt/py/bin/python3").unwrap();
        assert_eq!(e.mtime, 1000);
        assert_eq!(e.size, 4096);
        assert_eq!(e.probe.version, (3, 12, 4));
        // A hit requires BOTH mtime and size to match: a same-mtime binary swap
        // (different size) must miss, and so must a changed mtime.
        assert!(e.mtime == 1000 && e.size == 4096);
        assert!(
            !(e.mtime == 1000 && e.size == 8192),
            "size change must miss"
        );
        assert!(
            !(e.mtime == 1001 && e.size == 4096),
            "mtime change must miss"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn old_v1_entry_without_size_loads_and_misses() {
        // A pre-size cache file (no `size` field) must still parse, load as
        // size 0, and therefore never match a real interpreter's size.
        let json = br#"{"entries":{"/opt/py/bin/python3":{"mtime":1000,"probe":{"executable":"/opt/py/bin/python3","version":[3,12,4],"implementation":"cpython","freethreaded":false,"worker_importable":true}}}}"#;
        let loaded = read_cache(json).expect("old v1 file must still parse");
        let e = loaded.entries.get("/opt/py/bin/python3").unwrap();
        assert_eq!(e.mtime, 1000);
        assert_eq!(e.size, 0, "missing size defaults to 0 -> guaranteed miss");
    }

    #[test]
    fn corrupt_cache_file_is_ignored() {
        assert!(read_cache(b"not json at all").is_none());
    }

    #[test]
    fn active_venv_wins_over_python_version_pin() {
        // `.python-version` pins 3.10 but the active venv is 3.13: the venv
        // must win (the flags-contradict bug). Candidate pool would otherwise
        // reject the venv for not satisfying the pin.
        let venv = PathBuf::from("/venv/bin/python");
        let cands = [venv.clone(), PathBuf::from("python3.10")];
        let chosen = resolve_versioned(Some(venv), &cands, &req("3.10"), |c| {
            match c.to_str().unwrap() {
                "/venv/bin/python" => Some(probe_at("/venv/bin/python", (3, 13, 13), true)),
                "python3.10" => Some(probe_at("/usr/bin/python3.10", (3, 10, 0), true)),
                _ => None,
            }
        })
        .unwrap();
        assert_eq!(chosen, PathBuf::from("/venv/bin/python"));
    }

    #[test]
    fn unusable_active_venv_falls_back_to_pin() {
        // Active venv lacks the worker shim → not usable → honor the pin and
        // pick the discovered interpreter that satisfies it.
        let venv = PathBuf::from("/venv/bin/python");
        let cands = [venv.clone(), PathBuf::from("python3.10")];
        let chosen = resolve_versioned(Some(venv), &cands, &req("3.10"), |c| {
            match c.to_str().unwrap() {
                "/venv/bin/python" => Some(probe_at("/venv/bin/python", (3, 13, 13), false)),
                "python3.10" => Some(probe_at("/usr/bin/python3.10", (3, 10, 0), true)),
                _ => None,
            }
        })
        .unwrap();
        assert_eq!(chosen, PathBuf::from("/usr/bin/python3.10"));
    }

    #[test]
    fn no_active_venv_honors_pin() {
        // Without an active venv the pin filters the pool as before.
        let cands = [PathBuf::from("python3.13"), PathBuf::from("python3.10")];
        let chosen = resolve_versioned(None, &cands, &req("3.10"), |c| match c.to_str().unwrap() {
            "python3.13" => Some(probe_at("/usr/bin/python3.13", (3, 13, 0), true)),
            "python3.10" => Some(probe_at("/usr/bin/python3.10", (3, 10, 0), true)),
            _ => None,
        })
        .unwrap();
        assert_eq!(chosen, PathBuf::from("/usr/bin/python3.10"));
    }

    // ---- py launcher (`py --list-paths`) ----

    #[test]
    fn parses_py_list_paths_legacy_trailing_active_marker() {
        // Real legacy `py --list-paths`: `-N.M-64` tags, default marked by a
        // *trailing* `*`. Regression guard: the default install must not keep
        // a ` *` suffix (which would fail its later `exists()` check).
        let out = "\
 -3.13-64         C:\\Users\\me\\AppData\\Local\\Programs\\Python\\Python313\\python.exe *
 -3.12-64         C:\\Program Files\\Python312\\python.exe
 -3.9-32          C:\\Python39-32\\python.exe";
        let got = parse_py_list_paths(out);
        assert_eq!(
            got,
            vec![
                // Trailing `*` stripped, not folded into the path.
                PathBuf::from(
                    "C:\\Users\\me\\AppData\\Local\\Programs\\Python\\Python313\\python.exe"
                ),
                // A path containing a space survives intact.
                PathBuf::from("C:\\Program Files\\Python312\\python.exe"),
                PathBuf::from("C:\\Python39-32\\python.exe"),
            ]
        );
    }

    #[test]
    fn parses_py_list_paths_newer_leading_active_marker() {
        // Newer `py list`: `-V:` tags, default marked by a *leading* `*`.
        let out = "\
 -V:3.13          C:\\Program Files\\Python313\\python.exe
 -V:3.12 *        C:\\Users\\me\\AppData\\Local\\Programs\\Python\\Python312\\python.exe";
        let got = parse_py_list_paths(out);
        assert_eq!(
            got,
            vec![
                PathBuf::from("C:\\Program Files\\Python313\\python.exe"),
                // Leading `*` stripped, not folded into the path.
                PathBuf::from(
                    "C:\\Users\\me\\AppData\\Local\\Programs\\Python\\Python312\\python.exe"
                ),
            ]
        );
    }

    #[test]
    fn parses_py_list_paths_skips_non_tag_lines() {
        // Headers / blank lines (no leading `-`) are ignored.
        let out =
            "Installed Pythons found by py Launcher\n\n -3.10-64  C:\\Python310\\python.exe\n";
        assert_eq!(
            parse_py_list_paths(out),
            vec![PathBuf::from("C:\\Python310\\python.exe")]
        );
    }

    #[test]
    fn parses_py_list_paths_empty_when_no_installs() {
        assert!(parse_py_list_paths("").is_empty());
        assert!(parse_py_list_paths("No installed Pythons found!\n").is_empty());
    }

    #[cfg(not(windows))]
    #[test]
    fn py_launcher_is_noop_off_windows() {
        assert!(py_launcher_candidates().is_empty());
    }

    /// End-to-end probe-script + JSON shape, exercised against whatever
    /// `python3` is on PATH. Skips cleanly when none is available.
    #[test]
    fn probe_script_runs_against_real_python() {
        let Some(p) = probe(Path::new("python3")) else {
            return; // no python3 here; nothing to assert
        };
        assert_eq!(p.implementation, "cpython");
        assert!((p.version.0, p.version.1) >= (3, 0));
        // worker_importable depends on the shim being on PYTHONPATH; we only
        // assert the field deserialized, which reaching here proves.
    }
}
