//! Which first-party files coverage actually MEASURES this run. `--incremental`
//! trusts per-file coverage hashes, so a file coverage never measures is
//! invisible to it: editing one would leave a dependent test cached on a stale
//! pass. [`CovScope`] models the measured set the way pytest-cov + coverage.py
//! build it, so [`crate::coverage_skip`] can fold every UNMEASURED file into its
//! fingerprint instead.
//!
//! Inputs, as pytest sees them: the ini `addopts`, then `PYTEST_ADDOPTS`, then
//! the command line ([`effective_pytest_args`]); and the coverage config file
//! coverage.py would read ([`read_coverage_run`]). The measured set is
//! - `--cov=<v>` values (any bare `--cov` defers to the config's `[run] source`
//!   / `source_pkgs` / `source_dirs`), or everything under the cwd when no
//!   source is set;
//! - with no source, narrowed further by `[run] include`;
//! - minus `[run] omit`, always.
//!
//! Glob matching errs toward UNMEASURED (sound: an unmeasured file only costs a
//! coarser skip): an `omit` `*` crosses `/`, an `include` `*` does not.

use std::path::{Path, PathBuf};

use regex::Regex;

/// The measured set for one run (see the module note).
#[derive(Debug, Default)]
pub struct CovScope {
    /// Normalized source values (`./pkg/` -> `pkg`); empty = no source limit.
    pub sources: Vec<String>,
    /// Cwd-relative path prefixes the sources measure ([`scope_prefixes`]).
    prefixes: Vec<PathBuf>,
    /// `[run] include` (honored only without a source, as in coverage.py).
    include: Vec<Regex>,
    /// `[run] omit`.
    omit: Vec<Regex>,
    /// `scope` as a string, for matching absolute-path patterns.
    abs_scope: String,
}

impl CovScope {
    /// The measured set for pytest `args` (already [`effective_pytest_args`])
    /// run from `scope`. No coverage requested -> [`CovScope::default`]
    /// (nothing is partial: there is no coverage to be blind).
    pub fn resolve(scope: &Path, args: &[String]) -> Self {
        let values = cov_values(args);
        if values.is_empty() {
            return Self::default();
        }
        let cfg = coverage_config(scope, args);
        // pytest-cov: ANY bare --cov makes the CLI source None, so coverage
        // falls back to the config's source.
        let raw: Vec<String> = if values.iter().any(Option::is_none) {
            cfg.source
        } else {
            values.into_iter().flatten().map(String::from).collect()
        };
        // A whole-tree source measures the cwd: unions in everything.
        let sources = if raw.iter().any(|v| is_whole_tree(v)) {
            Vec::new()
        } else {
            raw.iter().map(|v| normalize_scope(v)).collect()
        };
        let abs_scope = slashed(&scope.to_string_lossy())
            .trim_end_matches('/')
            .to_string();
        let include = if sources.is_empty() {
            compile_globs(&abs_scope, &cfg.include, false)
        } else {
            Vec::new()
        };
        let omit = compile_globs(&abs_scope, &cfg.omit, true);
        let prefixes = sources
            .iter()
            .flat_map(|s| scope_prefixes(scope, s))
            .collect();
        Self {
            sources,
            prefixes,
            include,
            omit,
            abs_scope,
        }
    }

    /// A source-only scope (no config lookup), for tests.
    #[cfg(test)]
    pub fn from_sources(scope: &Path, sources: &[&str]) -> Self {
        let sources: Vec<String> = sources.iter().map(|s| normalize_scope(s)).collect();
        Self {
            prefixes: sources
                .iter()
                .flat_map(|s| scope_prefixes(scope, s))
                .collect(),
            sources,
            abs_scope: slashed(&scope.to_string_lossy())
                .trim_end_matches('/')
                .to_string(),
            ..Self::default()
        }
    }

    /// Whether some first-party file may go unmeasured (a source, include, or
    /// omit is in play). `false` = coverage sees every file: nothing to fold.
    pub fn is_partial(&self) -> bool {
        !self.sources.is_empty() || !self.include.is_empty() || !self.omit.is_empty()
    }

    /// Whether coverage measures the cwd-relative `rel`. Separators are
    /// normalized to `/` first: a Windows walk yields `scripts\\util.py`, and
    /// coverage.py matches its patterns against either separator.
    pub fn measures(&self, rel: &Path) -> bool {
        let rel = PathBuf::from(slashed(&rel.to_string_lossy()));
        let rel = rel.as_path();
        let abs = format!("{}/{}", self.abs_scope, rel.to_string_lossy());
        if self.omit.iter().any(|re| re.is_match(&abs)) {
            return false;
        }
        if !self.sources.is_empty() {
            return in_prefixes(rel, &self.prefixes);
        }
        self.include.is_empty() || self.include.iter().any(|re| re.is_match(&abs))
    }

    /// The source values that resolve to nothing under `scope` (no directory or
    /// module, even via the dotted / `src/` / absolute forms of
    /// [`scope_prefixes`]). Such a source exempts nothing, so EVERY first-party
    /// `.py` counts as unmeasured and any edit re-runs the suite: sound, but it
    /// silently defeats `--incremental`, so the run warns about each one.
    pub fn unmatched_sources(&self, scope: &Path) -> Vec<String> {
        self.sources
            .iter()
            .filter(|s| {
                !scope_prefixes(scope, s).iter().any(|p| {
                    let abs = scope.join(p);
                    abs.exists() || PathBuf::from(format!("{}.py", abs.to_string_lossy())).is_file()
                })
            })
            .cloned()
            .collect()
    }
}

/// The args pytest actually runs with: the ini `addopts` (from the config file
/// pytest discovers from `scope`), then `PYTEST_ADDOPTS`, then `args`, in
/// pytest's own order. A `--cov=pkg` living only in `addopts` narrows coverage
/// just as surely as one on the command line.
pub fn effective_pytest_args(scope: &Path, args: &[String]) -> Vec<String> {
    let mut out = crate::config::discover(scope, &mut std::io::sink()).addopts;
    if let Ok(env) = std::env::var("PYTEST_ADDOPTS") {
        out.extend(crate::config::shell_split(&env));
    }
    out.extend(args.iter().cloned());
    out
}

/// `path` with every `\\` turned into `/`, the one separator the matchers use.
fn slashed(path: &str) -> String {
    path.replace('\\', "/")
}

/// Just the args that shape what coverage measures (`--cov` in all forms and
/// `--cov-config`), in order, so a later run WITHOUT coverage can rebuild the
/// scope of the run that wrote the coverage index ([`CovScope::resolve`] reads
/// nothing else).
pub fn coverage_args(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        if a == "--cov" {
            out.push(a.clone());
            if let Some(v) = it.next_if(|n| !n.starts_with('-')) {
                out.push(v.clone());
            }
        } else if a == "--cov-config" {
            out.push(a.clone());
            if let Some(v) = it.next() {
                out.push(v.clone());
            }
        } else if a.starts_with("--cov=") || a.starts_with("--cov-config=") {
            out.push(a.clone());
        }
    }
    out
}

/// `--cov` values that measure the whole cwd tree (pytest-cov reads an empty /
/// `.` / `./` source as the cwd).
fn is_whole_tree(v: &str) -> bool {
    matches!(v, "" | "." | "./")
}

/// Every `--cov` occurrence in `args`: `Some(v)` for `--cov=v` or the space form
/// `--cov v` (pytest-cov's `--cov` is `nargs="?"`, so argparse consumes a
/// following non-option arg as its value), `None` for a bare `--cov`.
fn cov_values(args: &[String]) -> Vec<Option<&str>> {
    let mut out = Vec::new();
    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        if a == "--cov" {
            match it.next_if(|n| !n.starts_with('-')) {
                Some(v) => out.push(Some(v.as_str())),
                None => out.push(None),
            }
        } else if let Some(v) = a.strip_prefix("--cov=") {
            out.push(Some(v));
        }
    }
    out
}

/// Strip leading `./` and trailing `/` from a source value so it prefix-matches
/// cwd-relative paths component-wise: `Path::starts_with` compares components,
/// so a stray `CurDir` would otherwise match nothing and fold the WHOLE tree.
fn normalize_scope(v: &str) -> String {
    let mut s = v;
    while let Some(rest) = s.strip_prefix("./") {
        s = rest;
    }
    s.trim_end_matches('/').to_string()
}

/// The cwd-relative path prefixes a source value measures. Beyond the value as
/// a path, pytest-cov also accepts an absolute path (made relative to `scope`),
/// a dotted module/package name (`pkg.sub` -> `pkg/sub`), and resolves a bare
/// package name under a `src/` layout (`pkg` -> `src/pkg`). Like coverage.py,
/// a value naming an existing directory is ONLY that directory: the module-name
/// guesses (dotted, `src/`) apply only when it isn't one, so an unrelated
/// `src/pkg` next to a real `./pkg` is not assumed measured. An absolute path
/// outside `scope` yields nothing (everything folds: sound, maximally coarse).
fn scope_prefixes(scope: &Path, value: &str) -> Vec<PathBuf> {
    let p = Path::new(value);
    let base = if p.is_absolute() {
        match p.strip_prefix(scope) {
            Ok(r) => r.to_path_buf(),
            Err(_) => return Vec::new(),
        }
    } else {
        p.to_path_buf()
    };
    if scope.join(&base).is_dir() {
        return vec![base];
    }
    let mut out = vec![base];
    if !value.contains('/') && value.contains('.') && !value.ends_with(".py") {
        out.push(PathBuf::from(value.replace('.', "/")));
    }
    let src: Vec<PathBuf> = out
        .iter()
        .filter(|p| p.is_relative() && !p.starts_with("src"))
        .map(|p| Path::new("src").join(p))
        .collect();
    out.extend(src);
    out
}

/// Whether `rel` lies under one of `prefixes`: inside a scoped directory, or
/// the `<prefix>.py` module a dotted/bare name names.
fn in_prefixes(rel: &Path, prefixes: &[PathBuf]) -> bool {
    prefixes.iter().any(|p| {
        rel.starts_with(p) || rel.to_string_lossy() == format!("{}.py", p.to_string_lossy())
    })
}

/// Coverage file patterns as anchored regexes. Like coverage.py, a pattern that
/// is neither absolute nor starts with a wildcard is made absolute against the
/// cwd; a Windows drive path (`C:/...`) counts as absolute. `star_crosses`
/// picks whether `*` / `?` match `/` (see the module note on erring toward
/// unmeasured); `**` always does. Matching is case-insensitive on Windows, as
/// coverage.py's is. Uncompilable patterns are dropped.
fn compile_globs(abs_scope: &str, patterns: &[String], star_crosses: bool) -> Vec<Regex> {
    compile_globs_with(abs_scope, patterns, star_crosses, cfg!(windows))
}

/// [`compile_globs`] with the case rule explicit (testable off Windows).
fn compile_globs_with(
    abs_scope: &str,
    patterns: &[String],
    star_crosses: bool,
    case_insensitive: bool,
) -> Vec<Regex> {
    patterns
        .iter()
        .filter_map(|p| {
            let p = slashed(p);
            let p = p.strip_prefix("./").unwrap_or(&p);
            let full = if is_absolute_pattern(p) || p.starts_with(['*', '?']) {
                p.to_string()
            } else {
                format!("{abs_scope}/{p}")
            };
            regex::RegexBuilder::new(&glob_to_regex(&full, star_crosses))
                .case_insensitive(case_insensitive)
                .build()
                .ok()
        })
        .collect()
}

/// Whether a `/`-normalized pattern is absolute: rooted (`/...`, which also
/// covers a UNC `//server/...`) or a Windows drive path (`C:/...`).
fn is_absolute_pattern(p: &str) -> bool {
    let b = p.as_bytes();
    p.starts_with('/')
        || (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'/')
}

/// Translate one glob to an anchored regex (see [`compile_globs`]). `[...]` is
/// a character class as in `fnmatch` (`[!...]` negates, a leading `]` is
/// literal, an unclosed `[` is literal); without `star_crosses` a negated class
/// never matches `/`, like `*` / `?`.
fn glob_to_regex(glob: &str, star_crosses: bool) -> String {
    let (star, any) = if star_crosses {
        (".*", ".")
    } else {
        ("[^/]*", "[^/]")
    };
    let mut re = String::from("^");
    let mut chars = glob.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                // `**/` also matches zero directories.
                if chars.peek() == Some(&'/') {
                    chars.next();
                    re.push_str("(?:.*/)?");
                } else {
                    re.push_str(".*");
                }
            }
            '*' => re.push_str(star),
            '?' => re.push_str(any),
            '[' => {
                let rest: String = chars.clone().collect();
                match glob_class(&rest, star_crosses) {
                    Some((class, used)) => {
                        re.push_str(&class);
                        for _ in 0..used {
                            chars.next();
                        }
                    }
                    None => re.push_str(r"\["),
                }
            }
            c => re.push_str(&regex::escape(&c.to_string())),
        }
    }
    re.push('$');
    re
}

/// The regex class for a glob `[...]` whose body starts at `rest` (just past
/// the `[`), plus how many chars of `rest` it consumed (through the `]`).
/// `None` when unclosed.
fn glob_class(rest: &str, star_crosses: bool) -> Option<(String, usize)> {
    let chars: Vec<char> = rest.chars().collect();
    let mut i = 0;
    let negated = chars.first() == Some(&'!');
    if negated {
        i += 1;
    }
    // A `]` right after `[` / `[!` is a literal member.
    let body_start = i;
    if chars.get(i) == Some(&']') {
        i += 1;
    }
    while chars.get(i).is_some_and(|c| *c != ']') {
        i += 1;
    }
    if i >= chars.len() {
        return None;
    }
    let mut class = String::from(if negated { "[^" } else { "[" });
    for &c in &chars[body_start..i] {
        // Keep ranges (`-`); escape everything else regex treats specially.
        if c == '-' {
            class.push('-');
        } else if matches!(c, '\\' | ']' | '[' | '^' | '&' | '~') {
            class.push('\\');
            class.push(c);
        } else {
            class.push(c);
        }
    }
    if negated && !star_crosses {
        class.push('/');
    }
    class.push(']');
    Some((class, i + 1))
}

/// The `[run]` settings of the coverage config that matter for the measured set.
#[derive(Debug, Default, PartialEq)]
struct CoverageRun {
    source: Vec<String>,
    include: Vec<String>,
    omit: Vec<String>,
}

/// The `--cov-config` value (`=v` or space form), if given. The LAST one wins,
/// as with argparse: a command-line `--cov-config` overrides one in `addopts`
/// (which [`effective_pytest_args`] places first).
fn cov_config_arg(args: &[String]) -> Option<String> {
    let mut found = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--cov-config" {
            if let Some(v) = it.next() {
                found = Some(v.clone());
            }
        } else if let Some(v) = a.strip_prefix("--cov-config=") {
            found = Some(v.to_string());
        }
    }
    found
}

/// The `[run]` settings from the coverage config coverage.py would read, in its
/// own lookup order: the `--cov-config` file (pytest-cov's default
/// `.coveragerc`, which `COVERAGE_RCFILE` overrides), then `setup.cfg`,
/// `tox.ini`, `pyproject.toml`. The FIRST file coverage counts as read wins,
/// even if it sets none of these.
fn coverage_config(scope: &Path, args: &[String]) -> CoverageRun {
    let named = cov_config_arg(args)
        .filter(|f| f != ".coveragerc")
        .or_else(|| {
            std::env::var("COVERAGE_RCFILE")
                .ok()
                .filter(|v| !v.is_empty())
        })
        .unwrap_or_else(|| ".coveragerc".to_string());
    let tries = [
        (named.as_str(), true),
        ("setup.cfg", false),
        ("tox.ini", false),
        ("pyproject.toml", false),
    ];
    tries
        .iter()
        .find_map(|(file, ours)| read_coverage_run(&scope.join(file), *ours))
        .unwrap_or_default()
}

/// One coverage config file's `[run]` settings, or `None` when coverage wouldn't
/// count the file as read (missing; or, for a file coverage merely piggybacks
/// on, one with no coverage settings). An `ours` file (the named rc file) counts
/// once it exists and takes `[run]` or `[coverage:run]`; the shared INI files
/// need `[coverage:*]`; TOML needs `[tool.coverage]`. `source` also gathers
/// `source_pkgs` and `source_dirs` (coverage.py 7.10+), which narrow the same way. Values split like coverage's `getlist`: newlines and commas.
fn read_coverage_run(path: &Path, ours: bool) -> Option<CoverageRun> {
    let text = std::fs::read_to_string(path).ok()?;
    if path.extension().is_some_and(|e| e == "toml") {
        let doc: toml::Value = toml::from_str(&text).ok()?;
        let cov = doc.get("tool").and_then(|t| t.get("coverage"));
        if cov.is_none() && !ours {
            return None;
        }
        let run = cov.and_then(|c| c.get("run"));
        let list = |keys: &[&str]| -> Vec<String> {
            keys.iter()
                .filter_map(|k| run.and_then(|r| r.get(*k)))
                .flat_map(|v| match v {
                    toml::Value::Array(items) => items
                        .iter()
                        .filter_map(|i| i.as_str().map(String::from))
                        .collect(),
                    toml::Value::String(s) => split_cov_list(std::iter::once(s.as_str())),
                    _ => Vec::new(),
                })
                .collect()
        };
        return Some(CoverageRun {
            source: list(&["source", "source_pkgs", "source_dirs"]),
            include: list(&["include"]),
            omit: list(&["omit"]),
        });
    }
    let doc = crate::config::ini_parse(&text);
    if !ours && !doc.sections.iter().any(|s| s.starts_with("coverage:")) {
        return None;
    }
    let is_run = |s: &str| s == "coverage:run" || (ours && s == "run");
    // configparser: the LAST occurrence of a key wins.
    let last = |key: &str| -> Vec<String> {
        doc.entries
            .iter()
            .rev()
            .find(|(sec, k, _)| is_run(sec) && k == key)
            .map(|(_, _, f)| split_cov_list(f.iter().map(String::as_str)))
            .unwrap_or_default()
    };
    let mut source = last("source");
    source.extend(last("source_pkgs"));
    source.extend(last("source_dirs"));
    Some(CoverageRun {
        source,
        include: last("include"),
        omit: last("omit"),
    })
}

/// Split coverage list fragments on commas, trimmed, empties dropped.
fn split_cov_list<'a>(fragments: impl Iterator<Item = &'a str>) -> Vec<String> {
    fragments
        .flat_map(|f| f.split(','))
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(String::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sv(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    /// A scope with no coverage config files: a bare `--cov` measures the cwd.
    fn no_cfg() -> &'static Path {
        Path::new("/nonexistent/rstest-no-coverage-config")
    }

    fn sources(scope: &Path, args: &[&str]) -> Vec<String> {
        CovScope::resolve(scope, &sv(args)).sources
    }

    fn partial(scope: &Path, args: &[&str]) -> bool {
        CovScope::resolve(scope, &sv(args)).is_partial()
    }

    /// A fresh project dir holding `files` (relpath, body).
    fn cfg_proj(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rstest-covcfg-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        for (rel, body) in files {
            std::fs::write(d.join(rel), body).unwrap();
        }
        d
    }

    #[test]
    fn cli_sources_narrow_only_for_subtree_values() {
        let s = no_cfg();
        // Whole-tree scopes are not narrowed.
        for args in [
            &["--cov"][..],
            &["--cov=."],
            &["--cov=./"],
            &["--cov="],
            &["--cov=pkg", "--cov=."],
            &["--cov=pkg", "--cov"],
            &["--cov", "--cov=pkg"],
            &["--cov", "--cov-report="],
            &["--cov", "."],
            &["--cov-context=test"],
            &["-n", "2"],
        ] {
            assert!(!partial(s, args), "{args:?}");
        }
        assert_eq!(sources(s, &["--cov=pkg"]), vec!["pkg"]);
        assert_eq!(
            sources(s, &["--cov=pkg", "--cov=src/lib"]),
            vec!["pkg", "src/lib"]
        );
        // Space form: `--cov pkg` consumes `pkg` (nargs="?").
        assert_eq!(sources(s, &["--cov", "./pkg/", "-n"]), vec!["pkg"]);
    }

    #[test]
    fn normalize_scope_strips_dot_slash_and_trailing_slash() {
        assert_eq!(normalize_scope("pkg"), "pkg");
        assert_eq!(normalize_scope("./pkg"), "pkg");
        assert_eq!(normalize_scope("pkg/"), "pkg");
        assert_eq!(normalize_scope("./src/pkg/"), "src/pkg");
        assert_eq!(normalize_scope(".//pkg"), "/pkg"); // only "./" prefixes peel
    }

    #[test]
    fn scope_prefixes_cover_dotted_src_and_absolute_forms() {
        let root = Path::new("/proj");
        let p = |v: &str| scope_prefixes(root, v);
        assert_eq!(
            p("pkg"),
            vec![PathBuf::from("pkg"), PathBuf::from("src/pkg")]
        );
        assert_eq!(
            p("pkg.sub"),
            vec![
                PathBuf::from("pkg.sub"),
                PathBuf::from("pkg/sub"),
                PathBuf::from("src/pkg.sub"),
                PathBuf::from("src/pkg/sub"),
            ]
        );
        assert_eq!(p("src/pkg"), vec![PathBuf::from("src/pkg")]);
        assert_eq!(
            p("/proj/pkg"),
            vec![PathBuf::from("pkg"), PathBuf::from("src/pkg")]
        );
        assert!(p("/elsewhere/pkg").is_empty());
        let dotted = p("pkg.mod");
        assert!(in_prefixes(Path::new("pkg/mod.py"), &dotted));
        assert!(!in_prefixes(Path::new("pkg/other.py"), &dotted));
        assert!(in_prefixes(Path::new("src/pkg/x.py"), &p("pkg")));
        assert!(!in_prefixes(Path::new("pkgx/x.py"), &p("pkg")));
    }

    #[test]
    fn unmatched_sources_names_sources_that_resolve_to_nothing() {
        let scope = cfg_proj("unmatched", &[("single.py", "")]);
        std::fs::create_dir_all(scope.join("src/pkg")).unwrap();
        let cov = CovScope::from_sources(&scope, &["pkg", "single", "nope", "/elsewhere"]);
        assert_eq!(cov.unmatched_sources(&scope), vec!["nope", "/elsewhere"]);
    }

    #[test]
    fn bare_cov_reads_source_from_coverage_config() {
        let d = cfg_proj(
            "rc",
            &[(
                ".coveragerc",
                "[run]\nsource =\n    pkg, lib\nsource_pkgs = api\n",
            )],
        );
        assert_eq!(sources(&d, &["--cov"]), vec!["pkg", "lib", "api"]);
        // An explicit --cov=<v> overrides the config source (pytest-cov CLI wins).
        assert_eq!(sources(&d, &["--cov=other"]), vec!["other"]);
        // A bare --cov next to a value makes pytest-cov's source None -> config.
        assert_eq!(
            sources(&d, &["--cov=other", "--cov"]),
            vec!["pkg", "lib", "api"]
        );
        let d = cfg_proj(
            "setupcfg",
            &[("setup.cfg", "[coverage:run]\nsource = pkg\n")],
        );
        assert_eq!(sources(&d, &["--cov"]), vec!["pkg"]);
        let d = cfg_proj("setupcfg-bare", &[("setup.cfg", "[run]\nsource = pkg\n")]);
        assert!(
            sources(&d, &["--cov"]).is_empty(),
            "[run] in setup.cfg is not coverage's"
        );
        let d = cfg_proj(
            "toml",
            &[(
                "pyproject.toml",
                "[tool.coverage.run]\nsource = [\"pkg\"]\n",
            )],
        );
        assert_eq!(sources(&d, &["--cov"]), vec!["pkg"]);
        let d = cfg_proj("rc-dot", &[(".coveragerc", "[run]\nsource = .\n")]);
        assert!(!partial(&d, &["--cov"]));
    }

    #[test]
    fn coverage_config_lookup_follows_coverage_order() {
        // The first file coverage counts as read wins, even with no source.
        let d = cfg_proj(
            "order",
            &[
                (".coveragerc", "[report]\nshow_missing = true\n"),
                ("setup.cfg", "[coverage:run]\nsource = pkg\n"),
            ],
        );
        assert!(sources(&d, &["--cov"]).is_empty());
        let d = cfg_proj(
            "skip",
            &[
                ("setup.cfg", "[metadata]\nname = x\n"),
                ("tox.ini", "[coverage:run]\nsource = pkg\n"),
            ],
        );
        assert_eq!(sources(&d, &["--cov"]), vec!["pkg"]);
        let d = cfg_proj("named", &[("cov.ini", "[run]\nsource = pkg\n")]);
        assert_eq!(sources(&d, &["--cov", "--cov-config=cov.ini"]), vec!["pkg"]);
        assert_eq!(
            sources(&d, &["--cov-config", "cov.ini", "--cov"]),
            vec!["pkg"]
        );
        let d = cfg_proj(
            "last",
            &[(".coveragerc", "[run]\nsource = a\nsource = b\n")],
        );
        assert_eq!(sources(&d, &["--cov"]), vec!["b"]);
    }

    #[test]
    fn omit_and_include_shrink_the_measured_set() {
        let d = cfg_proj(
            "omit",
            &[(
                ".coveragerc",
                "[run]\nomit =\n    scripts/*\n    */generated_*.py\n",
            )],
        );
        let cov = CovScope::resolve(&d, &sv(&["--cov=."]));
        assert!(cov.is_partial(), "omit makes a whole-tree run partial");
        assert!(!cov.measures(Path::new("scripts/util.py")));
        // Omit `*` crosses `/` (errs toward unmeasured).
        assert!(!cov.measures(Path::new("scripts/deep/util.py")));
        assert!(!cov.measures(Path::new("pkg/generated_pb.py")));
        assert!(cov.measures(Path::new("pkg/mod.py")));
        // Omit applies under a source too.
        let cov = CovScope::resolve(&d, &sv(&["--cov=scripts"]));
        assert!(!cov.measures(Path::new("scripts/util.py")));

        let d = cfg_proj("include", &[(".coveragerc", "[run]\ninclude = pkg/*\n")]);
        let cov = CovScope::resolve(&d, &sv(&["--cov"]));
        assert!(cov.is_partial());
        assert!(cov.measures(Path::new("pkg/mod.py")));
        // Include `*` does NOT cross `/` (errs toward unmeasured).
        assert!(!cov.measures(Path::new("pkg/sub/mod.py")));
        assert!(!cov.measures(Path::new("helper.py")));
        // Include is ignored once a source is set (coverage.py semantics).
        let cov = CovScope::resolve(&d, &sv(&["--cov=helper"]));
        assert!(cov.measures(Path::new("helper.py")));
    }

    #[test]
    fn glob_to_regex_handles_double_star() {
        let re = Regex::new(&glob_to_regex("/p/**/gen.py", false)).unwrap();
        assert!(re.is_match("/p/gen.py"));
        assert!(re.is_match("/p/a/b/gen.py"));
        assert!(!re.is_match("/p/a/xgen.py"));
    }

    #[test]
    fn glob_to_regex_handles_character_classes() {
        let m = |glob: &str, crosses: bool, path: &str| {
            Regex::new(&glob_to_regex(glob, crosses))
                .unwrap()
                .is_match(path)
        };
        assert!(m(
            "*/migrations/[0-9]*.py",
            true,
            "/p/app/migrations/0001_init.py"
        ));
        assert!(!m(
            "*/migrations/[0-9]*.py",
            true,
            "/p/app/migrations/helpers.py"
        ));
        assert!(m("/p/[!_]*.py", true, "/p/mod.py"));
        assert!(!m("/p/[!_]*.py", true, "/p/_private.py"));
        // A leading `]` is a literal member; an unclosed `[` is literal.
        assert!(m("/p/[]x].py", true, "/p/].py"));
        assert!(m("/p/[x.py", true, "/p/[x.py"));
        // Without star_crosses, a negated class never matches `/`.
        assert!(!m("/p[!x]q", false, "/p/q"));
        assert!(m("/p[!x]q", true, "/p/q"));
        // Regex-special members stay literal.
        assert!(m("/p/[\\^]", true, "/p/^"));
    }

    #[test]
    fn omit_with_character_class_marks_files_unmeasured() {
        let d = cfg_proj(
            "omit-class",
            &[(".coveragerc", "[run]\nomit = */migrations/[0-9]*.py\n")],
        );
        let cov = CovScope::resolve(&d, &sv(&["--cov=."]));
        assert!(!cov.measures(Path::new("app/migrations/0001_init.py")));
        assert!(cov.measures(Path::new("app/migrations/helpers.py")));
    }

    #[test]
    fn source_dirs_narrows_like_source() {
        let d = cfg_proj("srcdirs", &[(".coveragerc", "[run]\nsource_dirs = pkg\n")]);
        assert_eq!(sources(&d, &["--cov"]), vec!["pkg"]);
        let d = cfg_proj(
            "srcdirs-toml",
            &[(
                "pyproject.toml",
                "[tool.coverage.run]\nsource_dirs = [\"pkg\"]\n",
            )],
        );
        assert_eq!(sources(&d, &["--cov"]), vec!["pkg"]);
    }

    #[test]
    fn measures_normalizes_backslash_separators() {
        // A Windows walk yields `scripts\util.py`; `omit = scripts/*` must still
        // match it, and a backslash pattern must match a slash path.
        let d = cfg_proj("backslash", &[(".coveragerc", "[run]\nomit = scripts/*\n")]);
        let cov = CovScope::resolve(&d, &sv(&["--cov=."]));
        assert!(!cov.measures(Path::new("scripts\\util.py")));
        assert!(cov.measures(Path::new("pkg\\mod.py")));
        let d = cfg_proj(
            "backslash-pat",
            &[(".coveragerc", "[run]\nomit = scripts\\*\n")],
        );
        let cov = CovScope::resolve(&d, &sv(&["--cov=."]));
        assert!(!cov.measures(Path::new("scripts/util.py")));
        // Sources match component-wise after normalization too.
        let cov = CovScope::from_sources(&d, &["pkg"]);
        assert!(cov.measures(Path::new("pkg\\mod.py")));
    }

    #[test]
    fn src_guess_applies_only_without_a_real_directory() {
        // `./pkg` exists: coverage measures only it, so `src/pkg` is NOT
        // assumed measured. Without `./pkg`, the package may live in `src/`.
        let d = cfg_proj("src-guess", &[]);
        std::fs::create_dir_all(d.join("pkg")).unwrap();
        std::fs::create_dir_all(d.join("src/pkg")).unwrap();
        assert_eq!(scope_prefixes(&d, "pkg"), vec![PathBuf::from("pkg")]);
        let cov = CovScope::from_sources(&d, &["pkg"]);
        assert!(!cov.measures(Path::new("src/pkg/mod.py")));
        std::fs::remove_dir_all(d.join("pkg")).unwrap();
        assert_eq!(
            scope_prefixes(&d, "pkg"),
            vec![PathBuf::from("pkg"), PathBuf::from("src/pkg")]
        );
    }

    #[test]
    fn windows_drive_patterns_are_absolute_and_case_rule_is_explicit() {
        assert!(is_absolute_pattern("C:/proj/scripts/*"));
        assert!(is_absolute_pattern("//server/share/*"));
        assert!(!is_absolute_pattern("scripts/*"));
        let globs = compile_globs_with("C:/proj", &sv(&["C:\\proj\\Scripts\\*"]), true, true);
        assert!(
            globs[0].is_match("C:/proj/scripts/util.py"),
            "drive path + case-insensitive"
        );
        let globs = compile_globs_with("/proj", &sv(&["Scripts/*"]), true, false);
        assert!(
            !globs[0].is_match("/proj/scripts/util.py"),
            "case-sensitive off Windows"
        );
    }

    #[test]
    fn coverage_args_keeps_only_coverage_shaping_flags() {
        assert_eq!(
            coverage_args(&sv(&[
                "tests/",
                "--cov",
                "pkg",
                "-n",
                "2",
                "--cov=lib",
                "--cov-report=",
                "--cov-config",
                "rc.ini",
                "--cov-context=test",
                "--cov"
            ])),
            sv(&[
                "--cov",
                "pkg",
                "--cov=lib",
                "--cov-config",
                "rc.ini",
                "--cov"
            ])
        );
    }

    #[test]
    fn last_cov_config_wins() {
        let d = cfg_proj(
            "cfg-last",
            &[
                ("a.ini", "[run]\nsource = a\n"),
                ("b.ini", "[run]\nsource = b\n"),
            ],
        );
        assert_eq!(
            sources(
                &d,
                &["--cov-config=a.ini", "--cov", "--cov-config", "b.ini"]
            ),
            vec!["b"]
        );
    }

    #[test]
    fn effective_args_prepend_ini_addopts_and_env() {
        let held = crate::test_env::lock();
        let d = cfg_proj(
            "addopts",
            &[("pytest.ini", "[pytest]\naddopts = --cov=pkg\n")],
        );
        let _env = crate::test_env::set_var(&held, "PYTEST_ADDOPTS", "-q --cov-config='my rc'");
        assert_eq!(
            effective_pytest_args(&d, &sv(&["-n", "2"])),
            vec!["--cov=pkg", "-q", "--cov-config=my rc", "-n", "2"]
        );
        let _env = crate::test_env::remove_var(&held, "PYTEST_ADDOPTS");
        assert_eq!(
            sources(
                &d,
                &effective_pytest_args(&d, &[])
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
            ),
            vec!["pkg"]
        );
    }
}
