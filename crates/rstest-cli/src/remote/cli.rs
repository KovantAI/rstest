//! Cloud-CLI transport: `s3://` via `aws`, `gs://` via `gcloud` / `gsutil`.

use anyhow::{Context, Result};

use super::Transport;

// ---- CliTransport: s3:// / gs:// via the cloud CLI --------------------------

/// A captured cloud-CLI invocation result. `success` is the process exit
/// status; `stdout` carries object bytes on a read; `stderr` is inspected to
/// tell a missing object from a real (auth/network) failure.
pub struct CliOutput {
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

/// Spawns cloud-CLI subprocesses. Behind a trait so `CliTransport` is
/// unit-testable without a real `aws`/`gcloud` binary or a real bucket.
pub trait CommandRunner {
    /// Run `program args...`, feeding `stdin` when present, and capture the
    /// result. An `Err` is reserved for the spawn itself failing; a non-zero
    /// exit is reported via `CliOutput::success == false`.
    fn run(&self, program: &str, args: &[String], stdin: Option<&[u8]>) -> Result<CliOutput>;
    /// Whether `program` resolves on `PATH` (cheap, no spawn).
    fn program_exists(&self, program: &str) -> bool;
}

/// Real runner over `std::process::Command`.
struct RealRunner;

impl CommandRunner for RealRunner {
    fn run(&self, program: &str, args: &[String], stdin: Option<&[u8]>) -> Result<CliOutput> {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            });
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning `{program}`"))?;
        // Feed stdin on a separate thread so a child that writes to stdout
        // before draining stdin (a payload larger than the pipe buffer) can't
        // deadlock against a blocking write here. Dropping the handle at the
        // end signals EOF. A write error (e.g. the CLI exited early and closed
        // the pipe) surfaces via the child's exit status + stderr, so we don't
        // propagate it separately.
        let writer = stdin.map(|data| {
            let mut si = child.stdin.take().expect("stdin piped");
            let data = data.to_vec();
            std::thread::spawn(move || {
                let _ = si.write_all(&data);
            })
        });
        let out = child
            .wait_with_output()
            .with_context(|| format!("waiting for `{program}`"))?;
        if let Some(w) = writer {
            let _ = w.join();
        }
        Ok(CliOutput {
            success: out.status.success(),
            stdout: out.stdout,
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
    fn program_exists(&self, program: &str) -> bool {
        let Some(paths) = std::env::var_os("PATH") else {
            return false;
        };
        // On Windows the CLIs land as `aws.exe` / `gcloud.cmd` / `gsutil.cmd`, so
        // a bare-name probe would falsely report them absent; try each PATHEXT
        // suffix in addition to the bare name.
        let exts = program_extensions();
        std::env::split_paths(&paths).any(|dir| {
            exts.iter()
                .any(|ext| dir.join(format!("{program}{ext}")).is_file())
        })
    }
}

/// Executable-name suffixes to try when probing `PATH`. On Windows this is the
/// bare name plus each `PATHEXT` entry (`aws` also matches `aws.exe`); on other
/// platforms just the bare name.
fn program_extensions() -> Vec<String> {
    if cfg!(windows) {
        let raw = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
        let mut exts = vec![String::new()];
        exts.extend(
            raw.split(';')
                .filter(|e| !e.is_empty())
                .map(|e| e.to_string()),
        );
        exts
    } else {
        vec![String::new()]
    }
}

#[derive(Clone, Copy)]
enum CloudTool {
    Aws,
    Gcloud,
    Gsutil,
}

/// Object-store transport that drives the cloud CLI already installed and
/// authenticated on the runner (the same tool a hand-rolled `aws s3 sync` used).
/// No SDK, no async runtime — credentials come from the process environment.
pub struct CliTransport {
    tool: CloudTool,
    /// Bucket+prefix root, trailing slash trimmed (e.g. `s3://bucket/prefix`).
    root: String,
    runner: Box<dyn CommandRunner>,
}

/// A read either returned bytes or the object was absent.
enum ReadOutcome {
    Found(Vec<u8>),
    Missing,
}

/// Whether stderr means "no such object" rather than a real failure, across
/// `aws`/`gcloud`/`gsutil`. Compared case-insensitively. A connectivity /
/// auth / permission failure is never "object absent" — it's checked first so
/// its message (which may itself contain "not found") can't mask a real error
/// as a missing object and silently read as an empty cache.
fn looks_like_not_found(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    const HARD: &[&str] = &[
        "could not connect",
        "could not resolve",
        "connection",
        "timed out",
        "timeout",
        "unable to locate credentials",
        "access denied",
        "accessdenied",
        "forbidden",
        "unauthorized",
        "network",
    ];
    if HARD.iter().any(|m| s.contains(m)) {
        return false;
    }
    const MISSING: &[&str] = &[
        "nosuchkey",
        "not found",
        "does not exist",
        "matched no objects",
        "no url matched",
        "no urls matched",
    ];
    MISSING.iter().any(|m| s.contains(m))
}

impl CliTransport {
    /// Build from an `s3://` / `gs://` remote, probing for the needed CLI. For
    /// `gs://`, `gcloud storage` is preferred with a fallback to `gsutil`.
    pub fn for_remote(remote: &str) -> Result<Self> {
        Self::for_remote_with(remote, Box::new(RealRunner))
    }

    fn for_remote_with(remote: &str, runner: Box<dyn CommandRunner>) -> Result<Self> {
        let root = remote.trim_end_matches('/').to_string();
        if remote.starts_with("s3://") {
            if !runner.program_exists("aws") {
                anyhow::bail!("cache remote {remote} needs the `aws` CLI on PATH (authenticated)");
            }
            Ok(Self {
                tool: CloudTool::Aws,
                root,
                runner,
            })
        } else if remote.starts_with("gs://") {
            let tool = if runner.program_exists("gcloud") {
                CloudTool::Gcloud
            } else if runner.program_exists("gsutil") {
                CloudTool::Gsutil
            } else {
                anyhow::bail!(
                    "cache remote {remote} needs the `gcloud` or `gsutil` CLI on PATH (authenticated)"
                );
            };
            Ok(Self { tool, root, runner })
        } else {
            anyhow::bail!("CliTransport: unsupported remote {remote}");
        }
    }

    fn program(&self) -> &str {
        match self.tool {
            CloudTool::Aws => "aws",
            CloudTool::Gcloud => "gcloud",
            CloudTool::Gsutil => "gsutil",
        }
    }

    fn base_url(&self) -> String {
        format!("{}/base.json", self.root)
    }
    fn segment_url(&self, id: &str) -> String {
        format!("{}/segments/seg-{id}.json", self.root)
    }
    fn segments_prefix(&self) -> String {
        format!("{}/segments/", self.root)
    }

    fn owned(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }
    fn list_args(&self, prefix: &str) -> Vec<String> {
        match self.tool {
            CloudTool::Aws => Self::owned(&["s3", "ls", prefix]),
            CloudTool::Gcloud => Self::owned(&["storage", "ls", prefix]),
            CloudTool::Gsutil => Self::owned(&["ls", prefix]),
        }
    }
    fn read_args(&self, url: &str) -> Vec<String> {
        match self.tool {
            CloudTool::Aws => Self::owned(&["s3", "cp", url, "-"]),
            CloudTool::Gcloud => Self::owned(&["storage", "cat", url]),
            CloudTool::Gsutil => Self::owned(&["cat", url]),
        }
    }
    fn write_args(&self, url: &str) -> Vec<String> {
        match self.tool {
            CloudTool::Aws => Self::owned(&["s3", "cp", "-", url]),
            CloudTool::Gcloud => Self::owned(&["storage", "cp", "-", url]),
            CloudTool::Gsutil => Self::owned(&["cp", "-", url]),
        }
    }
    fn delete_args(&self, url: &str) -> Vec<String> {
        match self.tool {
            CloudTool::Aws => Self::owned(&["s3", "rm", url]),
            CloudTool::Gcloud => Self::owned(&["storage", "rm", url]),
            CloudTool::Gsutil => Self::owned(&["rm", url]),
        }
    }

    fn read_url(&self, url: &str) -> Result<ReadOutcome> {
        let out = self
            .runner
            .run(self.program(), &self.read_args(url), None)?;
        if out.success {
            Ok(ReadOutcome::Found(out.stdout))
        } else if looks_like_not_found(&out.stderr) {
            Ok(ReadOutcome::Missing)
        } else {
            anyhow::bail!("reading {url}: {}", out.stderr.trim())
        }
    }
    fn write_url(&self, url: &str, bytes: &[u8]) -> Result<()> {
        let out = self
            .runner
            .run(self.program(), &self.write_args(url), Some(bytes))?;
        if out.success {
            Ok(())
        } else {
            anyhow::bail!("writing {url}: {}", out.stderr.trim())
        }
    }
}

impl Transport for CliTransport {
    fn list_segment_ids(&self) -> Result<Vec<String>> {
        let prefix = self.segments_prefix();
        let out = self
            .runner
            .run(self.program(), &self.list_args(&prefix), None)?;
        if !out.success {
            // An empty prefix lists as either exit-0-empty or a not-found /
            // empty-stderr non-zero (aws `s3 ls` of an absent prefix); treat
            // both as "no segments yet". A real error carries stderr.
            let se = out.stderr.trim();
            if se.is_empty() || looks_like_not_found(se) {
                return Ok(Vec::new());
            }
            anyhow::bail!("listing {prefix}: {se}");
        }
        // Whitespace-tokenize the listing (aws `s3 ls` columns, gcloud/gsutil
        // full URLs) and keep only `seg-<id>.json` basenames — date/size/PRE
        // tokens never match the prefix filter.
        let text = String::from_utf8_lossy(&out.stdout);
        let mut ids = Vec::new();
        for tok in text.split_whitespace() {
            let name = tok.rsplit('/').next().unwrap_or(tok);
            if let Some(id) = name
                .strip_prefix("seg-")
                .and_then(|s| s.strip_suffix(".json"))
            {
                ids.push(id.to_string());
            }
        }
        Ok(ids)
    }
    fn read_segment(&self, id: &str) -> Result<Option<Vec<u8>>> {
        match self.read_url(&self.segment_url(id))? {
            ReadOutcome::Found(b) => Ok(Some(b)),
            ReadOutcome::Missing => Ok(None),
        }
    }
    fn read_base(&self) -> Result<Option<Vec<u8>>> {
        match self.read_url(&self.base_url())? {
            ReadOutcome::Found(b) => Ok(Some(b)),
            ReadOutcome::Missing => Ok(None),
        }
    }
    fn write_segment(&self, id: &str, bytes: &[u8]) -> Result<()> {
        self.write_url(&self.segment_url(id), bytes)
    }
    fn write_base(&self, bytes: &[u8]) -> Result<()> {
        self.write_url(&self.base_url(), bytes)
    }
    fn delete_segment(&self, id: &str) -> Result<()> {
        let url = self.segment_url(id);
        let out = self
            .runner
            .run(self.program(), &self.delete_args(&url), None)?;
        if out.success || looks_like_not_found(&out.stderr) {
            Ok(())
        } else {
            anyhow::bail!("deleting {url}: {}", out.stderr.trim())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::pull;
    use super::*;
    use crate::reporting::sink::Sink;

    #[test]
    fn real_runner_spawn_failure_is_an_err() {
        // A program that can't resolve => spawn fails => Err (reserved for the
        // spawn itself, distinct from a non-zero exit).
        let r = RealRunner;
        let bogus = "rstest-definitely-not-a-real-binary-xyz".to_string();
        assert!(r.run(&bogus, &[], None).is_err());
    }

    #[test]
    fn real_runner_program_exists_probes_path() {
        // Walks every PATH entry (and, on Windows, every PATHEXT suffix) and
        // finds no match for a bogus name. Avoids mutating the process-global
        // PATH, which would race the rest of the parallel suite.
        let r = RealRunner;
        assert!(
            !r.program_exists("rstest-definitely-not-a-real-binary-xyz"),
            "a missing program must not resolve"
        );
    }

    #[test]
    fn program_extensions_bare_name_on_unix() {
        // Non-Windows: a single empty suffix, so `aws` probes exactly `aws`.
        let exts = program_extensions();
        assert!(exts.contains(&String::new()));
        #[cfg(not(windows))]
        assert_eq!(exts.len(), 1);
    }

    // ---- CliTransport (s3:// / gs://) --------------------------------------

    use std::cell::RefCell;

    /// A stubbed cloud CLI: `programs` are the binaries that "exist"; `handler`
    /// maps each (program, args, stdin) to a canned `CliOutput`; `calls`
    /// records every invocation for assertions.
    type StubHandler = Box<dyn Fn(&str, &[String], Option<&[u8]>) -> CliOutput>;
    type StubCall = (String, Vec<String>, Option<Vec<u8>>);
    struct StubRunner {
        programs: Vec<String>,
        handler: StubHandler,
        calls: RefCell<Vec<StubCall>>,
    }
    impl StubRunner {
        fn new(
            programs: &[&str],
            handler: impl Fn(&str, &[String], Option<&[u8]>) -> CliOutput + 'static,
        ) -> Box<Self> {
            Box::new(StubRunner {
                programs: programs.iter().map(|s| s.to_string()).collect(),
                handler: Box::new(handler),
                calls: RefCell::new(Vec::new()),
            })
        }
    }
    impl CommandRunner for StubRunner {
        fn run(&self, program: &str, args: &[String], stdin: Option<&[u8]>) -> Result<CliOutput> {
            self.calls.borrow_mut().push((
                program.into(),
                args.to_vec(),
                stdin.map(|b| b.to_vec()),
            ));
            Ok((self.handler)(program, args, stdin))
        }
        fn program_exists(&self, program: &str) -> bool {
            self.programs.iter().any(|p| p == program)
        }
    }
    fn ok_out(stdout: &[u8]) -> CliOutput {
        CliOutput {
            success: true,
            stdout: stdout.to_vec(),
            stderr: String::new(),
        }
    }
    fn err_out(stderr: &str) -> CliOutput {
        CliOutput {
            success: false,
            stdout: Vec::new(),
            stderr: stderr.into(),
        }
    }

    #[test]
    fn pull_fails_on_transport_error_but_skips_missing_and_corrupt() {
        // A listed segment's read outcome drives pull: a transport/auth error
        // must fail the pull (no silent partial baseline); a `None` (pruned by a
        // concurrent compaction mid-pull) or a corrupt blob is skipped.
        enum Mode {
            Err,
            None,
            Corrupt,
        }
        struct Stub(Mode);
        impl Transport for Stub {
            fn list_segment_ids(&self) -> Result<Vec<String>> {
                Ok(vec!["s1".into()])
            }
            fn read_segment(&self, _id: &str) -> Result<Option<Vec<u8>>> {
                match self.0 {
                    Mode::Err => anyhow::bail!("boom-network"),
                    Mode::None => Ok(None),
                    Mode::Corrupt => Ok(Some(b"{ not json".to_vec())),
                }
            }
            fn read_base(&self) -> Result<Option<Vec<u8>>> {
                Ok(None)
            }
            fn write_segment(&self, _: &str, _: &[u8]) -> Result<()> {
                Ok(())
            }
            fn write_base(&self, _: &[u8]) -> Result<()> {
                Ok(())
            }
            fn delete_segment(&self, _: &str) -> Result<()> {
                Ok(())
            }
        }
        assert!(pull(&Stub(Mode::Err), &mut Sink::captured().0).is_err());
        assert!(pull(&Stub(Mode::None), &mut Sink::captured().0).is_ok());
        assert!(pull(&Stub(Mode::Corrupt), &mut Sink::captured().0).is_ok());
    }

    #[test]
    fn not_found_detection_ignores_connectivity_and_auth() {
        // Genuine object-absent messages across the three CLIs.
        assert!(looks_like_not_found(
            "fatal error: An error occurred (NoSuchKey)"
        ));
        assert!(looks_like_not_found("CommandException: No URLs matched"));
        assert!(looks_like_not_found("the object does not exist"));
        // Connectivity / auth / permission failures are NOT "missing object",
        // even when the text happens to contain a not-found-ish phrase.
        assert!(!looks_like_not_found(
            "Could not connect to the endpoint URL"
        ));
        assert!(!looks_like_not_found("Unable to locate credentials"));
        assert!(!looks_like_not_found("An error occurred (AccessDenied)"));
        assert!(!looks_like_not_found(
            "could not resolve host: bucket not found"
        ));
    }

    #[test]
    fn cli_aws_list_parses_ls_columns() {
        // `aws s3 ls` prints date/time/size/name columns; only seg-*.json
        // basenames survive the filter.
        let out = "2024-01-01 12:00:00        42 seg-run1-1of2.json\n\
                   2024-01-01 12:00:01        42 seg-run1-2of2.json\n\
                                              PRE nested/\n";
        let t = CliTransport::for_remote_with(
            "s3://bucket/prefix",
            StubRunner::new(&["aws"], move |_p, args, _| {
                assert_eq!(args, &["s3", "ls", "s3://bucket/prefix/segments/"]);
                ok_out(out.as_bytes())
            }),
        )
        .unwrap();
        let mut ids = t.list_segment_ids().unwrap();
        ids.sort();
        assert_eq!(ids, vec!["run1-1of2".to_string(), "run1-2of2".to_string()]);
    }

    #[test]
    fn cli_gcloud_list_parses_full_urls() {
        let out = "gs://bucket/p/segments/seg-a.json\n\
                   gs://bucket/p/segments/seg-b.json\n";
        let t = CliTransport::for_remote_with(
            "gs://bucket/p/",
            StubRunner::new(&["gcloud"], move |p, args, _| {
                assert_eq!(p, "gcloud");
                assert_eq!(args, &["storage", "ls", "gs://bucket/p/segments/"]);
                ok_out(out.as_bytes())
            }),
        )
        .unwrap();
        let mut ids = t.list_segment_ids().unwrap();
        ids.sort();
        assert_eq!(ids, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn cli_list_empty_prefix_is_not_an_error() {
        // aws `s3 ls` of an absent prefix: non-zero exit, empty stderr.
        let t = CliTransport::for_remote_with(
            "s3://bucket/p",
            StubRunner::new(&["aws"], |_, _, _| err_out("")),
        )
        .unwrap();
        assert!(t.list_segment_ids().unwrap().is_empty());
    }

    #[test]
    fn cli_list_real_error_surfaces() {
        let t = CliTransport::for_remote_with(
            "s3://bucket/p",
            StubRunner::new(&["aws"], |_, _, _| {
                err_out("An error occurred (AccessDenied) when calling ListObjects")
            }),
        )
        .unwrap();
        assert!(t.list_segment_ids().is_err());
    }

    #[test]
    fn cli_read_base_missing_is_none() {
        let t = CliTransport::for_remote_with(
            "s3://bucket/p",
            StubRunner::new(&["aws"], |_, _, _| {
                err_out("fatal error: An error occurred (NoSuchKey)")
            }),
        )
        .unwrap();
        assert_eq!(t.read_base().unwrap(), None);
    }

    #[test]
    fn cli_read_base_real_error_surfaces() {
        let t = CliTransport::for_remote_with(
            "s3://bucket/p",
            StubRunner::new(&["aws"], |_, _, _| err_out("Unable to locate credentials")),
        )
        .unwrap();
        assert!(t.read_base().is_err());
    }

    #[test]
    fn cli_write_segment_feeds_stdin_and_builds_url() {
        let t = CliTransport::for_remote_with(
            "s3://bucket/p",
            StubRunner::new(&["aws"], |_, args, stdin| {
                assert_eq!(
                    args,
                    &["s3", "cp", "-", "s3://bucket/p/segments/seg-x.json"]
                );
                assert_eq!(stdin, Some(b"payload".as_ref()));
                ok_out(b"")
            }),
        )
        .unwrap();
        t.write_segment("x", b"payload").unwrap();
    }

    #[test]
    fn cli_gs_prefers_gcloud_then_falls_back_to_gsutil() {
        // gcloud present -> gcloud; only gsutil present -> gsutil verbs (no
        // "storage" subcommand); neither -> a clear error.
        let g = CliTransport::for_remote_with(
            "gs://b/p",
            StubRunner::new(&["gcloud", "gsutil"], |_, _, _| ok_out(b"")),
        )
        .unwrap();
        assert_eq!(g.program(), "gcloud");

        let s = CliTransport::for_remote_with(
            "gs://b/p",
            StubRunner::new(&["gsutil"], |_, args, _| {
                assert_eq!(args, &["cat", "gs://b/p/base.json"]);
                err_out("no url matched")
            }),
        )
        .unwrap();
        assert_eq!(s.program(), "gsutil");
        assert_eq!(s.read_base().unwrap(), None);

        assert!(CliTransport::for_remote_with(
            "gs://b/p",
            StubRunner::new(&[], |_, _, _| ok_out(b""))
        )
        .is_err());
    }

    #[test]
    fn cli_delete_tolerates_missing() {
        let t = CliTransport::for_remote_with(
            "s3://b/p",
            StubRunner::new(&["aws"], |_, args, _| {
                assert_eq!(args, &["s3", "rm", "s3://b/p/segments/seg-x.json"]);
                err_out("delete failed: does not exist")
            }),
        )
        .unwrap();
        assert!(t.delete_segment("x").is_ok());
    }

    // ---- RealRunner (real subprocess) --------------------------------------

    #[test]
    fn program_extensions_include_bare_name() {
        // Every platform tries the bare name; Windows also tries PATHEXT
        // suffixes so `aws` matches `aws.exe` / `gcloud.cmd`.
        let exts = program_extensions();
        assert!(exts.iter().any(|e| e.is_empty()), "bare name always tried");
        if cfg!(windows) {
            assert!(exts.len() > 1, "windows adds PATHEXT candidates");
        } else {
            assert_eq!(exts, vec![String::new()]);
        }
    }

    #[cfg(unix)]
    #[test]
    fn real_runner_runs_program_feeds_stdin_and_probes_path() {
        // Resolves programs through PATH, which a sibling test may be shimming.
        let _held = crate::test_env::lock();
        let r = RealRunner;
        // program_exists: a shell is always on PATH, a nonsense name is not.
        assert!(r.program_exists("sh"));
        assert!(!r.program_exists("rstest-no-such-binary-xyz"));
        // `cat` echoes stdin to stdout (exercises the stdin-piped branch).
        let out = r.run("cat", &[], Some(b"hello")).unwrap();
        assert!(out.success);
        assert_eq!(out.stdout, b"hello");
        // A missing program is an Err from the spawn itself.
        assert!(r.run("rstest-no-such-binary-xyz", &[], None).is_err());
    }

    // ---- CliTransport construction + gcloud/gsutil verbs -------------------

    #[test]
    fn cli_for_remote_wrapper_and_construction_errors() {
        // for_remote wraps RealRunner; result depends on whether `aws` is on
        // PATH, but either way the wrapper body runs.
        let _ = CliTransport::for_remote("s3://bucket/p");
        // s3:// without the aws CLI => a clear error.
        assert!(CliTransport::for_remote_with(
            "s3://b/p",
            StubRunner::new(&[], |_, _, _| ok_out(b""))
        )
        .is_err());
        // A scheme CliTransport doesn't handle => defensive bail.
        assert!(CliTransport::for_remote_with(
            "ftp://x",
            StubRunner::new(&["aws"], |_, _, _| ok_out(b""))
        )
        .is_err());
    }

    #[test]
    fn cli_gcloud_read_write_delete_verbs() {
        let t = CliTransport::for_remote_with(
            "gs://b/p",
            StubRunner::new(&["gcloud"], |_, args, _| {
                match args.get(1).map(String::as_str) {
                    Some("cat") => ok_out(b"{}"),
                    Some("cp") | Some("rm") => ok_out(b""),
                    _ => err_out("unexpected"),
                }
            }),
        )
        .unwrap();
        // storage cat -> Found -> Some / read_segment bytes.
        assert_eq!(t.read_base().unwrap(), Some(b"{}".to_vec()));
        assert_eq!(t.read_segment("x").unwrap(), Some(b"{}".to_vec()));
        // storage cp - <url> for both base and segment.
        t.write_base(b"{}").unwrap();
        t.write_segment("x", b"data").unwrap();
        // storage rm <url>.
        t.delete_segment("x").unwrap();
    }

    #[test]
    fn cli_read_segment_missing_is_none() {
        // A not-found stderr maps a read to Missing -> Ok(None), so a puller can
        // tell a segment pruned mid-pull from a real transport error.
        let t = CliTransport::for_remote_with(
            "gs://b/p",
            StubRunner::new(&["gcloud"], |_, _, _| err_out("not found")),
        )
        .unwrap();
        assert_eq!(t.read_segment("gone").unwrap(), None);
        // A real (non not-found) failure is still an error.
        let hard = CliTransport::for_remote_with(
            "gs://b/p",
            StubRunner::new(&["gcloud"], |_, _, _| {
                err_out("Unable to locate credentials")
            }),
        )
        .unwrap();
        assert!(hard.read_segment("gone").is_err());
    }

    #[test]
    fn cli_gsutil_list_write_delete_verbs() {
        let t = CliTransport::for_remote_with(
            "gs://b/p",
            StubRunner::new(&["gsutil"], |_, args, _| {
                match args.first().map(String::as_str) {
                    Some("ls") => ok_out(b"gs://b/p/segments/seg-z.json\n"),
                    Some("cp") | Some("rm") => ok_out(b""),
                    _ => err_out("unexpected"),
                }
            }),
        )
        .unwrap();
        assert_eq!(t.program(), "gsutil");
        assert_eq!(t.list_segment_ids().unwrap(), vec!["z".to_string()]);
        t.write_segment("z", b"d").unwrap();
        t.delete_segment("z").unwrap();
    }

    #[test]
    fn cli_write_and_delete_failures_surface() {
        // A real (non not-found) failure on write or delete is an error.
        let t = CliTransport::for_remote_with(
            "s3://b/p",
            StubRunner::new(&["aws"], |_, _, _| {
                err_out("An error occurred (AccessDenied)")
            }),
        )
        .unwrap();
        assert!(t.write_segment("x", b"d").is_err());
        assert!(t.delete_segment("x").is_err());
    }
}
