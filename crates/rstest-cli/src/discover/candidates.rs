//! Build the ordered candidate-interpreter list when no `--python` is given:
//! active venv, up-tree `.venv`, versioned PATH names, the Windows `py`
//! launcher, and uv-managed installs. Plus the `.python-version` reader.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::request::{parse_pyarg, PyArg};

/// Ordered candidate interpreters when no `--python` was given. Earlier wins.
/// Deduplicated, preserving order.
pub(super) fn discovery_candidates(scope: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        if !out.contains(&p) {
            out.push(p);
        }
    };

    // 1. The active virtualenv.
    if let Some(venv) = std::env::var_os("VIRTUAL_ENV") {
        if let Some(p) = venv_python(Path::new(&venv)) {
            push(p);
        }
    }

    // 2. A `.venv` found walking up from the scope dir (stop at a repo root).
    for dir in scope.ancestors() {
        if let Some(p) = venv_python(&dir.join(".venv")) {
            push(p);
        }
        if dir.join(".git").exists() {
            break;
        }
    }

    // 3. Versioned interpreter names on PATH.
    for name in path_python_names() {
        push(PathBuf::from(name));
    }

    // 3b. python.org installs reachable only through the Windows `py` launcher
    //     (not on PATH). No-op off Windows. Placed after PATH so an on-PATH
    //     interpreter still wins, but before uv-managed as a system source.
    for p in py_launcher_candidates() {
        push(p);
    }

    // 4. uv-managed interpreters, newest first: a fallback for version requests
    //    the venv/PATH can't satisfy. Placed last so a normal run still prefers
    //    the active environment (least surprise).
    for p in managed_candidates() {
        push(p);
    }

    out
}

/// uv's managed-interpreter install directory: `UV_PYTHON_INSTALL_DIR` if set,
/// else uv's platform default under the user data dir. None when we can't even
/// locate a home/data dir.
fn uv_python_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("UV_PYTHON_INSTALL_DIR") {
        return Some(PathBuf::from(d));
    }
    if cfg!(windows) {
        std::env::var_os("APPDATA").map(|b| PathBuf::from(b).join("uv/data/python"))
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .map(|b| b.join("uv/python"))
    }
}

/// Managed interpreters discovered under `uv_python_dir()`, newest version
/// first. Empty when uv manages nothing (the dir is absent).
fn managed_candidates() -> Vec<PathBuf> {
    uv_python_dir().map(|d| managed_in(&d)).unwrap_or_default()
}

/// Enumerate `<dir>/<install>/.../python`, sorted newest version first. Split
/// out from [`managed_candidates`] so tests need not touch process env.
pub(super) fn managed_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut installs: Vec<((u8, u8, u8), PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        if let Some(exe) = managed_exe(&entry.path()) {
            let ver = parse_dir_version(&entry.file_name().to_string_lossy());
            installs.push((ver, exe));
        }
    }
    // Descending by version; unparseable names sort last as (0,0,0).
    installs.sort_by_key(|i| std::cmp::Reverse(i.0));
    installs.into_iter().map(|(_, p)| p).collect()
}

/// The interpreter inside a managed install dir, if present.
fn managed_exe(install: &Path) -> Option<PathBuf> {
    for rel in ["bin/python3", "bin/python", "python.exe"] {
        let p = install.join(rel);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// Version embedded in a python-build-standalone dir name, e.g.
/// `cpython-3.12.4-macos-aarch64-none` or `cpython-3.13.0+freethreaded-...`.
/// Returns (0,0,0) when it can't be parsed (sorts such installs last).
pub(super) fn parse_dir_version(name: &str) -> (u8, u8, u8) {
    let version_field = match name.split('-').nth(1) {
        Some(v) => v.split('+').next().unwrap_or(v), // drop +freethreaded etc.
        None => return (0, 0, 0),
    };
    let mut it = version_field.split('.');
    let parse = |o: Option<&str>| o.and_then(|s| s.parse().ok());
    match (parse(it.next()), parse(it.next())) {
        (Some(maj), Some(min)) => (maj, min, parse(it.next()).unwrap_or(0)),
        _ => (0, 0, 0),
    }
}

/// The interpreter request implied by the nearest up-tree `.python-version`,
/// if any. A path entry resolves to [`PyArg::Path`]; a version name (`3.12`,
/// `pypy@3.10`) to a [`PyArg::Request`].
pub(super) fn python_version_arg(scope: &Path) -> Option<PyArg> {
    for dir in scope.ancestors() {
        if let Ok(raw) = std::fs::read_to_string(dir.join(".python-version")) {
            if let Some(line) = raw.lines().map(str::trim).find(|l| !l.is_empty()) {
                return Some(parse_pyarg(line));
            }
        }
        if dir.join(".git").exists() {
            break;
        }
    }
    None
}

/// `<venv>/bin/python` (unix) or `<venv>/Scripts/python.exe` (Windows), if it
/// exists.
pub(super) fn venv_python(venv: &Path) -> Option<PathBuf> {
    for rel in ["bin/python", "Scripts/python.exe"] {
        let p = venv.join(rel);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// Interpreter names to try on PATH, most-specific first. Free-threaded
/// (`python3.Xt`) and PyPy names come last so a request without those markers
/// prefers a standard CPython, while `3.13t` / `pypy@...` can still resolve.
pub(super) fn path_python_names() -> Vec<String> {
    if cfg!(windows) {
        vec!["python.exe".into(), "python3.exe".into(), "python".into()]
    } else {
        let mut v: Vec<String> = (9..=14).rev().map(|m| format!("python3.{m}")).collect();
        v.push("python3".into());
        v.push("python".into());
        v.extend((13..=14).rev().map(|m| format!("python3.{m}t")));
        v.push("pypy3".into());
        v.push("pypy".into());
        v
    }
}

/// Interpreters reported by the Windows Python Launcher (`py --list-paths`):
/// python.org installs that typically aren't on `PATH`. Empty (and never
/// spawns) off Windows, or when `py` is absent / reports nothing.
pub(super) fn py_launcher_candidates() -> Vec<PathBuf> {
    if !cfg!(windows) {
        return Vec::new();
    }
    let out = Command::new("py").arg("--list-paths").output().ok();
    match out {
        Some(o) if o.status.success() => parse_py_list_paths(&String::from_utf8_lossy(&o.stdout)),
        _ => Vec::new(),
    }
}

/// Parse `py --list-paths` output into absolute interpreter paths. Each line is
/// a tag, a path (which may contain spaces), and a default-`*` marker that may
/// lead or trail by launcher version; peel the tag, strip a `*` from either end.
pub(super) fn parse_py_list_paths(output: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        // Every interpreter line starts with the launcher's `-` tag.
        if !line.starts_with('-') {
            continue;
        }
        // Peel the tag (first whitespace-delimited token).
        let Some((_tag, rest)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        // The default install carries a `*` marker, leading or trailing by
        // launcher version. Strip whichever side it lands on so the path
        // doesn't keep a spurious ` *` suffix (which would fail its exists()).
        let path = rest.trim();
        let path = path.strip_prefix('*').unwrap_or(path).trim();
        let path = path.strip_suffix('*').unwrap_or(path).trim();
        if !path.is_empty() {
            out.push(PathBuf::from(path));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::request::PyArg;
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rstest-cand-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn parse_dir_version_handles_precision_and_junk() {
        assert_eq!(
            parse_dir_version("cpython-3.12.4-macos-aarch64-none"),
            (3, 12, 4)
        );
        // +freethreaded (and similar) suffixes are dropped before parsing.
        assert_eq!(
            parse_dir_version("cpython-3.13.0+freethreaded-linux"),
            (3, 13, 0)
        );
        // Two components => micro defaults to 0.
        assert_eq!(parse_dir_version("cpython-3.11-macos"), (3, 11, 0));
        // Unparseable / missing version field => (0,0,0) so it sorts last.
        assert_eq!(parse_dir_version("garbage"), (0, 0, 0));
        assert_eq!(parse_dir_version("cpython-notaversion-x"), (0, 0, 0));
    }

    #[test]
    fn parse_py_list_paths_peels_tag_and_star() {
        let out = parse_py_list_paths(
            " -V:3.12 *        C:\\Python312\\python.exe\n\
             -V:3.11          C:\\Python311\\python.exe *\n\
             not an entry line\n\
             -3.10\n", // tag only, no path => skipped (the `split_once` None arm)
        );
        assert_eq!(
            out,
            vec![
                PathBuf::from("C:\\Python312\\python.exe"),
                PathBuf::from("C:\\Python311\\python.exe"),
            ]
        );
    }

    #[test]
    fn managed_in_sorts_newest_first_and_empty_on_missing() {
        // Missing dir => empty (read_dir errors).
        assert!(managed_in(&tmpdir("missing").join("nope")).is_empty());

        let root = tmpdir("managed");
        for name in [
            "cpython-3.11.9-macos",
            "cpython-3.13.1-macos",
            "cpython-3.12.4-macos",
        ] {
            let bin = root.join(name).join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            std::fs::write(bin.join("python3"), "").unwrap();
        }
        // A dir with no interpreter inside is ignored.
        std::fs::create_dir_all(root.join("cpython-9.9.9-empty")).unwrap();

        let found = managed_in(&root);
        assert_eq!(found.len(), 3); // the empty install is skipped
                                    // Newest version first.
        assert!(found[0].to_string_lossy().contains("3.13.1"));
        assert!(found[2].to_string_lossy().contains("3.11.9"));
    }

    #[test]
    fn venv_python_finds_interpreter_or_none() {
        let empty = tmpdir("venv-empty");
        assert!(venv_python(&empty).is_none());

        let venv = tmpdir("venv-ok");
        std::fs::create_dir_all(venv.join("bin")).unwrap();
        std::fs::write(venv.join("bin/python"), "").unwrap();
        assert_eq!(venv_python(&venv), Some(venv.join("bin/python")));
    }

    #[test]
    fn python_version_arg_reads_first_nonblank_line() {
        let d = tmpdir("pyver");
        // Blank lines skipped; first real entry parsed as a version request.
        std::fs::write(d.join(".python-version"), "\n  \n3.12\n3.11\n").unwrap();
        assert!(matches!(python_version_arg(&d), Some(PyArg::Request(_))));

        // No file up-tree (and a .git boundary) => None.
        let none = tmpdir("pyver-none");
        std::fs::create_dir_all(none.join(".git")).unwrap();
        assert!(python_version_arg(&none).is_none());
    }

    #[test]
    #[cfg(not(windows))]
    fn path_python_names_orders_specific_first() {
        let names = path_python_names();
        let pos = |s: &str| names.iter().position(|n| n == s);
        // Versioned names come before the bare fallbacks...
        assert!(pos("python3.14") < pos("python3"));
        assert!(pos("python3") < pos("python"));
        // ...and free-threaded / pypy names come last.
        assert!(pos("python") < pos("python3.14t"));
        assert!(pos("python3.14t") < pos("pypy3"));
        assert_eq!(names.last().map(String::as_str), Some("pypy"));
    }
}
