//! The `--watch --fork-pool=app` template server (Unix).
//!
//! The per-run fork-pool zygote imports the app and exits once the pool is
//! forked, so every watch cycle pays the import again. Under `--watch` a
//! template server instead stays alive for the whole session and forks a pool
//! per cycle (`python/rstest_worker/_internal/template.py`). Its workers connect
//! back over a Unix socket in a private directory (a long-lived process can't
//! be handed new pipes), and are otherwise ordinary reparented workers: killed
//! by pid, reaped by the server.
//!
//! Before each cycle the watch loop drops the server if anything it imported
//! changed (re-stat of its module files, site-packages, the pytest config and
//! the warm manifest). The next pool starts a fresh one. Any failure falls back
//! to the usual spawn path for that cycle; two in a row turn the template off
//! for the session.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use super::{
    apply_shared_worker_env, EventReader, Proc, SeenStartupStderr, StartupStderr, StderrTail,
    Tracked, Worker, WorkerEnv, ZYGOTE_REPORT_TIMEOUT,
};

/// A path's change stamp: mtime (ns) and size; None when it doesn't exist (so
/// one appearing counts as a change too).
type Stamp = Option<(i128, u64)>;

fn stamp(path: &Path) -> Stamp {
    let md = std::fs::metadata(path).ok()?;
    let mtime = md.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some((mtime.as_nanos() as i128, md.len()))
}

/// The server's first line.
#[derive(Deserialize)]
struct Ready {
    files: Vec<PathBuf>,
    site: Vec<PathBuf>,
    /// Holding the manifest's imports (false after a guard restart).
    #[serde(default)]
    warm: bool,
}

/// The part of the warm manifest a server's template was built from. Deny
/// entries are left out: the server produced them itself while importing.
#[derive(Default, PartialEq, Debug)]
struct Basis {
    modules: std::collections::BTreeSet<String>,
    rewrite: std::collections::BTreeSet<String>,
    env: std::collections::BTreeSet<String>,
}

impl Basis {
    fn of(m: &crate::scheduling::warm::Manifest) -> Self {
        Basis {
            modules: m.modules.iter().cloned().collect(),
            rewrite: m.rewrite.clone(),
            env: m.env.clone(),
        }
    }
}

/// Whether the manifest now asks for a different template than `basis` (None:
/// the server holds no manifest imports, as with no manifest yet or after a
/// guard restart). New modules to warm, or new rewrite marks or env keys
/// (which could deny an imported one), call for a rebuild; new deny entries
/// alone don't.
fn manifest_moved(basis: Option<&Basis>, now: Option<&crate::scheduling::warm::Manifest>) -> bool {
    match (basis, now) {
        (_, None) => false,
        (None, Some(m)) => !m.modules.is_empty(),
        (Some(b), Some(m)) => {
            m.modules.iter().any(|x| !b.modules.contains(x))
                || m.rewrite != b.rewrite
                || m.env != b.env
        }
    }
}

#[derive(Deserialize)]
struct Forked {
    pids: Vec<u32>,
}

pub(super) struct TemplateServer {
    child: Child,
    ctl_w: ChildStdin,
    ctl_r: File,
    listener: UnixListener,
    dir: PathBuf,
    stamps: Vec<(PathBuf, Stamp)>,
    /// What the template was built from; None when it holds no manifest imports.
    basis: Option<Basis>,
    manifest: PathBuf,
    seq: u64,
}

impl TemplateServer {
    fn start(python: &Path, n: usize, env: &WorkerEnv) -> Result<Self> {
        let dir = crate::scratch::dir().join("template");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        // Only this user may connect; the per-request token rejects strays.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        let sock = dir.join("s");
        let listener = UnixListener::bind(&sock).context("binding the template socket")?;
        let manifest = crate::cache::file(crate::scheduling::warm::FILE);
        let basis = env
            .warm
            .as_ref()
            .and_then(|w| w.manifest.as_deref())
            .and_then(crate::scheduling::warm::load)
            .map(|m| Basis::of(&m));
        let mut command = Command::new(python);
        command
            .args(["-m", "rstest_worker", "--template-server"])
            .arg(&sock)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped());
        apply_shared_worker_env(&mut command, n, env);
        let mut child = command.spawn().context("spawning the template server")?;
        let ctl_w = child.stdin.take().context("template server stdin")?;
        let ctl_r = File::from(OwnedFd::from(
            child.stdout.take().context("template stdout")?,
        ));
        let ready = match read_line_timeout(&ctl_r, ZYGOTE_REPORT_TIMEOUT) {
            Ok(line) => serde_json::from_str::<Ready>(&line).context("template server report"),
            Err(e) => Err(e),
        };
        let ready = match ready {
            Ok(r) => r,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e.context("starting the template server"));
            }
        };
        let cwd = std::env::current_dir().unwrap_or_default();
        let mut watched = ready.files;
        watched.extend(ready.site);
        watched.extend(crate::config::CONFIG_NAMES.iter().map(|n| cwd.join(n)));
        let stamps = watched
            .into_iter()
            .map(|p| {
                let s = stamp(&p);
                (p, s)
            })
            .collect();
        Ok(Self {
            child,
            ctl_w,
            ctl_r,
            listener,
            dir,
            stamps,
            basis: basis.filter(|_| ready.warm),
            manifest,
            seq: 0,
        })
    }

    /// Anything the template imported, or that decides what it imports, moved.
    fn stale(&self) -> bool {
        self.stamps.iter().any(|(p, s)| stamp(p) != *s)
            || manifest_moved(
                self.basis.as_ref(),
                crate::scheduling::warm::load(&self.manifest).as_ref(),
            )
    }

    /// Fork workers `idx` (of a pool of `count`) with this run's environment.
    fn fork(
        &mut self,
        idx: &[usize],
        count: usize,
        env: &WorkerEnv,
        seen: &SeenStartupStderr,
    ) -> Result<Vec<Worker>> {
        self.seq += 1;
        let token = format!(
            "{:x}-{:x}-{}",
            std::process::id(),
            UNIX_EPOCH.elapsed().map_or(0, |d| d.as_nanos()),
            self.seq
        );
        // The run's own environment (run uid, worker count, opt-ins), applied
        // by each child over what the server started with.
        let mut probe = Command::new("python");
        apply_shared_worker_env(&mut probe, count, env);
        let run_env: serde_json::Map<String, serde_json::Value> = probe
            .get_envs()
            .map(|(k, v)| {
                let v = v.map_or(serde_json::Value::Null, |v| {
                    serde_json::Value::String(v.to_string_lossy().into_owned())
                });
                (k.to_string_lossy().into_owned(), v)
            })
            .collect();
        let request = serde_json::json!({"op": "fork", "idx": idx, "token": token, "env": run_env});
        writeln!(self.ctl_w, "{request}").context("asking the template server to fork")?;
        self.ctl_w.flush()?;
        let reply = read_line_timeout(&self.ctl_r, ZYGOTE_REPORT_TIMEOUT)?;
        let pids = serde_json::from_str::<Forked>(&reply)
            .context("template server fork reply")?
            .pids;
        if pids.len() != idx.len() {
            bail!(
                "template server forked {} of {} workers",
                pids.len(),
                idx.len()
            );
        }
        // Recorded while the server is their parent: no pid can be recycled yet.
        let tracked: Vec<Option<Tracked>> = pids.iter().map(|&p| Tracked::capture(p)).collect();
        let mut streams = self.accept(&token, 2 * idx.len())?;
        let mut workers = Vec::with_capacity(idx.len());
        for (&i, tracked) in idx.iter().zip(tracked) {
            let conn = streams
                .remove(&(i, "conn".to_string()))
                .with_context(|| format!("worker gw{i} never connected"))?;
            let err = streams
                .remove(&(i, "err".to_string()))
                .with_context(|| format!("worker gw{i} never sent its stderr"))?;
            workers.push(worker_from(conn, err, tracked, seen)?);
        }
        Ok(workers)
    }

    /// Accept `want` connections carrying `token`, keyed by (index, kind).
    /// Anything else that connects is dropped.
    fn accept(&self, token: &str, want: usize) -> Result<HashMap<(usize, String), UnixStream>> {
        let deadline = Instant::now() + ZYGOTE_REPORT_TIMEOUT;
        self.listener.set_nonblocking(true)?;
        let mut out = HashMap::new();
        while out.len() < want {
            let stream = match self.listener.accept() {
                Ok((s, _)) => s,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        bail!("workers did not connect back to the template socket");
                    }
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            stream.set_nonblocking(false)?;
            stream.set_read_timeout(Some(deadline.saturating_duration_since(Instant::now())))?;
            let Ok(header) = read_header(&stream) else {
                continue;
            };
            stream.set_read_timeout(None)?;
            let mut parts = header.split(' ');
            let (Some(t), Some(i), Some(kind)) = (parts.next(), parts.next(), parts.next()) else {
                continue;
            };
            let Ok(i) = i.parse::<usize>() else { continue };
            if t == token && (kind == "conn" || kind == "err") {
                out.insert((i, kind.to_string()), stream);
            }
        }
        Ok(out)
    }
}

impl Drop for TemplateServer {
    fn drop(&mut self) {
        let _ = writeln!(self.ctl_w, r#"{{"op": "exit"}}"#);
        let _ = self.ctl_w.flush();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A worker over its two connected streams, wired like a fork-prewarmed one.
fn worker_from(
    conn: UnixStream,
    err: UnixStream,
    tracked: Option<Tracked>,
    seen: &SeenStartupStderr,
) -> Result<Worker> {
    let cmd_w = File::from(OwnedFd::from(conn.try_clone()?));
    let evt_r = File::from(OwnedFd::from(conn));
    let err_r = File::from(OwnedFd::from(err));
    let startup = Arc::new(StartupStderr::new(Arc::clone(seen)));
    let stderr = StderrTail::capture(err_r, Some(startup));
    let abandoned = Arc::new(AtomicBool::new(false));
    let mut reader = EventReader::new(evt_r, Arc::clone(&abandoned));
    reader.startup = stderr.startup.clone();
    Ok(Worker {
        proc: Proc::Reparented(tracked),
        cmd_w,
        reader: Some(reader),
        stderr: Some(stderr),
        shutdown_sent: None,
        killed: false,
        abandoned,
    })
}

/// The `<token> <idx> <kind>` line a worker sends first, byte by byte so
/// nothing after it is consumed.
fn read_header(mut stream: &UnixStream) -> Result<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while line.len() < 256 {
        if stream.read(&mut byte)? == 0 {
            bail!("closed before its header");
        }
        if byte[0] == b'\n' {
            return Ok(String::from_utf8(line)?);
        }
        line.push(byte[0]);
    }
    bail!("header too long")
}

/// One line from the server's stdout, failing if it doesn't arrive in time.
fn read_line_timeout(file: &File, timeout: Duration) -> Result<String> {
    let deadline = Instant::now() + timeout;
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!(
                "template server did not answer within {}s",
                timeout.as_secs()
            );
        }
        let mut pfd = libc::pollfd {
            fd: file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = remaining.as_millis().min(libc::c_int::MAX as u128) as libc::c_int;
        // SAFETY: `pfd` is one valid pollfd for the duration of the call.
        let ready = unsafe { libc::poll(&mut pfd, 1, ms.max(1)) };
        if ready < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err.into());
        }
        if ready == 0 {
            continue;
        }
        match (&*file).read(&mut byte) {
            Ok(0) => bail!("template server exited"),
            Ok(_) if byte[0] == b'\n' => return Ok(String::from_utf8(line)?),
            Ok(_) => line.push(byte[0]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
}

/// The session's template server, if `--watch --fork-pool=app` turned it on.
struct Slot {
    enabled: bool,
    server: Option<TemplateServer>,
    failures: u32,
}

static SLOT: Mutex<Slot> = Mutex::new(Slot {
    enabled: false,
    server: None,
    failures: 0,
});

fn slot() -> std::sync::MutexGuard<'static, Slot> {
    SLOT.lock().unwrap_or_else(|p| p.into_inner())
}

/// Called once by the watch loop for `--watch --fork-pool=app`.
pub fn enable() {
    slot().enabled = true;
}

/// Before each watch cycle: drop a server whose template is out of date, so
/// the cycle's pool starts a fresh one.
pub fn drop_if_stale() {
    let mut s = slot();
    if s.server.as_ref().is_some_and(TemplateServer::stale) {
        s.server = None;
    }
}

/// Fork the pool off the session's server (starting it if needed). None when
/// the template is off for this run, or failed this time: the caller then
/// spawns the pool as usual.
pub(super) fn fork_pool(
    python: &Path,
    n: usize,
    env: &WorkerEnv,
    seen: &SeenStartupStderr,
) -> Option<Result<Vec<Worker>>> {
    let mut s = slot();
    if !s.enabled || env.warm.is_none() {
        return None;
    }
    if s.server.is_none() {
        match TemplateServer::start(python, n, env) {
            Ok(server) => s.server = Some(server),
            Err(e) => {
                s.fail(&e);
                return None;
            }
        }
    }
    let idx: Vec<usize> = (0..n).collect();
    let result = s.server.as_mut().map(|t| t.fork(&idx, n, env, seen))?;
    match result {
        Ok(workers) => {
            s.failures = 0;
            Some(Ok(workers))
        }
        Err(e) => {
            s.server = None;
            s.fail(&e);
            None
        }
    }
}

impl Slot {
    fn fail(&mut self, e: &anyhow::Error) {
        self.failures += 1;
        eprintln!("rstest: note: --watch template: {e:#}; this cycle spawns its workers as usual");
        if self.failures >= 2 {
            self.enabled = false;
            eprintln!("rstest: note: --watch template off for this session after two failures");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduling::warm::Manifest;

    fn manifest(modules: &[&str], env: &[&str]) -> Manifest {
        Manifest {
            modules: modules.iter().map(|s| s.to_string()).collect(),
            env: env.iter().map(|s| s.to_string()).collect(),
            ..Manifest::default()
        }
    }

    #[test]
    fn a_new_deny_entry_alone_does_not_rebuild() {
        // Persona regression: the server denied a module that reads the worker
        // id; recording that deny made the next cycle rebuild an identical
        // template.
        let built = manifest(&["slowapp", "envapp"], &[]);
        let basis = Basis::of(&built);
        let mut now = manifest(&["slowapp", "envapp"], &[]);
        now.deny.insert(
            "envapp".into(),
            "reads PYTEST_XDIST_WORKER at import".into(),
        );
        assert!(!manifest_moved(Some(&basis), Some(&now)));
    }

    #[test]
    fn new_modules_or_env_keys_rebuild() {
        let basis = Basis::of(&manifest(&["a"], &[]));
        assert!(manifest_moved(
            Some(&basis),
            Some(&manifest(&["a", "b"], &[]))
        ));
        assert!(manifest_moved(
            Some(&basis),
            Some(&manifest(&["a"], &["DJANGO_SETTINGS_MODULE"]))
        ));
        // A narrowed run never shrinks the manifest, but a subset is no reason
        // to rebuild either.
        assert!(!manifest_moved(Some(&basis), Some(&manifest(&["a"], &[]))));
    }

    #[test]
    fn a_cold_server_rebuilds_once_a_manifest_exists() {
        assert!(manifest_moved(None, Some(&manifest(&["a"], &[]))));
        assert!(!manifest_moved(None, Some(&manifest(&[], &[]))));
        assert!(!manifest_moved(None, None));
    }

    #[test]
    fn stamp_tracks_content_and_appearance() {
        let dir = std::env::temp_dir().join(format!("rstest-tpl-stamp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("mod.py");
        assert_eq!(stamp(&f), None);
        std::fs::write(&f, "x = 1\n").unwrap();
        let first = stamp(&f);
        assert!(first.is_some());
        std::fs::write(&f, "x = 22\n").unwrap();
        assert_ne!(stamp(&f), first);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forks_live_workers_that_shut_down_cleanly() {
        super::super::tests::with_live_python(|_, python| {
            let mut env = super::super::tests::base_env();
            env.run_uid = format!("tpl-{}", std::process::id());
            env.warm = Some(crate::scheduling::warm::WorkerWarm {
                manifest: None,
                result: crate::scratch::dir().join("tpl-test-result.json"),
            });
            let mut server = TemplateServer::start(python, 2, &env).expect("template server");
            let seen = SeenStartupStderr::default();
            let workers = server.fork(&[0, 1], 2, &env, &seen).expect("fork workers");
            assert_eq!(workers.len(), 2);
            let mut pids: Vec<u32> = workers
                .iter()
                .map(|w| super::super::tests::pid_of(w).expect("pid"))
                .collect();
            assert!(pids.iter().all(|&p| super::super::tests::alive(p)));
            pids.dedup();
            assert_eq!(pids.len(), 2, "distinct workers");
            for w in workers {
                w.shutdown().expect("shutdown");
            }
            // A second fork from the same server: the session reuses it.
            let again = server.fork(&[0], 1, &env, &seen).expect("second fork");
            for w in again {
                w.shutdown().expect("shutdown");
            }
            drop(server);
        });
    }
}
