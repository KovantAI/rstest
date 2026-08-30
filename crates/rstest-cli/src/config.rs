//! Minimal pytest ini discovery: rootdir, `python_files`, `testpaths`.
//!
//! Port of the validated subset from research spike 1 (static_collect.py).
//! Precedence per pytest docs: a `[pytest]`/`[tool:pytest]`/ini_options
//! section wins; files are probed upward from the invocation dir in the
//! order pytest.ini, pyproject.toml, tox.ini, setup.cfg per directory.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct ProjectConfig {
    pub rootdir: PathBuf,
    /// File-name patterns for test modules (pytest default: `test_*.py`).
    /// rstest additionally always accepts `*_test.py` only when listed here.
    pub python_files: Vec<String>,
    /// Default collection roots when no paths are given on the CLI.
    pub testpaths: Vec<String>,
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            rootdir: PathBuf::from("."),
            // pytest's actual default: BOTH patterns. Undercounting here
            // silently caps `-n auto` (the walk feeds auto_workers), so
            // this default must match pytest exactly.
            python_files: vec!["test_*.py".into(), "*_test.py".into()],
            testpaths: Vec::new(),
        }
    }
}

pub fn discover(start: &Path) -> ProjectConfig {
    let start = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());
    for dir in start.ancestors() {
        for probe in ["pytest.ini", "pyproject.toml", "tox.ini", "setup.cfg"] {
            let path = dir.join(probe);
            if !path.exists() {
                continue;
            }
            if let Some(mut cfg) = parse_config_file(&path) {
                cfg.rootdir = dir.to_path_buf();
                return cfg;
            }
        }
    }
    ProjectConfig::default()
}

/// Does this directory carry its own pytest configuration? (Any of the
/// four config files with a pytest section — the monorepo discovery
/// predicate.)
pub fn has_pytest_config(dir: &Path) -> bool {
    ["pytest.ini", "pyproject.toml", "tox.ini", "setup.cfg"]
        .iter()
        .any(|probe| {
            let p = dir.join(probe);
            p.exists() && parse_config_file(&p).is_some()
        })
}

fn parse_config_file(path: &Path) -> Option<ProjectConfig> {
    let text = std::fs::read_to_string(path).ok()?;
    let name = path.file_name()?.to_str()?;
    match name {
        "pyproject.toml" => parse_pyproject(&text),
        "pytest.ini" => parse_ini(&text, "pytest"),
        "tox.ini" => parse_ini(&text, "pytest"),
        "setup.cfg" => parse_ini(&text, "tool:pytest"),
        _ => None,
    }
}

fn parse_pyproject(text: &str) -> Option<ProjectConfig> {
    let doc: toml::Value = toml::from_str(text).ok()?;
    let ini = doc.get("tool")?.get("pytest")?.get("ini_options")?;
    let mut cfg = ProjectConfig::default();
    if let Some(v) = ini.get("python_files") {
        cfg.python_files = toml_str_list(v);
    }
    if let Some(v) = ini.get("testpaths") {
        cfg.testpaths = toml_str_list(v);
    }
    Some(cfg)
}

fn toml_str_list(v: &toml::Value) -> Vec<String> {
    match v {
        // pytest accepts both a list and a space-separated string
        toml::Value::Array(items) => items
            .iter()
            .filter_map(|i| i.as_str().map(String::from))
            .collect(),
        toml::Value::String(s) => s.split_whitespace().map(String::from).collect(),
        _ => Vec::new(),
    }
}

/// Just enough INI parsing for `[section] key = v1 v2` pytest configs.
fn parse_ini(text: &str, section: &str) -> Option<ProjectConfig> {
    let mut in_section = false;
    let mut cfg = ProjectConfig::default();
    let mut found = false;
    for line in text.lines() {
        let line = line.trim_end();
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_section = name == section;
            found |= in_section;
            continue;
        }
        if !in_section || line.trim().is_empty() || line.trim_start().starts_with(['#', ';']) {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let values: Vec<String> = value.split_whitespace().map(String::from).collect();
            match key.trim() {
                "python_files" if !values.is_empty() => cfg.python_files = values,
                "testpaths" if !values.is_empty() => cfg.testpaths = values,
                _ => {}
            }
        }
    }
    found.then_some(cfg)
}

/// rstest's own defaults from `[tool.rstest]` in pyproject.toml.
/// Precedence everywhere: CLI flag > [tool.rstest] > built-in default.
///
/// Looked up independently of pytest-ini discovery: that walk stops at
/// the FIRST pytest config file (which may be pytest.ini), while this
/// section only ever lives in pyproject.toml.
#[derive(Debug, Default, Clone)]
pub struct RstestSettings {
    pub numprocesses: Option<String>,
    pub dist: Option<String>,
    pub reruns: Option<u32>,
    /// Gate reruns to tests with prior flaky history (`flakes.json`).
    pub reruns_only_known_flaky: Option<bool>,
    pub worker_timeout: Option<u64>,
    /// Monorepo subproject globs (relative to the pyproject's dir);
    /// restricts/replaces auto-discovery.
    pub projects: Option<Vec<String>>,
    pub collect: Option<String>,
    /// Terminal output style: "dots" (default), "verbose", or "bar".
    pub output: Option<String>,
}

pub fn rstest_settings(start: &Path) -> RstestSettings {
    let start = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());
    for dir in start.ancestors() {
        let path = dir.join("pyproject.toml");
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(doc) = toml::from_str::<toml::Value>(&text) else {
            continue;
        };
        let Some(tool) = doc.get("tool").and_then(|t| t.get("rstest")) else {
            // pyproject exists but has no [tool.rstest]: stop at the
            // nearest pyproject (project boundary), like pytest does.
            return RstestSettings::default();
        };
        return RstestSettings {
            numprocesses: match tool.get("numprocesses") {
                Some(toml::Value::Integer(n)) => Some(n.to_string()),
                Some(toml::Value::String(s)) => Some(s.clone()),
                _ => None,
            },
            dist: tool.get("dist").and_then(|v| v.as_str()).map(String::from),
            reruns: tool
                .get("reruns")
                .and_then(|v| v.as_integer())
                .map(|n| n as u32),
            reruns_only_known_flaky: tool
                .get("reruns-only-known-flaky")
                .and_then(|v| v.as_bool()),
            worker_timeout: tool
                .get("worker-timeout")
                .and_then(|v| v.as_integer())
                .map(|n| n as u64),
            projects: tool.get("projects").and_then(|v| v.as_array()).map(|a| {
                a.iter()
                    .filter_map(|i| i.as_str().map(String::from))
                    .collect()
            }),
            collect: tool
                .get("collect")
                .and_then(|v| v.as_str())
                .map(String::from),
            output: tool
                .get("output")
                .and_then(|v| v.as_str())
                .map(String::from),
        };
    }
    RstestSettings::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rstest-cfg-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn default_python_files_match_pytest() {
        // BOTH patterns — undercounting silently caps -n auto.
        let cfg = ProjectConfig::default();
        assert_eq!(cfg.python_files, vec!["test_*.py", "*_test.py"]);
    }

    #[test]
    fn settings_from_pyproject() {
        let d = tmpdir("settings");
        std::fs::write(
            d.join("pyproject.toml"),
            r#"
[tool.rstest]
numprocesses = 4
dist = "loadfile"
reruns = 2
worker-timeout = 120
"#,
        )
        .unwrap();
        let s = rstest_settings(&d);
        assert_eq!(s.numprocesses.as_deref(), Some("4"));
        assert_eq!(s.dist.as_deref(), Some("loadfile"));
        assert_eq!(s.reruns, Some(2));
        assert_eq!(s.worker_timeout, Some(120));
    }

    #[test]
    fn settings_accept_auto_string() {
        let d = tmpdir("auto");
        std::fs::write(
            d.join("pyproject.toml"),
            "[tool.rstest]\nnumprocesses = \"auto\"\n",
        )
        .unwrap();
        assert_eq!(rstest_settings(&d).numprocesses.as_deref(), Some("auto"));
    }

    #[test]
    fn nearest_pyproject_is_the_boundary() {
        // A pyproject WITHOUT [tool.rstest] stops the ancestor walk —
        // a parent project's settings must not leak in.
        let parent = tmpdir("boundary");
        std::fs::write(parent.join("pyproject.toml"), "[tool.rstest]\nreruns = 9\n").unwrap();
        let child = parent.join("sub");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(child.join("pyproject.toml"), "[project]\nname = \"x\"\n").unwrap();
        assert_eq!(rstest_settings(&child).reruns, None);
    }

    #[test]
    fn discover_reads_pytest_ini_python_files() {
        let d = tmpdir("ini");
        std::fs::write(
            d.join("pytest.ini"),
            "[pytest]\npython_files = check_*.py\ntestpaths = tests\n",
        )
        .unwrap();
        let cfg = discover(&d);
        assert_eq!(cfg.python_files, vec!["check_*.py"]);
        assert_eq!(cfg.testpaths, vec!["tests"]);
        assert_eq!(cfg.rootdir, d.canonicalize().unwrap());
    }
}
