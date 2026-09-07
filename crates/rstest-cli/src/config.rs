//! Minimal pytest ini discovery: rootdir, `python_files`, `testpaths`.
//! Precedence per pytest docs: a `[pytest]`/`[tool:pytest]`/ini_options section
//! wins; files probed upward per dir: pytest.ini, pyproject.toml, tox.ini, setup.cfg.

use std::io::Write;
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

/// `err` receives the "ignoring malformed <file>" diagnostic; callers pass their
/// [`Sink`](crate::reporting::sink::Sink)'s stderr handle so it is captured and
/// consistent with the rest of the run's output (never a raw `eprintln!`).
pub fn discover(start: &Path, err: &mut dyn Write) -> ProjectConfig {
    let start = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());
    for dir in start.ancestors() {
        for probe in ["pytest.ini", "pyproject.toml", "tox.ini", "setup.cfg"] {
            let path = dir.join(probe);
            if !path.exists() {
                continue;
            }
            if let Some(mut cfg) = parse_config_file(&path, err) {
                cfg.rootdir = dir.to_path_buf();
                return cfg;
            }
        }
    }
    ProjectConfig::default()
}

/// Does this directory carry its own pytest configuration? (Any of the four
/// config files with a pytest section - the monorepo discovery predicate.)
pub fn has_pytest_config(dir: &Path, err: &mut dyn Write) -> bool {
    ["pytest.ini", "pyproject.toml", "tox.ini", "setup.cfg"]
        .iter()
        .any(|probe| {
            let p = dir.join(probe);
            p.exists() && parse_config_file(&p, err).is_some()
        })
}

fn parse_config_file(path: &Path, err: &mut dyn Write) -> Option<ProjectConfig> {
    let text = std::fs::read_to_string(path).ok()?;
    let name = path.file_name()?.to_str()?;
    match name {
        "pyproject.toml" => parse_pyproject(&text, path, err),
        "pytest.ini" => parse_ini(&text, "pytest"),
        "tox.ini" => parse_ini(&text, "pytest"),
        "setup.cfg" => parse_ini(&text, "tool:pytest"),
        _ => None,
    }
}

fn parse_pyproject(text: &str, path: &Path, err: &mut dyn Write) -> Option<ProjectConfig> {
    let doc: toml::Value = match toml::from_str(text) {
        Ok(d) => d,
        Err(e) => {
            let _ = writeln!(err, "rstest: ignoring malformed {}: {e}", path.display());
            return None;
        }
    };
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

/// Just enough INI parsing for pytest configs. Handles the single-line
/// `key = v1 v2` form, configparser's `:` delimiter, and the indented
/// multi-line form:
///
/// ```ini
/// testpaths =
///     tests
///     integration
/// ```
fn parse_ini(text: &str, section: &str) -> Option<ProjectConfig> {
    fn apply(cfg: &mut ProjectConfig, key: &str, values: Vec<String>) {
        if values.is_empty() {
            return;
        }
        match key {
            "python_files" => cfg.python_files = values,
            "testpaths" => cfg.testpaths = values,
            _ => {}
        }
    }

    let mut in_section = false;
    let mut cfg = ProjectConfig::default();
    let mut found = false;
    // The key still accepting indented continuation lines, plus values so far.
    let mut open: Option<(String, Vec<String>)> = None;

    for raw in text.lines() {
        let line = raw.trim_end();
        let content = line.trim_start();
        let is_indented = line.starts_with([' ', '\t']);

        // Blank line: configparser keeps it as part of an open value
        // (empty_lines_in_values=True is the default), so it does NOT
        // terminate the key. The value ends at the next un-indented key,
        // section header, or EOF. For whitespace-split lists a blank line
        // contributes nothing.
        if content.is_empty() {
            continue;
        }
        // Section header (never indented).
        if !is_indented {
            if let Some(name) = content.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                if let Some((k, v)) = open.take() {
                    apply(&mut cfg, &k, v);
                }
                in_section = name == section;
                found |= in_section;
                continue;
            }
        }
        if !in_section {
            continue;
        }
        if content.starts_with(['#', ';']) {
            continue;
        }
        // Indented continuation of an open key.
        if is_indented {
            if let Some((_, v)) = open.as_mut() {
                v.extend(content.split_whitespace().map(String::from));
                continue;
            }
        }
        // New key line: close the previous key, open this one. Split on the
        // first `=` or `:` (configparser accepts either delimiter).
        if let Some((k, v)) = open.take() {
            apply(&mut cfg, &k, v);
        }
        if let Some(idx) = content.find(['=', ':']) {
            let key = content[..idx].trim().to_string();
            let values = content[idx + 1..]
                .split_whitespace()
                .map(String::from)
                .collect();
            open = Some((key, values));
        }
    }
    if let Some((k, v)) = open.take() {
        apply(&mut cfg, &k, v);
    }
    found.then_some(cfg)
}

/// rstest's own defaults from `[tool.rstest]` in pyproject.toml. Precedence: CLI
/// flag > [tool.rstest] > built-in default. Looked up independently of pytest-ini
/// discovery, since this section only ever lives in pyproject.toml.
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

pub fn rstest_settings(start: &Path, err: &mut dyn Write) -> RstestSettings {
    let start = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());
    for dir in start.ancestors() {
        let path = dir.join("pyproject.toml");
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let doc = match toml::from_str::<toml::Value>(&text) {
            Ok(doc) => doc,
            Err(e) => {
                let _ = writeln!(err, "rstest: ignoring malformed {}: {e}", path.display());
                continue;
            }
        };
        let Some(tool) = doc.get("tool").and_then(|t| t.get("rstest")) else {
            // pyproject exists but has no [tool.rstest]: stop at the
            // nearest pyproject (project boundary), like pytest does.
            return RstestSettings::default();
        };
        return RstestSettings {
            numprocesses: match tool.get("numprocesses") {
                // Reject negatives; a `-3` typo must not become the literal "-3".
                Some(toml::Value::Integer(n)) if *n >= 0 => Some(n.to_string()),
                Some(toml::Value::String(s)) => Some(s.clone()),
                _ => None,
            },
            dist: tool.get("dist").and_then(|v| v.as_str()).map(String::from),
            // `try_from` rejects negatives instead of wrapping to a huge budget.
            reruns: tool
                .get("reruns")
                .and_then(|v| v.as_integer())
                .and_then(|n| u32::try_from(n).ok()),
            reruns_only_known_flaky: tool
                .get("reruns-only-known-flaky")
                .and_then(|v| v.as_bool()),
            worker_timeout: tool
                .get("worker-timeout")
                .and_then(|v| v.as_integer())
                .and_then(|n| u64::try_from(n).ok()),
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
        // BOTH patterns - undercounting silently caps -n auto.
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
        let s = rstest_settings(&d, &mut std::io::sink());
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
        assert_eq!(
            rstest_settings(&d, &mut std::io::sink())
                .numprocesses
                .as_deref(),
            Some("auto")
        );
    }

    #[test]
    fn nearest_pyproject_is_the_boundary() {
        // A pyproject WITHOUT [tool.rstest] stops the ancestor walk -
        // a parent project's settings must not leak in.
        let parent = tmpdir("boundary");
        std::fs::write(parent.join("pyproject.toml"), "[tool.rstest]\nreruns = 9\n").unwrap();
        let child = parent.join("sub");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(child.join("pyproject.toml"), "[project]\nname = \"x\"\n").unwrap();
        assert_eq!(rstest_settings(&child, &mut std::io::sink()).reruns, None);
    }

    #[test]
    fn discover_reads_pytest_ini_python_files() {
        let d = tmpdir("ini");
        std::fs::write(
            d.join("pytest.ini"),
            "[pytest]\npython_files = check_*.py\ntestpaths = tests\n",
        )
        .unwrap();
        let cfg = discover(&d, &mut std::io::sink());
        assert_eq!(cfg.python_files, vec!["check_*.py"]);
        assert_eq!(cfg.testpaths, vec!["tests"]);
        assert_eq!(cfg.rootdir, d.canonicalize().unwrap());
    }

    #[test]
    fn discover_reads_pyproject_ini_options_with_string_and_list() {
        // pytest.ini absent (first probe) => the probe loop `continue`s to
        // pyproject.toml. python_files as a SPACE-STRING exercises the string
        // arm of toml_str_list; testpaths as a list exercises the array arm.
        let d = tmpdir("pyproj-ini");
        std::fs::write(
            d.join("pyproject.toml"),
            "[tool.pytest.ini_options]\npython_files = \"check_*.py chk_*.py\"\ntestpaths = [\"a\", \"b\"]\n",
        )
        .unwrap();
        let cfg = discover(&d, &mut std::io::sink());
        assert_eq!(cfg.python_files, vec!["check_*.py", "chk_*.py"]);
        assert_eq!(cfg.testpaths, vec!["a", "b"]);
    }

    #[test]
    fn discover_reads_tox_ini_and_setup_cfg() {
        // tox.ini uses [pytest]; setup.cfg uses [tool:pytest].
        let t = tmpdir("tox");
        std::fs::write(t.join("tox.ini"), "[pytest]\npython_files = tox_*.py\n").unwrap();
        assert_eq!(
            discover(&t, &mut std::io::sink()).python_files,
            vec!["tox_*.py"]
        );

        let s = tmpdir("setupcfg");
        std::fs::write(
            s.join("setup.cfg"),
            "[tool:pytest]\npython_files = cfg_*.py\n",
        )
        .unwrap();
        assert_eq!(
            discover(&s, &mut std::io::sink()).python_files,
            vec!["cfg_*.py"]
        );
    }

    #[test]
    fn discover_bare_dir_falls_back_to_default() {
        // No config file up the ancestry => built-in default (both patterns).
        let d = tmpdir("bare");
        let cfg = discover(&d, &mut std::io::sink());
        assert_eq!(cfg.python_files, vec!["test_*.py", "*_test.py"]);
    }

    #[test]
    fn has_pytest_config_detects_section() {
        let yes = tmpdir("has-yes");
        std::fs::write(yes.join("pytest.ini"), "[pytest]\n").unwrap();
        assert!(has_pytest_config(&yes, &mut std::io::sink()));

        // A pyproject with no pytest section is not a pytest config boundary.
        let no = tmpdir("has-no");
        std::fs::write(no.join("pyproject.toml"), "[project]\nname = \"x\"\n").unwrap();
        assert!(!has_pytest_config(&no, &mut std::io::sink()));
    }

    #[test]
    fn ini_skips_comments_and_unknown_keys() {
        let d = tmpdir("ini-comments");
        std::fs::write(
            d.join("pytest.ini"),
            "[pytest]\n# a comment\n; also a comment\nunknown = whatever\npython_files = k_*.py\n",
        )
        .unwrap();
        let cfg = discover(&d, &mut std::io::sink());
        assert_eq!(cfg.python_files, vec!["k_*.py"]);
    }

    #[test]
    fn settings_projects_and_non_integer_numprocesses() {
        let d = tmpdir("projects");
        std::fs::write(
            d.join("pyproject.toml"),
            "[tool.rstest]\nnumprocesses = 1.5\nprojects = [\"pkg_a\", \"pkg_b\"]\ncollect = \"lazy\"\noutput = \"bar\"\n",
        )
        .unwrap();
        let s = rstest_settings(&d, &mut std::io::sink());
        // A float is neither Integer nor String => None.
        assert_eq!(s.numprocesses, None);
        assert_eq!(s.projects, Some(vec!["pkg_a".into(), "pkg_b".into()]));
        assert_eq!(s.collect.as_deref(), Some("lazy"));
        assert_eq!(s.output.as_deref(), Some("bar"));
    }

    #[test]
    fn settings_reject_negative_ints() {
        // A `-1`/`-5`/`-3` typo must not wrap to a near-infinite budget or
        // survive as a literal string; each falls back to None.
        let d = tmpdir("neg-ints");
        std::fs::write(
            d.join("pyproject.toml"),
            "[tool.rstest]\nreruns = -1\nworker-timeout = -5\nnumprocesses = -3\n",
        )
        .unwrap();
        let s = rstest_settings(&d, &mut std::io::sink());
        assert_eq!(s.reruns, None);
        assert_eq!(s.worker_timeout, None);
        assert_eq!(s.numprocesses, None);
    }

    #[test]
    fn ini_reads_multiline_and_colon_values() {
        // configparser indented multi-line form + `:` delimiter.
        let d = tmpdir("ini-multiline");
        std::fs::write(
            d.join("pytest.ini"),
            "[pytest]\ntestpaths =\n    tests\n    integration\npython_files:\n    check_*.py\n    chk_*.py\n",
        )
        .unwrap();
        let cfg = discover(&d, &mut std::io::sink());
        assert_eq!(cfg.testpaths, vec!["tests", "integration"]);
        assert_eq!(cfg.python_files, vec!["check_*.py", "chk_*.py"]);
    }

    #[test]
    fn discover_malformed_pyproject_falls_back_to_default() {
        // Malformed pyproject reached via discover() exercises parse_pyproject's
        // toml error arm (distinct from rstest_settings' own parse). pytest.ini
        // absent => probe loop falls through to pyproject.toml => None => default.
        let d = tmpdir("discover-bad-toml");
        std::fs::write(d.join("pyproject.toml"), "not [[[ valid = toml").unwrap();
        // The diagnostic routes through the passed writer (a Sink's stderr in
        // production), not a raw eprintln! — capture and assert it here.
        let mut err = Vec::new();
        let cfg = discover(&d, &mut err);
        assert_eq!(cfg.python_files, vec!["test_*.py", "*_test.py"]);
        assert_eq!(cfg.testpaths, Vec::<String>::new());
        assert!(
            String::from_utf8_lossy(&err).contains("ignoring malformed"),
            "expected the malformed-config note on the writer: {err:?}"
        );
    }

    #[test]
    fn ini_empty_value_and_section_header_close() {
        // `testpaths =` with no values opens an empty key; the `[other]` header
        // closes it via the section-header path, and apply() drops the empty
        // vec (leaving testpaths at its default) rather than clobbering it.
        let d = tmpdir("ini-empty-close");
        std::fs::write(
            d.join("pytest.ini"),
            "[pytest]\ntestpaths =\n[other]\npython_files = x_*.py\n",
        )
        .unwrap();
        let cfg = discover(&d, &mut std::io::sink());
        // Empty testpaths not applied => stays default (empty).
        assert_eq!(cfg.testpaths, Vec::<String>::new());
        // python_files lives in [other], not [pytest] => default retained.
        assert_eq!(cfg.python_files, vec!["test_*.py", "*_test.py"]);
    }

    #[test]
    fn ini_blank_lines_do_not_truncate_multiline_value() {
        // Regression: configparser keeps blank lines inside a value
        // (empty_lines_in_values=True). A blank line right after `key =`
        // (before the first indented value) OR between continuation lines
        // must NOT drop the indented values. The value ends only at the
        // next un-indented key/section/EOF.
        let d = tmpdir("ini-blank-lines");
        std::fs::write(
            d.join("pytest.ini"),
            "[pytest]\ntestpaths =\n\n    tests\n\n    integration\npython_files = y_*.py\n",
        )
        .unwrap();
        let cfg = discover(&d, &mut std::io::sink());
        assert_eq!(cfg.testpaths, vec!["tests", "integration"]);
        assert_eq!(cfg.python_files, vec!["y_*.py"]);
    }

    #[test]
    fn settings_bad_toml_falls_back_to_default() {
        // Unparseable pyproject => skipped; with none valid up-tree => default.
        let d = tmpdir("bad-toml");
        std::fs::write(d.join("pyproject.toml"), "this is : not = valid toml [[[").unwrap();
        let mut err = Vec::new();
        let s = rstest_settings(&d, &mut err);
        assert_eq!(s.numprocesses, None);
        assert_eq!(s.reruns, None);
        assert!(
            String::from_utf8_lossy(&err).contains("ignoring malformed"),
            "expected the malformed-config note on the writer: {err:?}"
        );
    }
}
