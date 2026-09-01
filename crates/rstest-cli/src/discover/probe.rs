//! Interrogate a candidate interpreter: run a probe script that reports its
//! identity and whether the worker shim imports, memoized in-process with a
//! disk layer keyed on file fingerprint.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::scheduling::worker::worker_pythonpath;

use super::cache::{disk_cache_get, disk_cache_put, file_fingerprint};

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
    from rstest_worker._internal import protocol, runner_pytest  # noqa: F401
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
pub(super) fn probe(candidate: &Path) -> Option<Probe> {
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
pub(super) fn cached_probe(candidate: &Path) -> Option<Probe> {
    static MEM: OnceLock<Mutex<HashMap<PathBuf, Option<Probe>>>> = OnceLock::new();
    let mem = MEM.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(hit) = mem.lock().unwrap().get(candidate) {
        return hit.clone();
    }

    // Disk cache only for absolute paths: a bare PATH name like `python3`
    // resolves differently as PATH changes, so caching it by name is unsafe.
    let fp = candidate
        .is_absolute()
        .then(|| file_fingerprint(candidate))
        .flatten();
    if let Some((m, s)) = fp {
        if let Some(p) = disk_cache_get(candidate, m, s) {
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
    if let (Some((m, s)), Some(p)) = (fp, &result) {
        // Persist POSITIVE probes only. A negative probe (the interpreter ran
        // but couldn't import the worker shim — e.g. msgpack not yet installed)
        // is keyed on the binary's fingerprint, which a `pip install` into
        // site-packages leaves unchanged. Persisting `false` would let the
        // stale miss survive the very install that fixes it, so rstest keeps
        // reporting "no usable Python interpreter found" until the cache is
        // deleted by hand. Re-probe negatives every run instead: cheap next to
        // a full session, and self-healing once the dep lands. The in-memory
        // cache above still de-dupes repeat probes within a single run.
        if p.worker_importable {
            disk_cache_put(candidate, m, s, p);
        }
    }
    result
}
