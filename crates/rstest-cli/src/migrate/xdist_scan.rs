//! The Python source scanner behind `xdist-removal-check`: regexes over a
//! project's sources and installed plugins that find `import xdist` sites,
//! xdist hook implementations, and `hasplugin("xdist")` gates.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use regex::Regex;

use super::xdist_removal::RemovalFinding;

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

fn re_importorskip() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"\bimportorskip\(\s*["']xdist(?:\.[\w.]+)?["']"#).unwrap())
}

fn re_skip() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*(?:pytest\.)?skip\(").unwrap())
}

/// Whether line `idx` sits in the `except ImportError` (or bare `except`)
/// branch of a `try:` whose body imports xdist: a skip there fires exactly
/// when pytest-xdist is gone.
fn skips_without_xdist(lines: &[&str], idx: usize) -> bool {
    let code = |j: usize| {
        let t = lines[j].trim();
        !t.is_empty() && !t.starts_with('#')
    };
    let level = indent(lines[idx]);
    let Some(head) = (0..idx)
        .rev()
        .find(|&j| code(j) && indent(lines[j]) < level)
    else {
        return false;
    };
    let h = lines[head].trim();
    let catches = h == "except:"
        || (h.starts_with("except")
            && (h.contains("ImportError") || h.contains("ModuleNotFoundError")));
    if !catches {
        return false;
    }
    let at = indent(lines[head]);
    let Some(start) = (0..head)
        .rev()
        .find(|&j| code(j) && indent(lines[j]) == at && lines[j].trim() == "try:")
    else {
        return false;
    };
    lines[start + 1..head]
        .iter()
        .any(|l| re_import().is_match(l))
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
pub(super) fn hook_specs(text: &str) -> Vec<String> {
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
pub(super) fn scan_source(
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
        } else if re_importorskip().is_match(line)
            || (re_skip().is_match(line) && skips_without_xdist(&lines, idx))
        {
            // `-p no:xdist` keeps the module importable, so only the scan sees it.
            out.push(RemovalFinding {
                allowed: false,
                blocking: false,
                fix: "drop the skip if the tests don't need xdist itself, or keep \
                      pytest-xdist installed for them"
                    .to_string(),
                kind: "importorskip".to_string(),
                location: at,
                text: snippet,
                why: "skips silently once pytest-xdist is gone: a module-level call skips \
                      every test in the file"
                    .to_string(),
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
pub(super) fn scan_plugin_source(
    text: &str,
    dist: &str,
    path: &str,
    is_entry: bool,
) -> Vec<RemovalFinding> {
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
    use super::import_fix;

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
}
