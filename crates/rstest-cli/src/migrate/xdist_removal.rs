//! `xdist-removal-check`: is it safe to uninstall pytest-xdist?
//!
//! rstest never needs pytest-xdist, but removing the package has sharp edges
//! that only show up once it is gone: xdist-only flags in `addopts` become a
//! pytest usage error, `-n` / `--dist` there were never read by rstest, code
//! importing `xdist` raises `ImportError`, xdist hook implementations fail
//! plugin validation, and plugins gating on `hasplugin("xdist")` change
//! behavior. This is a static scan over the pytest config, `PYTEST_ADDOPTS`,
//! the project's Python sources, and the installed pytest plugins, with the fix
//! per finding. `--xdist-trial` adds a run with xdist hidden from the plugin
//! manager (`-p no:xdist`). That drops its options and hook specs like an
//! uninstall, but the `xdist` module stays importable, so `import xdist`
//! sites are only caught by the static scan.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::Result;
use regex::Regex;
use serde::Serialize;

use super::bisect::MAXFAIL_LIFT;
use super::check::MigrateMeta;
use super::{run_session_capture, Outcomes, Phase};
use crate::config;
use crate::reporting::sink::Sink;

/// The `--xdist-removal-json` document (schema 1). Fields are alphabetical, as
/// in the other migrate documents.
#[derive(Serialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct XdistRemovalDoc {
    /// Every finding, blocking ones first.
    pub findings: Vec<RemovalFinding>,
    /// Envelope: `kind` is `"xdist-removal-check"`.
    pub meta: MigrateMeta,
    /// Whether pytest-xdist can be uninstalled now (no non-allowed blocking
    /// finding, and the trial passed when it ran).
    pub ready: bool,
    /// The `--xdist-trial` result: `null` when the trial was not requested.
    pub trial: Option<TrialReport>,
    /// The installed pytest-xdist version: `null` when it is not installed or
    /// the interpreter could not be probed.
    pub xdist_version: Option<String>,
}

/// One thing that breaks or changes when pytest-xdist is removed.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct RemovalFinding {
    /// Whether this location is on the `--migrate-allow` list.
    pub allowed: bool,
    /// Whether it breaks the run once xdist is gone (`false`: a behavior
    /// change or a warning).
    pub blocking: bool,
    /// The suggested fix.
    pub fix: String,
    /// Finding kind: `addopts_flag`, `addopts_ignored`, `required_plugin`,
    /// `ini_key`, `import`, `hook`, `hasplugin_gate`, `plugin_import`, or
    /// `plugin_gate`.
    pub kind: String,
    /// Where it is: a config source (`pytest.ini addopts`, `PYTEST_ADDOPTS`)
    /// or `path:line`.
    pub location: String,
    /// The offending flag, key, or source line.
    pub text: String,
    /// What happens once pytest-xdist is gone.
    pub why: String,
}

/// Result of the `--xdist-trial` run with `-p no:xdist`.
#[derive(Serialize, Debug, Default)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct TrialReport {
    /// The tail of the child's stderr when the session never started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The tail of the child's stderr when pytest-xdist is installed but the
    /// run with it loaded never started, so nothing could be compared.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_error: Option<String>,
    /// Whether a run with pytest-xdist loaded was compared against (`false`
    /// when it isn't installed, so nothing can be called a regression).
    pub compared: bool,
    /// Tests that failed with xdist hidden.
    pub failed: usize,
    /// Tests that pass with pytest-xdist loaded but fail (or are no longer
    /// collected) with it hidden.
    pub regressions: Vec<String>,
    /// Whether the session with xdist hidden started and reported outcomes.
    pub started: bool,
    /// Tests that ran with xdist hidden.
    pub tests: usize,
}

impl TrialReport {
    /// A trial blocks the removal when either session didn't start or a test
    /// regressed.
    fn passed(&self) -> bool {
        self.started && self.baseline_error.is_none() && self.regressions.is_empty()
    }
}

/// What the interpreter probe reports: the pytest-xdist version and the
/// source roots of every other installed `pytest11` plugin.
#[derive(serde::Deserialize, Default)]
struct Probe {
    xdist: Option<String>,
    plugins: Vec<ProbedPlugin>,
}

#[derive(serde::Deserialize)]
struct ProbedPlugin {
    dist: String,
    /// The top-level package dir (or module file) the entry point lives in.
    root: PathBuf,
    /// The entry point's own module file, the one pytest imports at startup.
    entry: Option<PathBuf>,
}

/// Lists pytest-xdist's version and each `pytest11` entry point's top-level
/// package (dir) or module (file). Imports nothing but `find_spec`'s parents.
const PROBE: &str = r#"
import importlib.metadata as md, importlib.util as u, json, os
out = {"xdist": None, "plugins": []}
try:
    out["xdist"] = md.version("pytest-xdist")
except Exception:
    pass
seen = set()
for dist in md.distributions():
    # One broken dist (stale dist-info, missing Name) must not sink the probe.
    try:
        name = dist.metadata.get("Name") or ""
        if name.lower().replace("_", "-") in ("pytest-xdist", "rstest"):
            continue
        eps = [ep for ep in dist.entry_points if ep.group == "pytest11"]
    except Exception:
        continue
    for ep in eps:
        try:
            mod = ep.value.split(":")[0].strip()
            top = u.find_spec(mod.split(".")[0])
            if top is None or not top.origin:
                continue
            spec = u.find_spec(mod) if "." in mod else top
            entry = spec.origin if spec is not None and spec.origin else None
        except Exception:
            continue
        root = os.path.dirname(top.origin) if top.submodule_search_locations else top.origin
        if (root, entry) in seen:
            continue
        seen.add((root, entry))
        out["plugins"].append({"dist": name, "root": root, "entry": entry})
print(json.dumps(out))
"#;

/// The probed plugins grouped by package root, each scanned once: a dist can
/// list several `pytest11` entry points in one package.
fn plugin_roots<'a>(
    plugins: impl Iterator<Item = &'a ProbedPlugin>,
) -> BTreeMap<PathBuf, (String, Vec<PathBuf>)> {
    let mut roots: BTreeMap<PathBuf, (String, Vec<PathBuf>)> = BTreeMap::new();
    for p in plugins {
        let (_, entries) = roots
            .entry(p.root.clone())
            .or_insert_with(|| (p.dist.clone(), Vec::new()));
        entries.extend(p.entry.clone());
    }
    roots
}

/// Whether importing the entry module `entry` runs `file`: the module itself,
/// or the `__init__.py` of any package on its import path.
fn imported_at_startup(file: &Path, entry: &Path) -> bool {
    file == entry
        || (file.file_name().is_some_and(|n| n == "__init__.py")
            && file.parent().is_some_and(|dir| entry.starts_with(dir)))
}

fn probe(python: &Path) -> Option<Probe> {
    let out = std::process::Command::new(python)
        .args(["-c", PROBE])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

/// Run the check. Exit code: 0 = pytest-xdist can go, 1 = at least one
/// blocking finding (or a failed trial). `allow` holds location substrings
/// that are reported but don't fail the gate.
pub fn run_xdist_removal_check(
    python: &Path,
    args: &[String],
    json_path: Option<&Path>,
    allow: &[String],
    trial: bool,
    sink: &mut Sink,
) -> Result<i32> {
    let cwd = std::env::current_dir()?;
    let project = config::discover(&cwd, sink.err());
    let settings = config::rstest_settings(&cwd, sink.err());
    let root = project.rootdir.clone();
    let probed = probe(python);

    let mut findings = Vec::new();
    let ini_name = project
        .inifile
        .as_deref()
        .map(|p| rel(&root, p))
        .unwrap_or_else(|| "config".to_string());
    findings.extend(scan_addopts(
        &project.addopts,
        &format!("{ini_name} addopts"),
        settings.dist.as_deref(),
    ));
    if let Ok(env) = std::env::var("PYTEST_ADDOPTS") {
        findings.extend(scan_addopts(
            &config::shell_split(&env),
            "PYTEST_ADDOPTS",
            settings.dist.as_deref(),
        ));
    }
    findings.extend(scan_ini_keys(&project.extra, &ini_name));
    let sources: Vec<(PathBuf, String)> = project_py_files(&root)
        .into_iter()
        .filter_map(|f| std::fs::read_to_string(&f).ok().map(|t| (f, t)))
        .collect();
    // Hook names the project declares itself (`pytest_addhooks` +
    // `@pytest.hookspec`): implementing those doesn't need pytest-xdist.
    let own_specs: BTreeSet<String> = sources.iter().flat_map(|(_, t)| hook_specs(t)).collect();
    for (file, text) in &sources {
        let name = file.file_name().and_then(|n| n.to_str()).unwrap_or("");
        // pytest registers conftests and plugin modules, never test modules,
        // so a hook-named def in a test module is inert.
        let test_module = name != "conftest.py"
            && project
                .python_files
                .iter()
                .any(|pat| crate::collect::glob_match(pat, name));
        findings.extend(scan_source(
            text,
            &rel(&root, file),
            &own_specs,
            test_module,
        ));
    }
    let xdist_version = probed.as_ref().and_then(|p| p.xdist.clone());
    for (root, (dist, entries)) in plugin_roots(probed.iter().flat_map(|p| &p.plugins)) {
        for file in plugin_py_files(&root) {
            if let Ok(text) = std::fs::read_to_string(&file) {
                let is_entry = entries.iter().any(|e| imported_at_startup(&file, e));
                findings.extend(scan_plugin_source(
                    &text,
                    &dist,
                    &file.display().to_string(),
                    is_entry,
                ));
            }
        }
    }
    for f in &mut findings {
        f.allowed = allow.iter().any(|p| f.location.contains(p.as_str()));
    }
    // Blocking first, then in scan order (config, sources, plugins).
    findings.sort_by_key(|f| !f.blocking);

    sink.out_line(&match (&probed, &xdist_version) {
        (None, _) => {
            "pytest-xdist: could not probe the interpreter; scanning config and sources only"
                .to_string()
        }
        (Some(_), Some(v)) => format!("pytest-xdist: {v} installed"),
        (Some(_), None) => {
            "pytest-xdist: not installed (the findings below still break or change a run)"
                .to_string()
        }
    });
    render(&findings, sink);

    let trial_report = if trial {
        // An unprobed interpreter may well have xdist: run the baseline then too.
        Some(run_trial(
            python,
            args,
            xdist_version.is_some() || probed.is_none(),
            sink,
        )?)
    } else {
        None
    };

    let blocking = findings.iter().filter(|f| f.blocking && !f.allowed).count();
    let ready = blocking == 0 && trial_report.as_ref().is_none_or(TrialReport::passed);
    if ready {
        sink.out_line(match (&probed, &xdist_version) {
            (None, _) => {
                "==> ready: no blocking findings in config and sources (installed plugins \
                 were not checked: the interpreter probe failed)."
            }
            (Some(_), Some(_)) => {
                "==> ready: uninstall pytest-xdist and drop it from your dependency lists."
            }
            (Some(_), None) => "==> ready: nothing depends on pytest-xdist.",
        });
    } else {
        sink.out_line(&format!(
            "==> not ready: {blocking} blocking finding(s){}. Fix them before uninstalling \
             pytest-xdist.",
            if trial_report.as_ref().is_some_and(|t| !t.passed()) {
                " and a failing trial"
            } else {
                ""
            }
        ));
    }
    if !trial && ready && xdist_version.is_some() {
        sink.out_line("    Prove it first: `rstest xdist-removal-check --xdist-trial` runs the suite with xdist hidden.");
    }

    if let Some(path) = json_path {
        let doc = XdistRemovalDoc {
            findings,
            meta: MigrateMeta {
                kind: "xdist-removal-check".to_string(),
                runner: "rstest".to_string(),
                schema: 1,
            },
            ready,
            trial: trial_report,
            xdist_version,
        };
        std::fs::write(path, serde_json::to_string_pretty(&doc)?)?;
    }
    Ok(if ready { 0 } else { 1 })
}

/// The human report: findings grouped by kind, one fix line each.
fn render(findings: &[RemovalFinding], sink: &mut Sink) {
    if findings.is_empty() {
        sink.out_line("  no findings: config, sources and plugins don't depend on pytest-xdist.\n");
        return;
    }
    let blocking = findings.iter().filter(|f| f.blocking && !f.allowed).count();
    let allowed = findings.iter().filter(|f| f.allowed).count();
    let allowed_note = if allowed > 0 {
        format!(", {allowed} allowed")
    } else {
        String::new()
    };
    sink.out_line(&format!(
        "  {} finding(s), {blocking} blocking{allowed_note}:\n",
        findings.len()
    ));
    for f in findings {
        let tag = match (f.blocking, f.allowed) {
            (_, true) => "ALLOWED",
            (true, false) => "BLOCKS",
            (false, false) => "CHANGES",
        };
        let mut text = f.text.clone();
        crate::text::truncate_on_boundary(&mut text, 90);
        sink.out_line(&format!("  [{tag}] {}: {text}", f.location));
        sink.out_line(&format!("    {}", f.why));
        sink.out_line(&format!("    FIX: {}\n", f.fix));
    }
}

/// Run the suite with xdist hidden (`-p no:xdist`, which drops its options and
/// hook specs as an uninstall does, though `import xdist` still succeeds) and,
/// when xdist is (or may be) installed, once
/// more with it loaded, to name the tests that only pass with it.
fn run_trial(
    python: &Path,
    args: &[String],
    xdist_installed: bool,
    sink: &mut Sink,
) -> Result<TrialReport> {
    let base: Vec<String> = args
        .iter()
        .cloned()
        .chain([MAXFAIL_LIFT.to_string()])
        .collect();
    let (mut baseline, baseline_stderr) = if xdist_installed {
        sink.warn("rstest xdist-removal-check: running the suite with pytest-xdist loaded…");
        run_session_capture(python, &[], &base)?
    } else {
        (None, String::new())
    };
    let baseline_error = baseline_failure(xdist_installed, baseline.as_ref(), &baseline_stderr);
    if baseline_error.is_some() {
        // Nothing to compare against: don't treat an empty report as a baseline.
        baseline = None;
    }
    sink.warn(
        "rstest xdist-removal-check: running the suite with pytest-xdist hidden (-p no:xdist)…",
    );
    let hidden_args: Vec<String> = base
        .iter()
        .cloned()
        .chain(["-p".to_string(), "no:xdist".to_string()])
        .collect();
    let (hidden, stderr) = run_session_capture(python, &[], &hidden_args)?;
    let mut report = trial_report(baseline.as_ref(), hidden.as_ref(), &stderr);
    report.baseline_error = baseline_error;
    render_trial(&report, sink);
    Ok(report)
}

/// The stderr tail when pytest-xdist is installed but the run with it loaded
/// didn't start: no report at all, or an empty one next to an error (a usage
/// error or INTERNALERROR still writes a report, with no tests).
fn baseline_failure(
    xdist_installed: bool,
    baseline: Option<&Outcomes>,
    stderr: &str,
) -> Option<String> {
    let failed =
        baseline.is_none_or(|b| b.is_empty() && stderr.to_ascii_lowercase().contains("error"));
    (xdist_installed && failed).then(|| stderr_tail(stderr, 12))
}

/// Compare the two trial runs. `baseline` is `None` when xdist isn't
/// installed (or that run didn't start): then only the hidden run's own
/// failures are counted, with nothing to call a regression.
fn trial_report(
    baseline: Option<&Outcomes>,
    hidden: Option<&Outcomes>,
    stderr: &str,
) -> TrialReport {
    // A pytest usage error still writes a report, with no tests: treat an
    // empty run as not started when the baseline had tests or pytest errored.
    let hidden = hidden.filter(|h| {
        !h.is_empty()
            || !(baseline.is_some_and(|b| !b.is_empty())
                || stderr.to_ascii_lowercase().contains("error"))
    });
    let Some(hidden) = hidden else {
        return TrialReport {
            error: Some(stderr_tail(stderr, 12)),
            ..TrialReport::default()
        };
    };
    let regressions = baseline
        .into_iter()
        .flatten()
        .filter(|(id, rec)| {
            rec.phase == Phase::Pass && hidden.get(*id).is_none_or(|h| h.phase == Phase::Fail)
        })
        .map(|(id, _)| id.clone())
        .collect();
    TrialReport {
        baseline_error: None,
        compared: baseline.is_some(),
        error: None,
        failed: hidden.values().filter(|r| r.phase == Phase::Fail).count(),
        regressions,
        started: true,
        tests: hidden.len(),
    }
}

fn render_trial(t: &TrialReport, sink: &mut Sink) {
    if let Some(err) = &t.baseline_error {
        sink.out_line(
            "TRIAL: the session did not start with pytest-xdist loaded, so nothing was compared:",
        );
        for line in err.lines() {
            sink.out_line(&format!("    {line}"));
        }
    }
    if !t.started {
        sink.out_line("TRIAL: the session did not start with pytest-xdist hidden:");
        for line in t.error.as_deref().unwrap_or("").lines() {
            sink.out_line(&format!("    {line}"));
        }
        return;
    }
    if t.regressions.is_empty() {
        let note = match (t.failed, t.compared) {
            (0, _) => String::new(),
            (n, true) => format!(" ({n} failed, and fail with pytest-xdist loaded too)"),
            (n, false) if t.baseline_error.is_some() => format!(" ({n} failed)"),
            (n, false) => {
                format!(" ({n} failed; pytest-xdist isn't installed, nothing to compare)")
            }
        };
        sink.out_line(&format!(
            "TRIAL: {} test(s) ran with pytest-xdist hidden, none regressed{note}.",
            t.tests
        ));
        return;
    }
    sink.out_line(&format!(
        "TRIAL: {} test(s) pass with pytest-xdist loaded but fail without it:",
        t.regressions.len()
    ));
    for id in t.regressions.iter().take(10) {
        sink.out_line(&format!("    {id}"));
    }
    if t.regressions.len() > 10 {
        sink.out_line(&format!("    … and {} more", t.regressions.len() - 10));
    }
}

/// The last `n` non-blank lines of a child's stderr.
fn stderr_tail(stderr: &str, n: usize) -> String {
    let lines: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).display().to_string()
}

/// Project `.py` files, pruning caches, VCS dirs, virtualenvs and hidden dirs
/// (the same walk shape as the `--changed` import graph).
fn project_py_files(root: &Path) -> Vec<PathBuf> {
    let walker = ignore::WalkBuilder::new(root)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .filter_entry(|e| {
            let n = e.file_name().to_str().unwrap_or("");
            !matches!(n, "__pycache__" | ".git" | "node_modules" | "site-packages")
                && !e.path().join("pyvenv.cfg").exists()
        })
        .build();
    walker
        .flatten()
        .filter(|e| {
            e.file_type().is_some_and(|t| t.is_file())
                && e.path().extension().and_then(|x| x.to_str()) == Some("py")
        })
        .map(ignore::DirEntry::into_path)
        .collect()
}

/// An installed plugin's `.py` files: the module itself, or its package tree.
fn plugin_py_files(root: &Path) -> Vec<PathBuf> {
    if root.is_file() {
        return vec![root.to_path_buf()];
    }
    ignore::WalkBuilder::new(root)
        .standard_filters(false)
        .build()
        .flatten()
        .filter(|e| {
            e.file_type().is_some_and(|t| t.is_file())
                && e.path().extension().and_then(|x| x.to_str()) == Some("py")
        })
        .map(ignore::DirEntry::into_path)
        .collect()
}

/// pytest-xdist options that only parse while the plugin is loaded. Each is
/// `(flag, takes_value, fix)`; `-n` / `--dist` / `-p xdist` are handled apart.
const XDIST_ONLY: &[(&str, bool, &str)] = &[
    (
        "-d",
        false,
        "drop it: it is xdist's `--dist load` shorthand, which is rstest's default",
    ),
    (
        "--maxprocesses",
        true,
        "drop it: rstest has no separate cap, pass `-n` instead",
    ),
    (
        "--max-worker-restart",
        true,
        "drop it: rstest respawns crashed workers on a fixed budget",
    ),
    (
        "--max-slave-restart",
        true,
        "drop it: rstest respawns crashed workers on a fixed budget",
    ),
    (
        "--tx",
        true,
        "drop it: rstest runs local workers only, no execnet gateways",
    ),
    (
        "--px",
        true,
        "drop it: rstest runs local workers only, no execnet gateways",
    ),
    (
        "--rsyncdir",
        true,
        "drop it: rstest runs local workers, nothing to sync",
    ),
    (
        "--rsyncignore",
        true,
        "drop it: rstest runs local workers, nothing to sync",
    ),
    (
        "--testrunuid",
        true,
        "drop it: rstest generates the run uid (the `testrun_uid` fixture)",
    ),
    (
        "--maxschedchunk",
        true,
        "drop it: rstest's scheduler has no chunk setting",
    ),
    (
        "--loadscope-reorder",
        false,
        "drop it: rstest's scheduler has no such setting",
    ),
    (
        "--no-loadscope-reorder",
        false,
        "drop it: rstest's scheduler has no such setting",
    ),
    ("-f", false, "drop it and use `rstest --watch` instead"),
    (
        "--looponfail",
        false,
        "drop it and use `rstest --watch` instead",
    ),
];

const GONE: &str = "a pytest usage error (exit 4, `unrecognized arguments`) once pytest-xdist \
                    is gone; while installed it parses but does nothing under rstest";

/// Scan one `addopts` source (ini `addopts` or `PYTEST_ADDOPTS`) for xdist
/// options. `tool_dist` is `[tool.rstest] dist`, to tell a mode that is
/// already carried over from one that is silently lost.
fn scan_addopts(tokens: &[String], source: &str, tool_dist: Option<&str>) -> Vec<RemovalFinding> {
    let finding = |kind: &str, text: String, why: &str, fix: String| RemovalFinding {
        allowed: false,
        blocking: true,
        fix,
        kind: kind.to_string(),
        location: source.to_string(),
        text,
        why: why.to_string(),
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i].as_str();
        let next = tokens.get(i + 1).map(String::as_str);
        // (flag, value, tokens consumed) for a value flag in `--f v`, `--f=v`,
        // or (short flags only) attached `-fv` form.
        let value_of = |flag: &str| -> Option<(Option<&str>, usize)> {
            if tok == flag {
                return Some((next, if next.is_some() { 2 } else { 1 }));
            }
            let rest = tok.strip_prefix(flag)?;
            if let Some(v) = rest.strip_prefix('=') {
                return Some((Some(v), 1));
            }
            (flag.len() == 2 && !rest.is_empty()).then_some((Some(rest), 1))
        };
        if let Some((v, used)) = value_of("-n").or_else(|| value_of("--numprocesses")) {
            let v = v.unwrap_or("auto");
            let key = if v.parse::<u32>().is_ok() {
                v.to_string()
            } else {
                format!("\"{v}\"")
            };
            out.push(finding(
                "addopts_ignored",
                tokens[i..i + used].join(" "),
                "rstest never reads `-n` from addopts (it picks the worker count itself), and \
                 it becomes a pytest usage error (exit 4) once pytest-xdist is gone",
                format!(
                    "drop it; pass `rstest -n {v}` or set `[tool.rstest] numprocesses = {key}`"
                ),
            ));
            i += used;
            continue;
        }
        if let Some((v, used)) = value_of("--dist") {
            let v = v.unwrap_or("");
            let fix = match v {
                "no" => "drop it; use `rstest -n 0` for a single process".to_string(),
                "worksteal" => {
                    "drop it; rstest's default `load` already dispatches slowest-first".to_string()
                }
                _ if tool_dist == Some(v) => {
                    format!("drop it: `[tool.rstest] dist = \"{v}\"` already carries the mode")
                }
                _ => format!("drop it and set `[tool.rstest] dist = \"{v}\"`"),
            };
            let why = if matches!(v, "loadgroup" | "loadscope" | "loadfile") && tool_dist != Some(v)
            {
                "rstest never reads `--dist` from addopts, so this run already uses `load` and \
                 loses the grouping (`xdist_group` / scope / file co-location) with no warning; \
                 once pytest-xdist is gone it is a pytest usage error (exit 4)"
            } else {
                "rstest never reads `--dist` from addopts, and it becomes a pytest usage error \
                 (exit 4) once pytest-xdist is gone"
            };
            out.push(finding(
                "addopts_ignored",
                tokens[i..i + used].join(" "),
                why,
                fix,
            ));
            i += used;
            continue;
        }
        if let Some((Some(v), used)) = value_of("-p") {
            if v == "xdist" || v.starts_with("xdist.") {
                out.push(finding(
                    "addopts_flag",
                    tokens[i..i + used].join(" "),
                    "pytest can't load the plugin once pytest-xdist is gone, and stops before \
                     collecting",
                    "drop it".to_string(),
                ));
            }
            i += used;
            continue;
        }
        let mut matched = false;
        for (flag, takes_value, fix) in XDIST_ONLY {
            let hit = if *takes_value {
                value_of(flag).map(|(_, used)| used)
            } else {
                (tok == *flag).then_some(1)
            };
            if let Some(used) = hit {
                out.push(finding(
                    "addopts_flag",
                    tokens[i..i + used].join(" "),
                    GONE,
                    (*fix).to_string(),
                ));
                i += used;
                matched = true;
                break;
            }
        }
        if !matched {
            i += 1;
        }
    }
    out
}

/// Scan the rest of the pytest config section: `required_plugins` naming
/// pytest-xdist, and xdist's own ini keys.
fn scan_ini_keys(extra: &[(String, Vec<String>)], ini_name: &str) -> Vec<RemovalFinding> {
    let mut out = Vec::new();
    for (key, values) in extra {
        match key.as_str() {
            "required_plugins" => {
                for v in values {
                    let name = v.split(['<', '>', '=', '!', '~']).next().unwrap_or("");
                    if name.eq_ignore_ascii_case("pytest-xdist") {
                        out.push(RemovalFinding {
                            allowed: false,
                            blocking: true,
                            fix: "remove `pytest-xdist` from `required_plugins`".to_string(),
                            kind: "required_plugin".to_string(),
                            location: format!("{ini_name} required_plugins"),
                            text: v.clone(),
                            why: "pytest refuses to start (`Missing required plugins: \
                                  pytest-xdist`) once it is gone"
                                .to_string(),
                        });
                    }
                }
            }
            "rsyncdirs" | "rsyncignore" | "looponfailroots" => out.push(RemovalFinding {
                allowed: false,
                blocking: false,
                fix: format!("remove `{key}`: rstest has no equivalent"),
                kind: "ini_key".to_string(),
                location: format!("{ini_name} {key}"),
                text: format!("{key} = {}", values.join(" ")),
                why: "an unknown ini key once pytest-xdist is gone: a PytestConfigWarning, and \
                      an error under `--strict-config`"
                    .to_string(),
            }),
            _ => {}
        }
    }
    out
}

fn re_import() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `xdist` anywhere in an `import a, b as c, ...` list, or `from xdist[...] import`.
        Regex::new(
            r"^\s*(?:import\s+(?:[\w.]+(?:\s+as\s+\w+)?\s*,\s*)*xdist\b|from\s+xdist(?:\.[\w.]+)?\s+import\b)",
        )
        .unwrap()
    })
}

fn re_hasplugin() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"\b(?:has_?plugin|getplugin)\(\s*["']xdist["']\s*\)"#).unwrap())
}

fn re_hook() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^\s*(?:async\s+)?def\s+(pytest_configure_node|pytest_testnodeready|pytest_testnodedown|pytest_handlecrashitem|pytest_xdist_\w+)\s*\(",
        )
        .unwrap()
    })
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Whether any block enclosing line `idx` satisfies `pred` (given the block's
/// header, trimmed). Enclosing blocks are the earlier code lines with strictly
/// less indentation, walking outward.
fn enclosed_by(lines: &[&str], idx: usize, mut pred: impl FnMut(&str) -> bool) -> bool {
    let mut level = indent(lines[idx]);
    for line in lines[..idx].iter().rev() {
        let code = line.trim();
        if level == 0 {
            break;
        }
        if code.is_empty() || code.starts_with('#') || indent(line) >= level {
            continue;
        }
        level = indent(line);
        if pred(code) {
            return true;
        }
    }
    false
}

/// The condition of an `if` / `elif` header.
fn if_cond(code: &str) -> Option<&str> {
    code.strip_prefix("if ")
        .or_else(|| code.strip_prefix("elif "))
        .and_then(|c| c.strip_suffix(':'))
}

/// An import is guarded when any enclosing block is a `try:` (with an
/// `except ImportError` presumably below), or an `if`/`elif` that only runs
/// under a type checker or while xdist is loaded (see [`and_chain_requires`]).
fn import_guarded(lines: &[&str], idx: usize) -> bool {
    enclosed_by(lines, idx, |code| {
        code == "try:"
            || if_cond(code).is_some_and(|c| {
                and_chain_requires(c, re_type_checking()) || and_chain_requires(c, re_xdist_gate())
            })
    })
}

/// A block that only runs while xdist is loaded.
fn xdist_gated(lines: &[&str], idx: usize) -> bool {
    enclosed_by(lines, idx, |code| {
        if_cond(code).is_some_and(|c| and_chain_requires(c, re_xdist_gate()))
    })
}

fn re_type_checking() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(?:\w+\.)*TYPE_CHECKING$").unwrap())
}

/// `hasplugin("xdist")` (a bool) or `getplugin("xdist")`, which may also be
/// compared `is not None`.
fn re_xdist_gate() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"^[\w.]*\b(?:has_?plugin\(\s*["']xdist["']\s*\)|getplugin\(\s*["']xdist["']\s*\)(?:\s+is\s+not\s+None)?)$"#,
        )
        .unwrap()
    })
}

/// Whether an `if` condition is true only when `term` holds: an `and` chain
/// with a bare `term` in it. Any `or`, and negated or compared terms, don't
/// count: their body can still run when `term` is false.
fn and_chain_requires(cond: &str, term: &Regex) -> bool {
    let mut cond = cond.trim();
    while let Some(inner) = cond.strip_prefix('(').and_then(|c| c.strip_suffix(')')) {
        cond = inner.trim();
    }
    if cond.split_whitespace().any(|w| w == "or") {
        return false;
    }
    cond.split(" and ").any(|t| term.is_match(t.trim()))
}

/// The decorator block right above the `def` at `idx`: stacked `@...` lines,
/// multi-line ones included (an `@` line counts once the text from it down to
/// the `def` has balanced parens). Stops at a blank line, another `def` or
/// `class`, or 20 lines up, so a previous function's body never counts.
fn decorators(lines: &[&str], idx: usize) -> String {
    let mut start = idx;
    for k in (idx.saturating_sub(20)..idx).rev() {
        let l = lines[k].trim();
        if l.is_empty()
            || l.starts_with("def ")
            || l.starts_with("async def ")
            || l.starts_with("class ")
        {
            break;
        }
        if l.starts_with('@') {
            let block = lines[k..idx].join("\n");
            if block.matches('(').count() == block.matches(')').count() {
                start = k;
            }
        }
    }
    lines[start..idx].join("\n")
}

fn re_optional() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\boptionalhook\s*=\s*True\b").unwrap())
}

fn re_hookspec() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"@\s*(?:\w+\.)*hookspec\b").unwrap())
}

/// The xdist hook names a file declares as its own specs (`@pytest.hookspec`).
fn hook_specs(text: &str) -> Vec<String> {
    if !text.contains("hookspec") {
        return Vec::new();
    }
    let lines: Vec<&str> = text.lines().collect();
    lines
        .iter()
        .enumerate()
        .filter_map(|(idx, l)| {
            let m = re_hook().captures(l)?;
            re_hookspec()
                .is_match(&decorators(&lines, idx))
                .then(|| m[1].to_string())
        })
        .collect()
}

/// The replacement for the xdist helpers one import line brings in. A
/// `from xdist[.mod] import a, b` line names its helpers itself; for
/// `import xdist[.mod] [as x]` the file is searched for `x.<helper>` (the
/// helpers live in `xdist.plugin` and are re-exported by `xdist`). Without a
/// helper, the advice depends on the module imported.
fn import_fix(line: &str, text: &str) -> String {
    const HELPERS: &[(&[&str], &str)] = &[
        (
            &["get_xdist_worker_id"],
            "`get_xdist_worker_id(...)` -> the `worker_id` fixture, or \
             `os.environ.get(\"PYTEST_XDIST_WORKER\", \"master\")`",
        ),
        (
            &["is_xdist_worker"],
            "`is_xdist_worker(request)` -> `hasattr(request.config, \"workerinput\")`",
        ),
        (
            &["is_xdist_controller", "is_xdist_master"],
            "`is_xdist_controller(...)` -> `not hasattr(config, \"workerinput\")` (rstest has \
             no controller process at -n >= 2)",
        ),
    ];
    let code = line.split('#').next().unwrap_or("").trim();
    // The module the line imports, and the names it brings in (a `from`
    // import) or the name it binds the module to (`import ... [as x]`).
    let (module, names, bound) = if let Some(rest) = code.strip_prefix("from ") {
        let (module, names) = rest.split_once(" import ").unwrap_or((rest, ""));
        let names: Vec<&str> = names
            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .filter(|n| !n.is_empty())
            .collect();
        (module.trim(), names, None)
    } else {
        let entry = code
            .strip_prefix("import")
            .unwrap_or(code)
            .split(',')
            .map(str::trim)
            .find(|e| e.split_whitespace().next().is_some_and(is_xdist_module))
            .unwrap_or("xdist");
        let mut parts = entry.split_whitespace();
        let module = parts.next().unwrap_or("xdist");
        let bound = match (parts.next(), parts.next()) {
            (Some("as"), Some(alias)) => alias,
            _ => "xdist",
        };
        (module, Vec::new(), Some(bound))
    };
    // A `from ... import (` list continues past this line: fall back to the file.
    let continued = code.ends_with('(') || code.ends_with('\\');
    let uses = |h: &str| match bound {
        None if continued => text.contains(h),
        None => names.contains(&h),
        Some(b) => text.contains(&format!("{b}.{h}")) || text.contains(&format!("{b}.plugin.{h}")),
    };
    let fixes: Vec<&str> = HELPERS
        .iter()
        .filter(|(helpers, _)| helpers.iter().any(|h| uses(h)))
        .map(|(_, fix)| *fix)
        .collect();
    if !fixes.is_empty() {
        return fixes.join("; ");
    }
    match module {
        "xdist" => "use the native `worker_id` / `testrun_uid` fixtures or the `PYTEST_XDIST_*` \
                    env, or guard the import with `try:` / `except ImportError`"
            .to_string(),
        "xdist.plugin" => "`xdist.plugin` is pytest-xdist's own plugin module (its options and \
                           fixtures): rstest provides `-n`/`--dist` and the `worker_id` / \
                           `testrun_uid` fixtures natively, so drop the import, or guard it \
                           with `try:` / `except ImportError`"
            .to_string(),
        other => format!(
            "`{other}` is pytest-xdist internals with no rstest equivalent: drop the code that \
             uses it, or guard the import with `try:` / `except ImportError`"
        ),
    }
}

/// `xdist` or one of its submodules (`xdist.plugin`), not `xdistlike`.
fn is_xdist_module(m: &str) -> bool {
    m == "xdist" || m.starts_with("xdist.")
}

/// The name of the class whose body line `idx` is in, if any.
fn enclosing_class(lines: &[&str], idx: usize) -> Option<String> {
    let mut found = None;
    enclosed_by(lines, idx, |code| {
        found = code
            .strip_prefix("class ")
            .and_then(|rest| {
                rest.split(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .next()
            })
            .filter(|n| !n.is_empty())
            .map(str::to_string);
        found.is_some()
    });
    found
}

#[derive(PartialEq, Debug)]
enum Registration {
    /// Every `register(Class...)` in the file sits under an xdist gate.
    Gated,
    /// Some `register(Class...)` runs without xdist.
    Ungated,
    /// The file never registers the class.
    NotFound,
}

/// How a hook class is registered with the plugin manager in its own file.
fn registration(lines: &[&str], class: &str) -> Registration {
    let re = Regex::new(&format!(r"\bregister\(\s*{}\b", regex::escape(class))).unwrap();
    let mut seen = false;
    for (idx, l) in lines.iter().enumerate() {
        if l.trim_start().starts_with('#') || !re.is_match(l) {
            continue;
        }
        if !xdist_gated(lines, idx) {
            return Registration::Ungated;
        }
        seen = true;
    }
    if seen {
        Registration::Gated
    } else {
        Registration::NotFound
    }
}

/// Scan one project source file: unguarded `xdist` imports, xdist hook
/// implementations not marked optional, and `hasplugin("xdist")` gates.
///
/// `own_specs` are hook names the project declares itself (their
/// implementations don't depend on xdist); `test_module` skips the hook check,
/// since pytest never registers a test module as a plugin.
fn scan_source(
    text: &str,
    rel_path: &str,
    own_specs: &BTreeSet<String>,
    test_module: bool,
) -> Vec<RemovalFinding> {
    if !text.contains("xdist")
        && !text.contains("pytest_configure_node")
        && !text.contains("pytest_testnode")
        && !text.contains("pytest_handlecrashitem")
    {
        return Vec::new();
    }
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        let at = format!("{rel_path}:{}", idx + 1);
        let snippet = line.trim().to_string();
        if re_import().is_match(line) && !import_guarded(&lines, idx) {
            out.push(RemovalFinding {
                allowed: false,
                blocking: true,
                fix: import_fix(line, text),
                kind: "import".to_string(),
                location: at,
                text: snippet,
                why: "raises ImportError once pytest-xdist is gone".to_string(),
            });
        } else if let Some(m) = re_hook().captures(line) {
            let name = &m[1];
            let deco = decorators(&lines, idx);
            if test_module
                || own_specs.contains(name)
                || re_optional().is_match(&deco)
                || re_hookspec().is_match(&deco)
                || xdist_gated(&lines, idx)
            {
                continue;
            }
            let registration = enclosing_class(&lines, idx).map(|c| registration(&lines, &c));
            if registration == Some(Registration::Gated) {
                continue;
            }
            if let Some(Registration::NotFound) = registration {
                out.push(RemovalFinding {
                    allowed: false,
                    blocking: false,
                    fix: "register the class only under `if config.pluginmanager.hasplugin(\"xdist\"):`, \
                          or mark the hook `@pytest.hookimpl(optionalhook=True)`"
                        .to_string(),
                    kind: "hook".to_string(),
                    location: at,
                    text: snippet,
                    why: format!(
                        "a method of a class registered outside this file: if it is registered \
                         without an xdist gate, every run stops with `PluginValidationError: \
                         unknown hook '{name}'` once pytest-xdist is gone"
                    ),
                });
                continue;
            }
            let emulated = matches!(
                name,
                "pytest_configure_node" | "pytest_testnodeready" | "pytest_testnodedown"
            );
            out.push(RemovalFinding {
                allowed: false,
                blocking: true,
                fix: if emulated {
                    "mark it `@pytest.hookimpl(optionalhook=True)`: rstest still calls it \
                     (emulated), with or without pytest-xdist"
                        .to_string()
                } else {
                    format!(
                        "delete it (rstest never calls `{name}`), or mark it \
                         `@pytest.hookimpl(optionalhook=True)`"
                    )
                },
                kind: "hook".to_string(),
                location: at,
                text: snippet,
                why: format!(
                    "every run stops with `PluginValidationError: unknown hook '{name}'` once \
                     pytest-xdist is gone (the hook spec is xdist's)"
                ),
            });
        } else if re_hasplugin().is_match(line) {
            out.push(RemovalFinding {
                allowed: false,
                blocking: false,
                fix: "gate on the worker itself instead: `hasattr(config, \"workerinput\")` or \
                      `os.environ.get(\"PYTEST_XDIST_WORKER\")`, which rstest sets with or \
                      without pytest-xdist"
                    .to_string(),
                kind: "hasplugin_gate".to_string(),
                location: at,
                text: snippet,
                why: "always false once pytest-xdist is gone, so this branch changes".to_string(),
            });
        }
    }
    out
}

/// Scan one installed plugin's source: `hasplugin("xdist")` gates (a behavior
/// change) and unguarded module-level `xdist` imports. The trial can't catch
/// the latter, since `-p no:xdist` leaves the module importable. `is_entry`
/// marks the entry-point module pytest imports at startup, where such an
/// import stops every run; elsewhere in the package it breaks only when that
/// module is imported. Imports inside functions and hook implementations are
/// left alone: plugins commonly reach them only behind an xdist gate.
fn scan_plugin_source(text: &str, dist: &str, path: &str, is_entry: bool) -> Vec<RemovalFinding> {
    if !text.contains("xdist") {
        return Vec::new();
    }
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    for (idx, l) in lines.iter().enumerate() {
        let location = format!("{dist} ({path}:{})", idx + 1);
        if re_import().is_match(l) && indent(l) == 0 {
            out.push(RemovalFinding {
                allowed: false,
                blocking: is_entry,
                fix: format!(
                    "upgrade or drop {dist}, or keep pytest-xdist installed: it imports xdist \
                     unconditionally"
                ),
                kind: "plugin_import".to_string(),
                location,
                text: l.trim().to_string(),
                why: if is_entry {
                    format!(
                        "{dist}'s plugin module raises ImportError at pytest startup once \
                         pytest-xdist is gone, so every run stops"
                    )
                } else {
                    format!(
                        "raises ImportError once pytest-xdist is gone, whenever {dist} \
                         imports this module"
                    )
                },
            });
        } else if re_hasplugin().is_match(l) {
            out.push(RemovalFinding {
                allowed: false,
                blocking: false,
                fix: format!(
                    "check {dist}'s behavior without pytest-xdist (see the rstest plugin \
                     compatibility list); under rstest pool workers it now takes its \
                     non-xdist path"
                ),
                kind: "plugin_gate".to_string(),
                location,
                text: l.trim().to_string(),
                why: format!(
                    "{dist} switches behavior on `hasplugin(\"xdist\")`, which turns false"
                ),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::migrate::Rec;

    /// [`scan_source`] with no project-declared specs, as a non-test module.
    fn scan(text: &str, rel_path: &str) -> Vec<RemovalFinding> {
        scan_source(text, rel_path, &BTreeSet::new(), false)
    }

    /// Count findings per kind.
    fn kinds(findings: &[RemovalFinding]) -> BTreeMap<&str, usize> {
        let mut m = BTreeMap::new();
        for f in findings {
            *m.entry(f.kind.as_str()).or_insert(0) += 1;
        }
        m
    }

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn addopts_n_and_dist_are_ignored_and_break_after_removal() {
        let f = scan_addopts(
            &v(&["-n", "4", "--dist", "loadgroup", "-q"]),
            "pytest.ini addopts",
            None,
        );
        assert_eq!(f.len(), 2, "{f:?}");
        assert!(f.iter().all(|f| f.blocking && f.kind == "addopts_ignored"));
        assert_eq!(f[0].text, "-n 4");
        assert!(f[0].fix.contains("numprocesses = 4"), "{}", f[0].fix);
        assert_eq!(f[1].text, "--dist loadgroup");
        assert!(f[1].fix.contains("dist = \"loadgroup\""));
        assert!(f[1].why.contains("xdist_group"), "grouping loss is named");
    }

    #[test]
    fn addopts_attached_and_eq_forms() {
        let f = scan_addopts(
            &v(&["-nauto", "--dist=loadfile", "--numprocesses=2"]),
            "s",
            None,
        );
        let texts: Vec<&str> = f.iter().map(|f| f.text.as_str()).collect();
        assert_eq!(texts, ["-nauto", "--dist=loadfile", "--numprocesses=2"]);
        assert!(f[0].fix.contains("numprocesses = \"auto\""));
    }

    #[test]
    fn addopts_dist_already_in_tool_rstest() {
        let f = scan_addopts(&v(&["--dist", "loadgroup"]), "s", Some("loadgroup"));
        assert!(f[0].fix.contains("already carries"), "{}", f[0].fix);
        assert!(!f[0].why.contains("xdist_group"), "nothing is lost today");
    }

    #[test]
    fn addopts_dist_no_and_worksteal_fixes() {
        let f = scan_addopts(&v(&["--dist=no"]), "s", None);
        assert!(f[0].fix.contains("-n 0"));
        let f = scan_addopts(&v(&["--dist", "worksteal"]), "s", None);
        assert!(f[0].fix.contains("slowest-first"));
    }

    #[test]
    fn addopts_xdist_only_flags() {
        let f = scan_addopts(
            &v(&[
                "--tx",
                "3*popen",
                "--rsyncdir=src",
                "-d",
                "--maxprocesses",
                "8",
                "--looponfail",
                "-f",
                "--max-worker-restart=2",
                "-v",
            ]),
            "s",
            None,
        );
        let texts: Vec<&str> = f.iter().map(|f| f.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "--tx 3*popen",
                "--rsyncdir=src",
                "-d",
                "--maxprocesses 8",
                "--looponfail",
                "-f",
                "--max-worker-restart=2"
            ]
        );
        assert!(f.iter().all(|f| f.blocking && f.kind == "addopts_flag"));
        assert!(f[4].fix.contains("--watch"));
    }

    #[test]
    fn addopts_plugin_loads() {
        let f = scan_addopts(
            &v(&[
                "-p",
                "xdist.looponfail",
                "-p",
                "no:xdist",
                "-pxdist",
                "-p",
                "cov",
            ]),
            "s",
            None,
        );
        let texts: Vec<&str> = f.iter().map(|f| f.text.as_str()).collect();
        assert_eq!(
            texts,
            ["-p xdist.looponfail", "-pxdist"],
            "`no:xdist` is fine"
        );
    }

    #[test]
    fn addopts_clean_has_no_findings() {
        // `--durations` shares no prefix with a table entry; `-p` values that
        // aren't xdist are skipped with their value.
        let f = scan_addopts(
            &v(&["-q", "--durations=5", "-p", "-d", "-ra", "--strict-markers"]),
            "s",
            None,
        );
        assert!(f.is_empty(), "{f:?}");
    }

    #[test]
    fn ini_required_plugins_and_xdist_keys() {
        let extra = vec![
            (
                "required_plugins".to_string(),
                v(&["pytest-cov", "pytest-xdist>=3"]),
            ),
            ("rsyncdirs".to_string(), v(&["src", "tests"])),
            ("markers".to_string(), v(&["slow"])),
        ];
        let f = scan_ini_keys(&extra, "pytest.ini");
        assert_eq!(
            kinds(&f),
            BTreeMap::from([("ini_key", 1), ("required_plugin", 1)])
        );
        let req = f.iter().find(|f| f.kind == "required_plugin").unwrap();
        assert!(req.blocking);
        assert_eq!(req.text, "pytest-xdist>=3");
        let key = f.iter().find(|f| f.kind == "ini_key").unwrap();
        assert!(!key.blocking);
        assert_eq!(key.text, "rsyncdirs = src tests");
    }

    #[test]
    fn source_unguarded_import_blocks_with_helper_fix() {
        let src = "import os\nfrom xdist import get_xdist_worker_id\n\ndef f(r):\n    return get_xdist_worker_id(r)\n";
        let f = scan(src, "tests/conftest.py");
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].kind, "import");
        assert_eq!(f[0].location, "tests/conftest.py:2");
        assert!(f[0].blocking);
        assert!(f[0].fix.contains("worker_id"));
        assert!(f[0].fix.contains("PYTEST_XDIST_WORKER"));
    }

    #[test]
    fn source_import_forms() {
        for line in [
            "import xdist",
            "import xdist.plugin",
            "from xdist.scheduler import LoadScheduling",
            "  from xdist import is_xdist_worker",
            "import os, xdist",
            "import pytest, xdist.plugin",
            "import os as o, xdist as x",
        ] {
            assert_eq!(scan(line, "c.py").len(), 1, "{line}");
        }
        for line in [
            "import xdistlike",
            "import os, xdistlike",
            "# import xdist",
            "x = 'import xdist'",
            "pytest.importorskip(\"xdist\")",
        ] {
            assert!(scan(line, "c.py").is_empty(), "{line}");
        }
    }

    #[test]
    fn import_fix_is_per_line() {
        let src = "from xdist import is_xdist_worker\nimport xdist.plugin as xp\n";
        let f = scan(src, "conftest.py");
        assert_eq!(f.len(), 2, "{f:?}");
        assert!(f[0].fix.contains("is_xdist_worker"), "{}", f[0].fix);
        assert!(!f[1].fix.contains("is_xdist_worker"), "{}", f[1].fix);
        assert!(f[1].fix.contains("xdist.plugin"), "{}", f[1].fix);
    }

    #[test]
    fn import_fix_finds_helpers_through_the_bound_name() {
        let text = "import xdist.plugin as xp\nxp.get_xdist_worker_id(r)\n";
        assert!(import_fix("import xdist.plugin as xp", text).contains("worker_id"));
        let text = "import os, xdist\nxdist.is_xdist_controller(c)\n";
        assert!(import_fix("import os, xdist", text).contains("is_xdist_controller"));
        // Plain module, no helper: the generic advice.
        assert!(import_fix("import xdist", "import xdist\n").contains("testrun_uid"));
        // Other internals name their module.
        let fix = import_fix(
            "from xdist.scheduler import LoadScheduling",
            "from xdist.scheduler import LoadScheduling\n",
        );
        assert!(fix.contains("`xdist.scheduler`"), "{fix}");
        assert!(!fix.contains("is_xdist_worker"));
    }

    #[test]
    fn source_guarded_import_is_fine() {
        let src = "try:\n    # optional\n    import xdist\nexcept ImportError:\n    xdist = None\n";
        assert!(scan(src, "c.py").is_empty());
        let src = "if TYPE_CHECKING:\n    from xdist.workermanage import WorkerController\n";
        assert!(scan(src, "c.py").is_empty());
        // Not the first statement of the block, and nested blocks inside it.
        let src = "if typing.TYPE_CHECKING:\n    from _pytest.config import Config\n    from xdist.workermanage import WorkerController\n";
        assert!(scan(src, "c.py").is_empty());
        let src = "try:\n    import os\n    if os.name:\n        import xdist\nexcept ImportError:\n    pass\n";
        assert!(scan(src, "c.py").is_empty());
    }

    #[test]
    fn render_header_excludes_allowed_from_blocking() {
        let mut f = scan_addopts(&v(&["-n", "4"]), "pytest.ini", None);
        assert!(f.iter().all(|f| f.blocking));
        let total = f.len();
        f[0].allowed = true;
        let (mut sink, cap) = Sink::captured();
        render(&f, &mut sink);
        let header = cap.out().lines().next().unwrap().to_string();
        assert_eq!(
            header,
            format!("  {total} finding(s), {} blocking, 1 allowed:", total - 1)
        );
    }

    #[test]
    fn source_hasplugin_gated_import_is_fine() {
        let src = "\
def pytest_configure(config):
    if config.pluginmanager.hasplugin(\"xdist\"):
        from xdist import is_xdist_worker
";
        let f = scan(src, "conftest.py");
        assert_eq!(kinds(&f), BTreeMap::from([("hasplugin_gate", 1)]), "{f:?}");
        for cond in [
            "pm.hasplugin('xdist') and not config.option.foo",
            "config.option.foo and pm.hasplugin('xdist')",
            "pm.getplugin('xdist')",
            "pm.getplugin('xdist') is not None",
            "(pm.hasplugin('xdist'))",
        ] {
            let src = format!("if {cond}:\n    from xdist import is_xdist_worker\n");
            assert_eq!(kinds(&scan(&src, "c.py")).get("import"), None, "{cond}");
        }
        // Gates whose body can still run without xdist don't guard it.
        for cond in [
            "not pm.hasplugin('xdist')",
            "not(pm.hasplugin('xdist'))",
            "pm.getplugin('xdist') is None",
            "pm.hasplugin('xdist') or os.environ.get('PYTEST_XDIST_WORKER')",
            "pm.hasplugin('xdist') == False",
            "pm.hasplugin('xdist') is not None",
        ] {
            let src = format!("if {cond}:\n    from xdist import is_xdist_worker\n");
            assert_eq!(kinds(&scan(&src, "c.py")).get("import"), Some(&1), "{cond}");
        }
    }

    #[test]
    fn source_type_checking_guard_must_be_positive() {
        for cond in [
            "TYPE_CHECKING and sys.version_info >= (3, 9)",
            "(typing.TYPE_CHECKING)",
        ] {
            let src = format!("if {cond}:\n    import xdist\n");
            assert!(scan(&src, "c.py").is_empty(), "{cond}");
        }
        for cond in [
            "not TYPE_CHECKING",
            "TYPE_CHECKING or sys.version_info < (3, 9)",
            "TYPE_CHECKING_LATER",
        ] {
            let src = format!("if {cond}:\n    import xdist\n");
            assert_eq!(scan(&src, "c.py").len(), 1, "{cond}");
        }
        // The `else` of a TYPE_CHECKING block runs.
        let src = "if TYPE_CHECKING:\n    pass\nelse:\n    import xdist\n";
        assert_eq!(scan(src, "c.py").len(), 1);
    }

    #[test]
    fn source_hook_class_registration() {
        let class = "\
class XdistHooks:
    def pytest_configure_node(self, node):
        pass
";
        // Registered only behind an xdist gate: safe.
        let src = format!(
            "{class}\ndef pytest_configure(config):\n    if config.pluginmanager.hasplugin('xdist'):\n        config.pluginmanager.register(XdistHooks())\n"
        );
        assert_eq!(kinds(&scan(&src, "conftest.py")).get("hook"), None);
        // Registered unconditionally: blocks.
        let src = format!(
            "{class}\ndef pytest_configure(config):\n    config.pluginmanager.register(XdistHooks(), 'x')\n"
        );
        let f = scan(&src, "conftest.py");
        assert!(f.iter().any(|f| f.kind == "hook" && f.blocking), "{f:?}");
        // Registered elsewhere: flagged, but not blocking.
        let f = scan(class, "conftest.py");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].kind == "hook" && !f[0].blocking);
        // A module-level hook defined only under a gate.
        let src = "if pm.hasplugin('xdist'):\n    def pytest_configure_node(node):\n        pass\n";
        assert_eq!(kinds(&scan(src, "conftest.py")).get("hook"), None);
    }

    #[test]
    fn plugin_roots_dedup_and_startup_modules() {
        let p = |entry: &str| ProbedPlugin {
            dist: "pytest-foo".into(),
            root: "/sp/pytest_foo".into(),
            entry: Some(entry.into()),
        };
        let plugins = [p("/sp/pytest_foo/a.py"), p("/sp/pytest_foo/b.py")];
        let roots = plugin_roots(plugins.iter());
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[Path::new("/sp/pytest_foo")].1.len(), 2);
        let entry = Path::new("/sp/pytest_foo/sub/plugin.py");
        for f in [
            "/sp/pytest_foo/sub/plugin.py",
            "/sp/pytest_foo/__init__.py",
            "/sp/pytest_foo/sub/__init__.py",
        ] {
            assert!(imported_at_startup(Path::new(f), entry), "{f}");
        }
        for f in [
            "/sp/pytest_foo/other.py",
            "/sp/pytest_foo/other/__init__.py",
        ] {
            assert!(!imported_at_startup(Path::new(f), entry), "{f}");
        }
    }

    #[test]
    fn source_import_outside_the_guard_still_blocks() {
        // After the try/except, at a shallower level than the try body.
        let src = "try:\n    import yaml\nexcept ImportError:\n    yaml = None\nimport xdist\n";
        assert_eq!(scan(src, "c.py").len(), 1);
        // Inside a function: raises when called.
        let src = "def f():\n    import xdist\n";
        assert_eq!(scan(src, "c.py").len(), 1);
        // Inside `except ImportError:` of an unrelated try.
        let src = "try:\n    import a\nexcept ImportError:\n    import xdist\n";
        assert_eq!(scan(src, "c.py").len(), 1);
    }

    #[test]
    fn source_hooks_need_optionalhook() {
        let src = "\
def pytest_configure_node(node):
    pass


@pytest.hookimpl(optionalhook=True)
def pytest_testnodedown(node, error):
    pass


@pytest.hookimpl(
    tryfirst=True,
    optionalhook=True,
)
def pytest_testnodeready(node):
    pass


class Plugin:
    @pytest.hookimpl(tryfirst=True)
    def pytest_xdist_make_scheduler(self, config, log):
        pass


def pytest_configure(config):
    config.pluginmanager.register(Plugin())
";
        let f = scan(src, "conftest.py");
        let locs: Vec<&str> = f.iter().map(|f| f.location.as_str()).collect();
        assert_eq!(locs, ["conftest.py:1", "conftest.py:20"], "{f:?}");
        assert!(f[0].fix.contains("emulated"));
        assert!(f[1]
            .fix
            .contains("never calls `pytest_xdist_make_scheduler`"));
        assert!(f[1]
            .why
            .contains("unknown hook 'pytest_xdist_make_scheduler'"));
    }

    #[test]
    fn source_optionalhook_false_still_blocks() {
        let src =
            "@pytest.hookimpl(optionalhook=False)\ndef pytest_configure_node(node):\n    pass\n";
        assert_eq!(scan(src, "conftest.py").len(), 1);
        let src =
            "@pytest.hookimpl(optionalhook = True)\ndef pytest_configure_node(node):\n    pass\n";
        assert!(scan(src, "conftest.py").is_empty());
    }

    #[test]
    fn source_previous_body_is_not_the_decorator_block() {
        // No blank line between the functions: `optionalhook=True` in the
        // previous body (a string here) must not mark the hook optional.
        let src = "\
def helper():
    return dict(optionalhook=True)
@pytest.hookimpl(tryfirst=True)
def pytest_testnodedown(node, error):
    pass
";
        let f = scan(src, "conftest.py");
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].location, "conftest.py:4");
    }

    #[test]
    fn source_project_declared_specs_are_not_findings() {
        // The e2e nodehooks fixture shape: specs via pytest_addhooks, then
        // plain implementations.
        let spec_src = "\
def pytest_addhooks(pluginmanager):
    class XdistSpecs:
        @pytest.hookspec
        def pytest_configure_node(self, node): ...

        @pytest.hookspec(firstresult=True)
        def pytest_testnodedown(self, node, error): ...

    pluginmanager.add_hookspecs(XdistSpecs)


class XDistHooks:
    def pytest_configure_node(self, node):
        pass

    def pytest_testnodedown(self, node, error):
        pass

    def pytest_testnodeready(self, node):
        pass
";
        let specs: BTreeSet<String> = hook_specs(spec_src).into_iter().collect();
        assert_eq!(
            specs,
            BTreeSet::from(["pytest_configure_node".into(), "pytest_testnodedown".into()])
        );
        let f = scan_source(spec_src, "conftest.py", &specs, false);
        // Only the hook the project did NOT declare remains.
        let texts: Vec<&str> = f.iter().map(|f| f.text.as_str()).collect();
        assert_eq!(texts, ["def pytest_testnodeready(self, node):"], "{f:?}");
    }

    #[test]
    fn source_hooks_in_test_modules_are_inert() {
        let src = "def pytest_configure_node(node):\n    pass\n";
        assert!(scan_source(src, "tests/test_x.py", &BTreeSet::new(), true).is_empty());
    }

    #[test]
    fn source_hasplugin_gate_is_a_behavior_change() {
        let src = "def pytest_configure(config):\n    if config.pluginmanager.hasplugin('xdist'):\n        pass\n";
        let f = scan(src, "conftest.py");
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].kind, "hasplugin_gate");
        assert!(!f[0].blocking);
    }

    #[test]
    fn plugin_source_reports_only_gates() {
        let src = "import xdist\nif config.pluginmanager.hasplugin(\"xdist\"):\n    x = 1\n";
        let f = scan_plugin_source(src, "pytest-sugar", "/site/pytest_sugar.py", false);
        assert_eq!(
            kinds(&f),
            BTreeMap::from([("plugin_gate", 1), ("plugin_import", 1)])
        );
        let gate = f.iter().find(|f| f.kind == "plugin_gate").unwrap();
        assert_eq!(gate.location, "pytest-sugar (/site/pytest_sugar.py:2)");
        assert!(!gate.blocking);
        // Outside the entry module the import is only a warning.
        assert!(f.iter().all(|f| !f.blocking));
    }

    #[test]
    fn plugin_entry_module_top_level_import_blocks() {
        let src = "from xdist import get_xdist_worker_id\n";
        let f = scan_plugin_source(src, "pytest-foo", "/site/pytest_foo/plugin.py", true);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].kind, "plugin_import");
        assert!(f[0].blocking);
        assert!(f[0].why.contains("every run stops"));
    }

    #[test]
    fn plugin_guarded_or_function_level_imports_are_left_alone() {
        let src = "\
try:
    import xdist
except ImportError:
    xdist = None


def pytest_configure(config):
    if config.pluginmanager.hasplugin('xdist'):
        from xdist import is_xdist_worker
";
        let f = scan_plugin_source(src, "pytest-foo", "/p.py", true);
        assert_eq!(kinds(&f), BTreeMap::from([("plugin_gate", 1)]), "{f:?}");
    }

    fn outcomes(entries: &[(&str, Phase)]) -> Outcomes {
        entries
            .iter()
            .map(|(id, phase)| {
                (
                    id.to_string(),
                    Rec {
                        phase: *phase,
                        wall: 0.0,
                        cpu: None,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn trial_names_regressions_and_dropped_tests() {
        let base = outcomes(&[
            ("a", Phase::Pass),
            ("b", Phase::Pass),
            ("c", Phase::Fail),
            ("d", Phase::Pass),
        ]);
        let hidden = outcomes(&[("a", Phase::Pass), ("b", Phase::Fail), ("c", Phase::Fail)]);
        let t = trial_report(Some(&base), Some(&hidden), "");
        assert!(t.started);
        assert_eq!(
            t.regressions,
            v(&["b", "d"]),
            "b fails, d is no longer collected"
        );
        assert_eq!(t.failed, 2);
        assert_eq!(t.tests, 3);
        assert!(!t.passed());
    }

    #[test]
    fn trial_empty_run_after_usage_error_is_not_started() {
        let base = outcomes(&[("a", Phase::Pass)]);
        let err = "pytest.main(): error: unrecognized arguments: -n --dist\n";
        let t = trial_report(Some(&base), Some(&Outcomes::new()), err);
        assert!(!t.started);
        assert!(t.error.unwrap().contains("unrecognized arguments"));
        // No baseline, but pytest said so.
        assert!(!trial_report(None, Some(&Outcomes::new()), err).started);
        // pytest's own messages are uppercase or CamelCase.
        for err in [
            "ERROR: Missing required plugins: pytest-xdist\n",
            "PluginValidationError: unknown hook 'pytest_configure_node'\n",
        ] {
            assert!(
                !trial_report(None, Some(&Outcomes::new()), err).started,
                "{err}"
            );
        }
        // A genuinely empty selection with no error is a (vacuous) pass.
        assert!(trial_report(None, Some(&Outcomes::new()), "").passed());
    }

    #[test]
    fn trial_without_baseline_counts_failures_only() {
        let hidden = outcomes(&[("a", Phase::Pass), ("b", Phase::Fail)]);
        let t = trial_report(None, Some(&hidden), "");
        assert!(t.passed(), "no baseline, nothing to call a regression");
        assert!(!t.compared);
        assert_eq!(t.failed, 1);
    }

    #[test]
    fn baseline_that_never_started_is_a_failure() {
        let err = "ERROR: usage: pytest [options]\npytest: error: unrecognized arguments: --foo\n";
        let empty = Outcomes::new();
        let ran = outcomes(&[("a", Phase::Pass)]);
        assert!(baseline_failure(true, None, "").is_some());
        let e = baseline_failure(true, Some(&empty), err).unwrap();
        assert!(e.contains("unrecognized arguments: --foo"));
        // An empty selection with no error, or a run with tests, is a baseline.
        assert!(baseline_failure(true, Some(&empty), "").is_none());
        assert!(baseline_failure(true, Some(&ran), err).is_none());
        // Not installed: there is no baseline to fail.
        assert!(baseline_failure(false, None, err).is_none());
    }

    #[test]
    fn trial_with_failed_baseline_does_not_pass() {
        let hidden = outcomes(&[("a", Phase::Pass), ("b", Phase::Fail)]);
        let mut t = trial_report(None, Some(&hidden), "");
        t.baseline_error = Some("ERROR: unrecognized arguments: --foo".into());
        assert!(t.started && !t.passed());
        let (mut sink, cap) = Sink::captured();
        render_trial(&t, &mut sink);
        let out = cap.out();
        assert!(
            out.contains("did not start with pytest-xdist loaded"),
            "{out}"
        );
        assert!(out.contains("unrecognized arguments: --foo"), "{out}");
        assert!(!out.contains("isn't installed"), "{out}");
    }

    #[test]
    fn trial_session_that_never_started_keeps_stderr_tail() {
        let err =
            "noise\n\nERROR: usage: pytest [options]\npytest: error: unrecognized arguments: -n\n";
        let t = trial_report(None, None, err);
        assert!(!t.started && !t.passed());
        let e = t.error.unwrap();
        assert!(e.contains("unrecognized arguments: -n"));
        assert_eq!(stderr_tail(err, 2).lines().count(), 2);
    }

    #[test]
    fn doc_envelope_and_trial_shape() {
        let doc = XdistRemovalDoc {
            findings: scan_addopts(&v(&["-n", "4"]), "pytest.ini addopts", None),
            meta: MigrateMeta {
                kind: "xdist-removal-check".into(),
                runner: "rstest".into(),
                schema: 1,
            },
            ready: false,
            trial: None,
            xdist_version: Some("3.6.1".into()),
        };
        let j = serde_json::to_value(&doc).unwrap();
        assert_eq!(j["meta"]["kind"], "xdist-removal-check");
        assert_eq!(j["findings"][0]["location"], "pytest.ini addopts");
        assert!(j["trial"].is_null());
        let t = serde_json::to_value(TrialReport {
            started: true,
            ..TrialReport::default()
        })
        .unwrap();
        assert_eq!(
            t,
            serde_json::json!({
                "compared": false,
                "failed": 0,
                "regressions": [],
                "started": true,
                "tests": 0
            })
        );
    }
}
