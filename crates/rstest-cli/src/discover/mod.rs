//! Interpreter discovery + validation, uv-style: build an ordered candidate
//! list, probe each (confirm it runs and imports the worker shim), commit to
//! the first that passes. Positive probes are cached on disk keyed by mtime.

mod cache;
mod candidates;
mod probe;
mod request;

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use anyhow::{bail, Result};

/// Lock a best-effort cache mutex, recovering from poisoning instead of
/// panicking. These mutexes guard only in-memory cache maps; if a thread
/// panicked while holding one, the map may be stale but is never unsafe, and
/// the discovery caches must never turn a poisoned lock into a run-killing
/// panic (the "never fail the run on cache IO" contract).
pub(super) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

use candidates::{discovery_candidates, python_version_arg, venv_candidates, venv_python};
use probe::{cached_probe, Probe};
use request::{matches, parse_pyarg, PyArg, Request};

/// Minimum interpreter we'll run workers on.
const MIN_VERSION: (u8, u8) = (3, 9);

/// Resolve the interpreter to run workers with. `scope` anchors the upward
/// `.venv` / `.python-version` walk. An explicit `--python` is authoritative:
/// still probed, but never silently replaced by a different interpreter.
/// Non-fatal discovery notes (a skipped venv, an overridden pin) go to stderr.
pub fn resolve(scope: &Path, explicit: Option<&str>) -> Result<PathBuf> {
    let resolved = resolve_policy(scope, explicit)?;
    for w in &resolved.warnings {
        eprintln!("rstest: warning: {w}");
    }
    Ok(resolved.executable)
}

/// The project virtualenv the run did NOT use: the nearest `.venv` found
/// walking up from `scope` (as discovery does) when `used` is some other
/// interpreter, e.g. pre-commit's or tox's `$VIRTUAL_ENV` won discovery.
/// Returns the venv directory and whether `used` came from `$VIRTUAL_ENV`.
/// Venv roots are compared, not interpreter files: two venvs' `bin/python`
/// usually resolve to the same base interpreter through symlinks.
pub fn skipped_project_venv(scope: &Path, used: &Path) -> Option<(PathBuf, bool)> {
    let root_of = |python: &Path| {
        let root = python.parent()?.parent()?;
        Some(root.canonicalize().unwrap_or_else(|_| root.to_path_buf()))
    };
    let used_root = root_of(used);
    let from_virtual_env = std::env::var_os("VIRTUAL_ENV")
        .and_then(|v| venv_python(Path::new(&v)))
        .is_some_and(|p| p == used);
    for dir in scope.ancestors() {
        let venv = dir.join(".venv");
        if let Some(p) = venv_python(&venv) {
            if root_of(&p) != used_root {
                return Some((venv, from_virtual_env));
            }
            return None;
        }
        if dir.join(".git").exists() {
            break;
        }
    }
    None
}

fn resolve_policy(scope: &Path, explicit: Option<&str>) -> Result<Resolved> {
    // Explicit --python: its version request filters the pool with no fallback
    // to a mismatching interpreter, and the user's choice is never second-guessed
    // by the venv guard below.
    if let Some(s) = explicit {
        return match parse_pyarg(s) {
            PyArg::Path(p) => resolve_with(
                &[interpreter_path(p)],
                &Policy::explicit(None),
                cached_probe,
            ),
            PyArg::Request(r) => resolve_with(
                &discovery_candidates(scope),
                &Policy::explicit(Some(&r)),
                cached_probe,
            ),
        };
    }
    let venvs = venv_candidates(scope);
    match python_version_arg(scope) {
        // Concrete path in `.python-version`: the one candidate, authoritative.
        Some((PyArg::Path(p), file)) => resolve_with(
            &[interpreter_path(p)],
            &Policy {
                origin: Origin::PinFile(&file),
                ..Policy::explicit(None)
            },
            cached_probe,
        ),
        // A version in `.python-version` is a *soft* pin: it filters system
        // interpreters, but a usable virtualenv (active `$VIRTUAL_ENV` or the
        // project's `.venv`) wins over it. The venv holds the project's deps,
        // and a stale pin (the venv was rebuilt on a newer Python, the file
        // wasn't touched) must not reject the environment the project uses.
        Some((PyArg::Request(r), file)) => resolve_with(
            &discovery_candidates(scope),
            &Policy {
                request: Some(&r),
                venvs: &venvs,
                soft_pin: true,
                guard_venv: true,
                origin: Origin::PinFile(&file),
            },
            cached_probe,
        ),
        // Nothing requested: first usable interpreter in discovery order.
        None => resolve_with(
            &discovery_candidates(scope),
            &Policy {
                venvs: &venvs,
                guard_venv: true,
                origin: Origin::Discovery,
                ..Policy::explicit(None)
            },
            cached_probe,
        ),
    }
}

/// A `--python` / `.python-version` path naming a virtualenv directory (as uv
/// accepts, e.g. `--python .venv`) means that venv's interpreter.
fn interpreter_path(p: PathBuf) -> PathBuf {
    if p.is_dir() {
        venv_python(&p).unwrap_or(p)
    } else {
        p
    }
}

/// Where the interpreter request came from; shapes the error text.
#[derive(Clone, Copy)]
enum Origin<'a> {
    /// `--python` on the command line.
    Explicit,
    /// A `.python-version` file (the path, so errors can name it).
    PinFile(&'a Path),
    /// Plain discovery, nothing requested.
    Discovery,
}

/// How [`resolve_with`] treats the candidate list.
struct Policy<'a> {
    request: Option<&'a Request>,
    /// The candidates that are virtualenv interpreters (`$VIRTUAL_ENV`, an
    /// up-tree `.venv`).
    venvs: &'a [PathBuf],
    /// `request` is a soft `.python-version` pin: venv candidates are exempt.
    soft_pin: bool,
    /// Refuse to fall past a venv that lacks rstest to a non-venv interpreter
    /// (see [`resolve_with`]). Off for an explicit `--python`.
    guard_venv: bool,
    origin: Origin<'a>,
}

impl<'a> Policy<'a> {
    fn explicit(request: Option<&'a Request>) -> Self {
        Policy {
            request,
            venvs: &[],
            soft_pin: false,
            guard_venv: false,
            origin: Origin::Explicit,
        }
    }
}

/// The chosen interpreter plus stderr notes about how it was chosen.
#[derive(Debug)]
struct Resolved {
    executable: PathBuf,
    warnings: Vec<String>,
}

/// Walk the candidate list, probing each; return the first usable interpreter's
/// canonical executable that also satisfies the request when one is given (uv's
/// "first-compatible among system interpreters"). `probe_fn` is injected for tests.
///
/// The venv guard (E1): when a virtualenv candidate runs but can't import the
/// worker shim (rstest was never installed into it) and the next usable
/// interpreter is *not* a venv (a PATH or uv-managed Python), stop with an error
/// naming the venv instead of falling through. The project's dependencies live
/// in that venv, so the fall-through run would fail every test that imports
/// them with a bare `ModuleNotFoundError` that never mentions the venv. The
/// cost is the rare user who deliberately runs a global rstest against a
/// project whose `.venv` doesn't need it; the error tells them to pass
/// `--python` for that, which is one flag. Falling from one venv to another
/// (an unrelated `$VIRTUAL_ENV` without rstest, then the project's `.venv`) is
/// allowed with a warning, since the second venv is where the deps live.
fn resolve_with<F>(candidates: &[PathBuf], policy: &Policy, mut probe_fn: F) -> Result<Resolved>
where
    F: FnMut(&Path) -> Option<Probe>,
{
    let mut rejected: Vec<String> = Vec::new();
    // Venv candidates skipped so far, with the reason, and the first one that
    // was skipped only for lacking rstest (the guard's trigger).
    let mut skipped_venvs: Vec<(PathBuf, String)> = Vec::new();
    let mut shimless_venv: Option<&PathBuf> = None;
    for cand in candidates {
        let is_venv = policy.venvs.contains(cand);
        let reason = match probe_fn(cand) {
            None => unrunnable_reason(cand).to_string(),
            Some(p) if (p.version.0, p.version.1) < MIN_VERSION => format!(
                "Python {}.{}.{} is older than the required {}.{}",
                p.version.0, p.version.1, p.version.2, MIN_VERSION.0, MIN_VERSION.1,
            ),
            Some(p) if !p.worker_importable => {
                if is_venv && shimless_venv.is_none() {
                    shimless_venv = Some(cand);
                }
                "cannot import the rstest worker shim (is rstest installed in it?)".to_string()
            }
            Some(p) => match policy.request {
                Some(r) if !matches(&p, r) && !(policy.soft_pin && is_venv) => format!(
                    "Python {}.{}.{} ({}) does not satisfy '{}'",
                    p.version.0, p.version.1, p.version.2, p.implementation, r,
                ),
                _ => return accept(cand, p, is_venv, policy, shimless_venv, skipped_venvs),
            },
        };
        if is_venv {
            skipped_venvs.push((cand.clone(), reason.clone()));
        }
        rejected.push(format!("  {}: {reason}", cand.display()));
    }
    let (hint, trailer) = match (policy.origin, policy.request) {
        (Origin::PinFile(f), Some(r)) => (
            format!(
                "No interpreter satisfied '{r}' (pinned by {}). Tried:",
                f.display()
            ),
            format!(
                "\n\nUpdate or remove {}, or pass --python PATH-OR-VERSION.",
                f.display()
            ),
        ),
        (Origin::PinFile(f), None) => (
            format!(
                "no usable Python interpreter at the path named in {}:",
                f.display()
            ),
            format!(
                "\n\nUpdate or remove {}, or pass --python PATH-OR-VERSION.",
                f.display()
            ),
        ),
        // The user already passed --python: don't tell them to pass it.
        (Origin::Explicit, Some(r)) => (
            format!("No interpreter satisfied '{r}'. Tried:"),
            String::new(),
        ),
        (Origin::Explicit, None) => (
            "no usable Python interpreter at the --python path:".to_string(),
            String::new(),
        ),
        (Origin::Discovery, _) => (
            "no usable Python interpreter found. Tried:".to_string(),
            "\n\nPass one explicitly with --python PATH-OR-VERSION.".to_string(),
        ),
    };
    bail!("{hint}\n{}{trailer}", rejected.join("\n"));
}

/// Commit to a usable candidate, or refuse per the venv guard.
fn accept(
    cand: &Path,
    p: Probe,
    is_venv: bool,
    policy: &Policy,
    shimless_venv: Option<&PathBuf>,
    skipped_venvs: Vec<(PathBuf, String)>,
) -> Result<Resolved> {
    if let (true, false, Some(venv)) = (policy.guard_venv, is_venv, shimless_venv) {
        let py = venv.display();
        bail!(
            "found {py} but rstest is not installed in it (it cannot import the rstest worker \
             shim).\nThat environment holds your project's dependencies, so rstest will not \
             silently run your tests with {} instead.\n\nInstall rstest into it:\n    \
             uv pip install --python {py} rstest\n    \
             {py} -m pip install rstest\n\nTo use a different interpreter on purpose, pass \
             --python (e.g. --python {}).",
            p.executable.display(),
            p.executable.display(),
        );
    }
    let mut warnings: Vec<String> = skipped_venvs
        .iter()
        .map(|(v, why)| {
            format!(
                "skipped {}: {why}; using {}",
                v.display(),
                p.executable.display()
            )
        })
        .collect();
    if let (true, Some(r), Origin::PinFile(f)) =
        (is_venv && policy.soft_pin, policy.request, policy.origin)
    {
        if !matches(&p, r) {
            warnings.push(format!(
                "{} pins '{r}' but {} is Python {}.{}.{}; using the virtualenv \
                 (update the pin, or pass --python {r} to enforce it)",
                f.display(),
                cand.display(),
                p.version.0,
                p.version.1,
                p.version.2,
            ));
        }
    }
    Ok(Resolved {
        executable: p.executable,
        warnings,
    })
}

/// Why a candidate the probe couldn't run was rejected: missing vs present
/// but not a working Python.
fn unrunnable_reason(cand: &Path) -> &'static str {
    if cand.is_dir() {
        return "a directory with no bin/python or Scripts/python.exe in it";
    }
    let bare_name = cand.components().count() == 1 && !cand.exists();
    if bare_name {
        if on_path(cand) {
            "not runnable as a Python interpreter"
        } else {
            "not found on PATH"
        }
    } else if !cand.exists() {
        "no such file"
    } else {
        "not runnable as a Python interpreter"
    }
}

/// Whether a bare command name resolves to a file on PATH.
fn on_path(name: &Path) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let p = dir.join(name);
        p.is_file() || (cfg!(windows) && dir.join(format!("{}.exe", name.display())).is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::cache::{read_cache, write_cache, CacheEntry, DiskCache};
    #[cfg(not(windows))]
    use super::candidates::{discovery_candidates, managed_in, py_launcher_candidates};
    use super::candidates::{parse_dir_version, parse_py_list_paths, path_python_names};
    use super::probe::{probe, Probe};
    use super::request::{matches, parse_pyarg, PyArg, Request};
    use super::{resolve_with, Origin, Policy, Resolved};
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

    /// Plain discovery with no venvs (the guard has nothing to trigger on).
    fn discovery() -> Policy<'static> {
        Policy {
            origin: Origin::Discovery,
            ..Policy::explicit(None)
        }
    }

    /// [`resolve_with`], keeping just the chosen executable.
    fn run(
        cands: &[PathBuf],
        policy: &Policy,
        f: impl FnMut(&Path) -> Option<Probe>,
    ) -> anyhow::Result<PathBuf> {
        resolve_with(cands, policy, f).map(|r: Resolved| r.executable)
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
        let chosen = run(&cands, &discovery(), |c| {
            (c == Path::new("good")).then(|| probe_at("/usr/bin/good", (3, 12, 0), true))
        })
        .unwrap();
        assert_eq!(chosen, PathBuf::from("/usr/bin/good"));
    }

    #[test]
    fn returns_canonical_executable_not_candidate_name() {
        let cands = [PathBuf::from("python3")];
        let chosen = run(&cands, &discovery(), |_| {
            Some(probe_at("/opt/py/bin/python3.12", (3, 12, 4), true))
        })
        .unwrap();
        assert_eq!(chosen, PathBuf::from("/opt/py/bin/python3.12"));
    }

    #[test]
    fn too_old_is_rejected_with_reason() {
        let cands = [PathBuf::from("python3")];
        let err = run(&cands, &discovery(), |_| {
            Some(probe_at("/x", (3, 7, 0), true))
        })
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("3.7.0"), "{msg}");
        assert!(msg.contains("3.9"), "{msg}");
    }

    #[test]
    fn missing_shim_is_rejected_with_reason() {
        let cands = [PathBuf::from("python3")];
        let err = run(&cands, &discovery(), |_| {
            Some(probe_at("/x", (3, 12, 0), false))
        })
        .unwrap_err();
        assert!(err.to_string().contains("worker shim"), "{err}");
    }

    #[test]
    fn no_candidates_lists_everything_tried() {
        let cands = [PathBuf::from("python3.12"), PathBuf::from("python3")];
        let err = run(&cands, &discovery(), |_| None).unwrap_err();
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
        let chosen = run(&cands, &discovery(), |c| match c.to_str().unwrap() {
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
        // Bare command names resolve on PATH, not as an implementation.
        assert_eq!(parse_pyarg("python3"), PyArg::Path("python3".into()));
        assert_eq!(parse_pyarg("python3.12"), PyArg::Path("python3.12".into()));
    }

    #[test]
    fn request_selects_first_compatible_in_order() {
        let cands = [PathBuf::from("a"), PathBuf::from("b"), PathBuf::from("c")];
        let want = req(">=3.12");
        let chosen = run(&cands, &Policy::explicit(Some(&want)), |c| {
            match c.to_str().unwrap() {
                "a" => Some(probe_at("/a", (3, 11, 0), true)), // too old for request
                "b" => Some(probe_at("/b", (3, 12, 5), true)), // first match
                "c" => Some(probe_at("/c", (3, 13, 0), true)),
                _ => None,
            }
        })
        .unwrap();
        assert_eq!(chosen, PathBuf::from("/b"));
    }

    #[test]
    fn unsatisfiable_request_reports_spec_and_mismatches() {
        let cands = [PathBuf::from("a")];
        let want = req(">=3.13");
        let err = run(&cands, &Policy::explicit(Some(&want)), |_| {
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

    /// Soft `.python-version` pin policy, as `resolve` builds it.
    fn pinned<'a>(venvs: &'a [PathBuf], r: &'a Request, file: &'a Path) -> Policy<'a> {
        Policy {
            request: Some(r),
            venvs,
            soft_pin: true,
            guard_venv: true,
            origin: Origin::PinFile(file),
        }
    }

    /// Plain discovery over `venvs` + system interpreters, as `resolve` builds it.
    fn discovering(venvs: &[PathBuf]) -> Policy<'_> {
        Policy {
            venvs,
            guard_venv: true,
            ..discovery()
        }
    }

    const PIN: &str = "/proj/.python-version";

    #[test]
    fn active_venv_wins_over_python_version_pin() {
        // `.python-version` pins 3.10 but the active venv is 3.13: the venv
        // must win (the flags-contradict bug). Candidate pool would otherwise
        // reject the venv for not satisfying the pin.
        let venv = PathBuf::from("/venv/bin/python");
        let cands = [venv.clone(), PathBuf::from("python3.10")];
        let venvs = [venv];
        let r = req("3.10");
        let got = resolve_with(&cands, &pinned(&venvs, &r, Path::new(PIN)), |c| {
            match c.to_str().unwrap() {
                "/venv/bin/python" => Some(probe_at("/venv/bin/python", (3, 13, 13), true)),
                "python3.10" => Some(probe_at("/usr/bin/python3.10", (3, 10, 0), true)),
                _ => None,
            }
        })
        .unwrap();
        assert_eq!(got.executable, PathBuf::from("/venv/bin/python"));
        // The overridden pin is reported, naming its file.
        assert!(
            got.warnings.iter().any(|w| w.contains(PIN)),
            "{:?}",
            got.warnings
        );
    }

    #[test]
    fn project_dotvenv_wins_over_stale_pin() {
        // E2: no active venv, the discovered project `.venv` is 3.14 with
        // rstest, `.python-version` says 3.13. The project's own env wins.
        let venv = PathBuf::from("/proj/.venv/bin/python");
        let cands = [venv.clone(), PathBuf::from("python3.13")];
        let venvs = [venv];
        let r = req("3.13");
        let got = resolve_with(&cands, &pinned(&venvs, &r, Path::new(PIN)), |c| {
            match c.to_str().unwrap() {
                "/proj/.venv/bin/python" => {
                    Some(probe_at("/proj/.venv/bin/python", (3, 14, 0), true))
                }
                "python3.13" => Some(probe_at("/usr/bin/python3.13", (3, 13, 0), true)),
                _ => None,
            }
        })
        .unwrap();
        assert_eq!(got.executable, PathBuf::from("/proj/.venv/bin/python"));
    }

    #[test]
    fn matching_pin_on_venv_is_silent() {
        let venv = PathBuf::from("/proj/.venv/bin/python");
        let cands = [venv.clone()];
        let venvs = [venv];
        let r = req("3.13");
        let got = resolve_with(&cands, &pinned(&venvs, &r, Path::new(PIN)), |_| {
            Some(probe_at("/proj/.venv/bin/python", (3, 13, 2), true))
        })
        .unwrap();
        assert!(got.warnings.is_empty(), "{:?}", got.warnings);
    }

    #[test]
    fn unsatisfiable_pin_names_its_file() {
        // E2: with no venv to exempt, an unsatisfiable pin's error says where
        // the pin came from.
        let cands = [PathBuf::from("python3.12")];
        let r = req("3.11");
        let err = resolve_with(&cands, &pinned(&[], &r, Path::new(PIN)), |_| {
            Some(probe_at("/usr/bin/python3.12", (3, 12, 0), true))
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains(PIN), "{err}");
        assert!(err.contains("'3.11'"), "{err}");
    }

    #[test]
    fn shimless_venv_stops_instead_of_falling_to_system_python() {
        // E1: the project `.venv` runs but lacks rstest; a PATH interpreter
        // has it. Refuse, naming the venv and how to fix it.
        let venv = PathBuf::from("/proj/.venv/bin/python");
        let cands = [venv.clone(), PathBuf::from("python3")];
        let venvs = [venv];
        let err = resolve_with(&cands, &discovering(&venvs), |c| {
            match c.to_str().unwrap() {
                "/proj/.venv/bin/python" => {
                    Some(probe_at("/proj/.venv/bin/python", (3, 12, 0), false))
                }
                "python3" => Some(probe_at("/usr/bin/python3", (3, 12, 0), true)),
                _ => None,
            }
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("/proj/.venv/bin/python"), "{err}");
        assert!(err.contains("pip install"), "{err}");
        assert!(err.contains("--python"), "{err}");
    }

    #[test]
    fn shimless_venv_guard_also_applies_under_a_pin() {
        let venv = PathBuf::from("/proj/.venv/bin/python");
        let cands = [venv.clone(), PathBuf::from("python3.10")];
        let venvs = [venv];
        let r = req("3.10");
        let err = resolve_with(&cands, &pinned(&venvs, &r, Path::new(PIN)), |c| {
            match c.to_str().unwrap() {
                "/proj/.venv/bin/python" => {
                    Some(probe_at("/proj/.venv/bin/python", (3, 13, 13), false))
                }
                "python3.10" => Some(probe_at("/usr/bin/python3.10", (3, 10, 0), true)),
                _ => None,
            }
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("/proj/.venv/bin/python"), "{err}");
    }

    #[test]
    fn shimless_active_venv_falls_to_project_venv_with_warning() {
        // An unrelated `$VIRTUAL_ENV` without rstest, then the project's
        // `.venv` with it: venv-to-venv fall-through is fine, but say so.
        let active = PathBuf::from("/other/bin/python");
        let proj = PathBuf::from("/proj/.venv/bin/python");
        let cands = [active.clone(), proj.clone(), PathBuf::from("python3")];
        let venvs = [active, proj];
        let got = resolve_with(&cands, &discovering(&venvs), |c| {
            match c.to_str().unwrap() {
                "/other/bin/python" => Some(probe_at("/other/bin/python", (3, 12, 0), false)),
                "/proj/.venv/bin/python" => {
                    Some(probe_at("/proj/.venv/bin/python", (3, 12, 0), true))
                }
                _ => None,
            }
        })
        .unwrap();
        assert_eq!(got.executable, PathBuf::from("/proj/.venv/bin/python"));
        assert!(
            got.warnings.iter().any(|w| w.contains("/other/bin/python")),
            "{:?}",
            got.warnings
        );
    }

    #[test]
    fn broken_venv_falls_through_with_warning() {
        // A venv whose interpreter no longer runs (e.g. its base Python was
        // upgraded away) is not a "rstest missing" case: fall through, but
        // name the skipped venv.
        let venv = PathBuf::from("/proj/.venv/bin/python");
        let cands = [venv.clone(), PathBuf::from("python3")];
        let venvs = [venv];
        let got = resolve_with(&cands, &discovering(&venvs), |c| {
            match c.to_str().unwrap() {
                "python3" => Some(probe_at("/usr/bin/python3", (3, 12, 0), true)),
                _ => None,
            }
        })
        .unwrap();
        assert_eq!(got.executable, PathBuf::from("/usr/bin/python3"));
        assert!(
            got.warnings
                .iter()
                .any(|w| w.contains("/proj/.venv/bin/python")),
            "{:?}",
            got.warnings
        );
    }

    #[test]
    fn explicit_python_skips_the_venv_guard_and_the_hint() {
        // An explicit `--python 3.12` is the user's choice: no guard, and the
        // error doesn't tell them to pass --python again.
        let venv = PathBuf::from("/proj/.venv/bin/python");
        let cands = [venv.clone(), PathBuf::from("python3.12")];
        let r = req("3.12");
        let got = run(&cands, &Policy::explicit(Some(&r)), |c| {
            match c.to_str().unwrap() {
                "/proj/.venv/bin/python" => {
                    Some(probe_at("/proj/.venv/bin/python", (3, 12, 0), false))
                }
                "python3.12" => Some(probe_at("/usr/bin/python3.12", (3, 12, 0), true)),
                _ => None,
            }
        })
        .unwrap();
        assert_eq!(got, PathBuf::from("/usr/bin/python3.12"));
        let err = run(
            &[PathBuf::from("rstest-no-such-python")],
            &Policy::explicit(None),
            |_| None,
        )
        .unwrap_err()
        .to_string();
        assert!(!err.contains("Pass one explicitly"), "{err}");
        assert!(err.contains("not found"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn venv_dir_maps_to_its_interpreter() {
        use std::fs;
        let tmp = std::env::temp_dir().join(format!("rstest-venvdir-{}", std::process::id()));
        fs::create_dir_all(tmp.join("bin")).unwrap();
        fs::write(tmp.join("bin/python"), "").unwrap();
        assert_eq!(super::interpreter_path(tmp.clone()), tmp.join("bin/python"));
        // A file passes through untouched.
        let file = tmp.join("bin/python");
        assert_eq!(super::interpreter_path(file.clone()), file);
        fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn no_venvs_honors_pin() {
        // Without a venv the pin filters the pool as before.
        let cands = [PathBuf::from("python3.13"), PathBuf::from("python3.10")];
        let r = req("3.10");
        let got = resolve_with(&cands, &pinned(&[], &r, Path::new(PIN)), |c| {
            match c.to_str().unwrap() {
                "python3.13" => Some(probe_at("/usr/bin/python3.13", (3, 13, 0), true)),
                "python3.10" => Some(probe_at("/usr/bin/python3.10", (3, 10, 0), true)),
                _ => None,
            }
        })
        .unwrap();
        assert_eq!(got.executable, PathBuf::from("/usr/bin/python3.10"));
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
