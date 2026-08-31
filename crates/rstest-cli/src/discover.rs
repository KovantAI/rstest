//! Interpreter discovery + validation, uv-style: build an ordered candidate
//! list, probe each (confirm it runs and imports the worker shim), commit to
//! the first that passes. Positive probes are cached on disk keyed by mtime.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::worker::worker_pythonpath;

/// Minimum interpreter we'll run workers on.
const MIN_VERSION: (u8, u8) = (3, 9);

/// What a `--python` value or `.python-version` entry resolves to: either a
/// concrete interpreter path (authoritative - probed, never fallen back from)
/// or a version/implementation request matched against discovered candidates.
#[derive(Debug, PartialEq)]
enum PyArg {
    Path(PathBuf),
    Request(Request),
}

/// A version-and-implementation request, e.g. `>=3.12,<3.13`, `pypy@3.10`,
/// `3.13t`. All constraints are ANDed.
#[derive(Debug, Default, Clone, PartialEq)]
struct Request {
    /// `cpython`, `pypy`, ... matched case-insensitively. None = any.
    implementation: Option<String>,
    /// Free-threaded build required (the `t` suffix, e.g. `3.13t`).
    freethreaded: bool,
    constraints: Vec<Constraint>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Op {
    Eq,
    Ge,
    Le,
    Gt,
    Lt,
}

/// A single version constraint at the precision the user wrote: `3` pins only
/// major, `3.12` major+minor, `3.12.4` all three.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Constraint {
    op: Op,
    major: u8,
    minor: Option<u8>,
    micro: Option<u8>,
}

impl Constraint {
    fn matches(&self, v: (u8, u8, u8)) -> bool {
        let target = (self.major, self.minor.unwrap_or(0), self.micro.unwrap_or(0));
        match self.op {
            // Equality compares only the components the user specified, so a
            // bare `3.12` matches any 3.12.x.
            Op::Eq => {
                v.0 == self.major
                    && self.minor.is_none_or(|m| v.1 == m)
                    && self.micro.is_none_or(|m| v.2 == m)
            }
            Op::Ge => v >= target,
            Op::Le => v <= target,
            Op::Gt => v > target,
            Op::Lt => v < target,
        }
    }
}

impl std::fmt::Display for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(i) = &self.implementation {
            write!(f, "{i}@")?;
        }
        let parts: Vec<String> = self
            .constraints
            .iter()
            .map(|c| {
                let op = match c.op {
                    Op::Eq => "",
                    Op::Ge => ">=",
                    Op::Le => "<=",
                    Op::Gt => ">",
                    Op::Lt => "<",
                };
                let mut s = format!("{op}{}", c.major);
                if let Some(m) = c.minor {
                    s.push_str(&format!(".{m}"));
                }
                if let Some(m) = c.micro {
                    s.push_str(&format!(".{m}"));
                }
                s
            })
            .collect();
        write!(f, "{}", parts.join(","))?;
        if self.freethreaded {
            write!(f, "t")?;
        }
        Ok(())
    }
}

/// Does a probed interpreter satisfy a request?
fn matches(p: &Probe, r: &Request) -> bool {
    if let Some(want) = &r.implementation {
        if !p.implementation.eq_ignore_ascii_case(want) {
            return false;
        }
    }
    if r.freethreaded && !p.freethreaded {
        return false;
    }
    r.constraints.iter().all(|c| c.matches(p.version))
}

/// Interpret a `--python` / `.python-version` value. An existing path is taken
/// verbatim; otherwise we try to parse a version request; failing that we still
/// treat it as a path so the probe step produces a clear "not runnable" error.
fn parse_pyarg(s: &str) -> PyArg {
    let s = s.trim();
    if Path::new(s).exists() {
        return PyArg::Path(PathBuf::from(s));
    }
    match parse_request(s) {
        Some(r) => PyArg::Request(r),
        None => PyArg::Path(PathBuf::from(s)),
    }
}

/// Parse `[impl@]constraints[t]`, e.g. `pypy@>=3.10,<3.12`, `3.13t`, `3`.
/// None when the version portion isn't numeric (so the caller can fall back to
/// treating the whole string as a path).
fn parse_request(s: &str) -> Option<Request> {
    let mut req = Request::default();
    let ver_part = match s.split_once('@') {
        Some((impl_, rest)) => {
            req.implementation = Some(impl_.to_ascii_lowercase());
            rest
        }
        // A leading letter with no '@' is a bare implementation name (`pypy`).
        None if s.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) => {
            req.implementation = Some(s.to_ascii_lowercase());
            ""
        }
        None => s,
    };

    let ver_part = match ver_part.strip_suffix('t') {
        // `t` is the free-threaded marker only after a digit (`3.13t`), not a
        // stray trailing letter.
        Some(head) if head.chars().last().is_some_and(|c| c.is_ascii_digit()) => {
            req.freethreaded = true;
            head
        }
        _ => ver_part,
    };

    if !ver_part.is_empty() {
        for tok in ver_part.split(',') {
            req.constraints.push(parse_constraint(tok)?);
        }
    }

    // Reject an empty request (no impl, no constraints): that's not a spec.
    if req.implementation.is_none() && req.constraints.is_empty() {
        return None;
    }
    Some(req)
}

fn parse_constraint(tok: &str) -> Option<Constraint> {
    let tok = tok.trim();
    let (op, rest) = if let Some(r) = tok.strip_prefix(">=") {
        (Op::Ge, r)
    } else if let Some(r) = tok.strip_prefix("<=") {
        (Op::Le, r)
    } else if let Some(r) = tok.strip_prefix("==") {
        (Op::Eq, r)
    } else if let Some(r) = tok.strip_prefix('>') {
        (Op::Gt, r)
    } else if let Some(r) = tok.strip_prefix('<') {
        (Op::Lt, r)
    } else {
        (Op::Eq, tok)
    };
    let mut nums = rest.split('.');
    let major = nums.next()?.parse().ok()?;
    let minor = nums.next().map(|s| s.parse()).transpose().ok()?;
    let micro = nums.next().map(|s| s.parse()).transpose().ok()?;
    if nums.next().is_some() {
        return None; // too many components
    }
    Some(Constraint {
        op,
        major,
        minor,
        micro,
    })
}

/// One interrogated interpreter. Field names match the probe script's JSON.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Probe {
    /// Canonical interpreter path (`sys.executable`, symlinks resolved). This,
    /// not the candidate name we invoked, is what workers spawn with.
    pub executable: PathBuf,
    /// (major, minor, micro).
    pub version: (u8, u8, u8),
    /// `cpython`, `pypy`, ... surfaced by the version-grammar tier.
    #[allow(dead_code)]
    pub implementation: String,
    /// True on free-threaded (`Py_GIL_DISABLED`) builds; used by the
    /// version-grammar tier (`3.13t`).
    #[allow(dead_code)]
    pub freethreaded: bool,
    /// Whether `rstest_worker` imports under this interpreter - the thing that
    /// actually has to work for the run to start.
    pub worker_importable: bool,
}

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

/// Ordered candidate interpreters when no `--python` was given. Earlier wins.
/// Deduplicated, preserving order.
fn discovery_candidates(scope: &Path) -> Vec<PathBuf> {
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
fn managed_in(dir: &Path) -> Vec<PathBuf> {
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
fn parse_dir_version(name: &str) -> (u8, u8, u8) {
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
fn python_version_arg(scope: &Path) -> Option<PyArg> {
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
fn venv_python(venv: &Path) -> Option<PathBuf> {
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
fn path_python_names() -> Vec<String> {
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
fn py_launcher_candidates() -> Vec<PathBuf> {
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
fn parse_py_list_paths(output: &str) -> Vec<PathBuf> {
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

/// Script run inside a candidate to report its identity and shim-importability.
/// Prints a single JSON line matching [`Probe`].
const PROBE_SCRIPT: &str = "\
import json, os, sys, sysconfig
try:
    # Mirror the worker entrypoint (rstest_worker.__main__): importing these
    # pulls in the worker's real runtime deps (msgpack, the vendored pytest),
    # so a shallow `import rstest_worker` can't give a false positive.
    # __main__ prepends _vendor to sys.path *before* importing runner_pytest
    # (its `import pytest` must hit the vendored core, not a venv-installed
    # pytest), so replicate that here or the probe spuriously fails wherever
    # pytest isn't separately installed.
    import rstest_worker
    _vendor = os.path.join(os.path.dirname(rstest_worker.__file__), '_vendor')
    if _vendor not in sys.path:
        sys.path.insert(0, _vendor)
    from rstest_worker import protocol, runner_pytest  # noqa: F401
    ok = True
except BaseException:
    # protocol raises SystemExit (a BaseException, not Exception) when msgpack
    # is absent, so a bare `except Exception` would let it kill the probe.
    ok = False
v = sys.version_info
print(json.dumps({
    'executable': sys.executable,
    'version': [v.major, v.minor, v.micro],
    'implementation': sys.implementation.name,
    'freethreaded': bool(sysconfig.get_config_var('Py_GIL_DISABLED')),
    'worker_importable': ok,
}))";

/// Run the probe script in `candidate`. None if it can't run or doesn't speak
/// our protocol (not a Python interpreter, wrong version of Python, etc.).
fn probe(candidate: &Path) -> Option<Probe> {
    let out = Command::new(candidate)
        .args(["-c", PROBE_SCRIPT])
        .env("PYTHONPATH", worker_pythonpath())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

/// [`probe`] memoized in-process, with a disk layer for stable absolute paths.
/// Discovery repeats over the same names (per session, per monorepo project),
/// and managed/venv interpreters rarely change, so persisting skips subprocesses.
fn cached_probe(candidate: &Path) -> Option<Probe> {
    static MEM: OnceLock<Mutex<HashMap<PathBuf, Option<Probe>>>> = OnceLock::new();
    let mem = MEM.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(hit) = mem.lock().unwrap().get(candidate) {
        return hit.clone();
    }

    // Disk cache only for absolute paths: a bare PATH name like `python3`
    // resolves differently as PATH changes, so caching it by name is unsafe.
    let mtime = candidate
        .is_absolute()
        .then(|| file_mtime(candidate))
        .flatten();
    if let Some(m) = mtime {
        if let Some(p) = disk_cache_get(candidate, m) {
            mem.lock()
                .unwrap()
                .insert(candidate.to_path_buf(), Some(p.clone()));
            return Some(p);
        }
    }

    let result = probe(candidate);
    mem.lock()
        .unwrap()
        .insert(candidate.to_path_buf(), result.clone());
    if let (Some(m), Some(p)) = (mtime, &result) {
        // Persist POSITIVE probes only. A negative is keyed on the binary's
        // mtime, which a `pip install` fixing the shim leaves unchanged; caching
        // `false` would outlive the fix. Re-probe negatives every run instead.
        if p.worker_importable {
            disk_cache_put(candidate, m, p);
        }
    }
    result
}

fn file_mtime(p: &Path) -> Option<u64> {
    let modified = std::fs::metadata(p).ok()?.modified().ok()?;
    modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// On-disk probe cache. The filename carries a schema version so a format
/// change starts a fresh file rather than mis-parsing an old one.
#[derive(Default, Serialize, Deserialize)]
struct DiskCache {
    entries: HashMap<String, CacheEntry>,
}

#[derive(Clone, Serialize, Deserialize)]
struct CacheEntry {
    mtime: u64,
    probe: Probe,
}

/// `<cache dir>/rstest/interp-probes-v1.json`, or None if no cache dir is
/// resolvable (probing simply isn't persisted then).
fn cache_path() -> Option<PathBuf> {
    let base = if let Some(d) = std::env::var_os("RSTEST_CACHE_DIR") {
        PathBuf::from(d)
    } else if cfg!(windows) {
        PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?
    };
    Some(base.join("rstest").join("interp-probes-v1.json"))
}

/// The process-wide cache, loaded from disk on first use.
fn disk() -> &'static Mutex<DiskCache> {
    static D: OnceLock<Mutex<DiskCache>> = OnceLock::new();
    D.get_or_init(|| {
        let loaded = cache_path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| read_cache(&b));
        Mutex::new(loaded.unwrap_or_default())
    })
}

fn disk_cache_get(candidate: &Path, mtime: u64) -> Option<Probe> {
    let key = candidate.to_string_lossy().into_owned();
    let d = disk().lock().unwrap();
    let e = d.entries.get(&key)?;
    (e.mtime == mtime).then(|| e.probe.clone())
}

fn disk_cache_put(candidate: &Path, mtime: u64, probe: &Probe) {
    let key = candidate.to_string_lossy().into_owned();
    let mut d = disk().lock().unwrap();
    d.entries.insert(
        key,
        CacheEntry {
            mtime,
            probe: probe.clone(),
        },
    );
    if let Some(path) = cache_path() {
        let _ = write_cache(&path, &d); // best-effort; never fail the run on cache IO
    }
}

fn read_cache(bytes: &[u8]) -> Option<DiskCache> {
    serde_json::from_slice(bytes).ok()
}

/// Write the cache via temp-file + rename so a concurrent reader never sees a
/// half-written file. The temp name is pid-scoped to avoid clobbering between
/// concurrent rstest processes.
fn write_cache(path: &Path, cache: &DiskCache) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    let bytes = serde_json::to_vec(cache).unwrap_or_default();
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn disk_cache_roundtrips_and_gates_on_mtime() {
        let dir = std::env::temp_dir().join(format!("rstest-cache-{}", std::process::id()));
        let path = dir.join("probes.json");
        let mut cache = DiskCache::default();
        cache.entries.insert(
            "/opt/py/bin/python3".into(),
            CacheEntry {
                mtime: 1000,
                probe: probe_at("/opt/py/bin/python3", (3, 12, 4), true),
            },
        );
        write_cache(&path, &cache).unwrap();

        let loaded = read_cache(&std::fs::read(&path).unwrap()).unwrap();
        let e = loaded.entries.get("/opt/py/bin/python3").unwrap();
        assert_eq!(e.mtime, 1000);
        assert_eq!(e.probe.version, (3, 12, 4));
        // A stale mtime must miss (caller re-probes).
        assert_ne!(e.mtime, 1001);

        std::fs::remove_dir_all(&dir).unwrap();
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
