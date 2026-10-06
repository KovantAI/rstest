//! ANSI palette, pytest's scheme: green=pass, red=fail/error,
//! yellow=skip/xfail/xpass. Also the one place that decides what the
//! terminal gets.
//!
//! Color follows pytest's `should_do_markup`: `--color=yes/no` (the last one
//! wins) beats everything; then `PY_COLORS=1/0`; then a non-empty `NO_COLOR`
//! turns it off and a non-empty `FORCE_COLOR` turns it on; else color iff
//! stdout is a tty and `TERM` is not `dumb`. The workers read the same flags
//! and environment, so their assertion diffs agree with this output.
//!
//! The live footer (cursor movement) also needs an interactive terminal:
//! stdout a tty, `TERM` not `dumb`, no `CI` variable, and color on. So
//! `NO_COLOR`, `--color=no` and `TERM=dumb` mean zero escape sequences, and a
//! CI job on a pty (Buildkite, `docker -t`) gets a plain log.

use std::io::IsTerminal;

#[derive(Clone, Copy, Default)]
pub struct Palette {
    enabled: bool,
    /// Interactive terminal: the live per-worker footer may move the
    /// cursor. Implies `enabled`.
    live: bool,
}

/// The terminal decision for a run, from the session args, an environment
/// lookup and whether stdout is a tty. Pure, so tests can drive it.
fn decide(
    session_args: &[String],
    env: impl Fn(&str) -> Option<String>,
    stdout_tty: bool,
) -> Palette {
    let set = |k: &str| env(k).is_some_and(|v| !v.is_empty());
    let dumb = env("TERM").as_deref() == Some("dumb");
    // `--color=X` or `--color X`; the last one wins.
    let value = |i: usize| match session_args[i].as_str() {
        "--color" => session_args.get(i + 1).map(String::as_str),
        a => a.strip_prefix("--color="),
    };
    let forced = (0..session_args.len())
        .rev()
        .find_map(|i| match value(i) {
            Some("yes") => Some(Some(true)),
            Some("no") => Some(Some(false)),
            Some(_) => Some(None), // auto, or a value pytest will reject
            None => None,
        })
        .flatten();
    let enabled = forced.unwrap_or_else(|| match env("PY_COLORS").as_deref() {
        Some("1") => true,
        Some("0") => false,
        _ if set("NO_COLOR") => false,
        _ if set("FORCE_COLOR") => true,
        _ => stdout_tty && !dumb,
    });
    let ci = env("CI").is_some_and(|v| !matches!(v.trim(), "" | "0" | "false" | "False"));
    let live = enabled && stdout_tty && !dumb && !ci;
    Palette { enabled, live }
}

impl Palette {
    pub fn detect(session_args: &[String]) -> Self {
        decide(
            session_args,
            |k| std::env::var(k).ok(),
            std::io::stdout().is_terminal(),
        )
    }

    /// Whether output may move the cursor (the live footer, the bar view):
    /// an interactive, color-capable terminal outside CI.
    pub fn live(&self) -> bool {
        self.live
    }

    fn paint(&self, code: &str, s: &str) -> String {
        if self.enabled {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    pub fn green(&self, s: &str) -> String {
        self.paint("32", s)
    }

    pub fn red(&self, s: &str) -> String {
        self.paint("31", s)
    }

    pub fn bold_red(&self, s: &str) -> String {
        self.paint("31;1", s)
    }

    pub fn yellow(&self, s: &str) -> String {
        self.paint("33", s)
    }

    pub fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }

    /// Color for an outcome word or progress char.
    pub fn outcome(&self, word_or_char: &str) -> String {
        match word_or_char {
            "PASSED" | "." => self.green(word_or_char),
            "FAILED" | "ERROR" | "F" | "E" => self.red(word_or_char),
            _ => self.yellow(word_or_char), // SKIPPED/XFAIL/XPASS/s/x/X
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn forced_color_flags_override() {
        let on = Palette::detect(&v(&["--color=yes"]));
        assert_eq!(on.green("ok"), "\x1b[32mok\x1b[0m");
        let off = Palette::detect(&v(&["--color=no"]));
        assert_eq!(off.green("ok"), "ok");
        // last flag wins (pytest semantics for repeated flags)
        let last = Palette::detect(&v(&["--color=yes", "--color=no"]));
        assert_eq!(last.red("x"), "x");
        // The space-separated form, and a later --color=auto handing the
        // decision back to the environment.
        assert!(decide(&v(&["--color", "yes"]), env(&[]), false).enabled);
        assert!(!decide(&v(&["--color=yes", "--color=auto"]), env(&[]), false).enabled);
    }

    #[test]
    fn outcome_palette() {
        let p = Palette::detect(&v(&["--color=yes"]));
        assert!(p.outcome(".").contains("32"));
        assert!(p.outcome("F").contains("31"));
        assert!(p.outcome("s").contains("33"));
    }

    #[test]
    fn tty_gets_color_and_live_footer() {
        let p = decide(&[], env(&[("TERM", "xterm")]), true);
        assert!(p.enabled && p.live());
        let piped = decide(&[], env(&[]), false);
        assert!(!piped.enabled && !piped.live());
        // --color=yes colors a pipe but never moves its cursor.
        let forced = decide(&v(&["--color=yes"]), env(&[]), false);
        assert!(forced.enabled && !forced.live());
    }

    #[test]
    fn dumb_terminal_gets_no_escapes() {
        let p = decide(&[], env(&[("TERM", "dumb")]), true);
        assert!(!p.enabled && !p.live());
        // Asking for color on a dumb terminal still never moves the cursor.
        let p = decide(&v(&["--color=yes"]), env(&[("TERM", "dumb")]), true);
        assert!(p.enabled && !p.live());
    }

    #[test]
    fn no_color_and_color_no_disable_the_footer_too() {
        let p = decide(&[], env(&[("NO_COLOR", "1")]), true);
        assert!(!p.enabled && !p.live());
        let p = decide(&v(&["--color=no"]), env(&[]), true);
        assert!(!p.enabled && !p.live());
        // An empty NO_COLOR does not count (no-color.org, pytest).
        assert!(decide(&[], env(&[("NO_COLOR", "")]), true).enabled);
    }

    #[test]
    fn force_color_and_py_colors_color_a_pipe() {
        let p = decide(&[], env(&[("FORCE_COLOR", "1")]), false);
        assert!(p.enabled && !p.live());
        assert!(decide(&[], env(&[("PY_COLORS", "1")]), false).enabled);
        assert!(!decide(&[], env(&[("PY_COLORS", "0")]), true).enabled);
        // pytest's order: PY_COLORS beats NO_COLOR, NO_COLOR beats FORCE_COLOR.
        assert!(decide(&[], env(&[("PY_COLORS", "1"), ("NO_COLOR", "1")]), false).enabled);
        assert!(!decide(&[], env(&[("NO_COLOR", "1"), ("FORCE_COLOR", "1")]), true).enabled);
        // The flag beats the environment.
        assert!(!decide(&v(&["--color=no"]), env(&[("FORCE_COLOR", "1")]), true).enabled);
    }

    #[test]
    fn ci_on_a_pty_keeps_color_but_no_live_footer() {
        let p = decide(&[], env(&[("CI", "true")]), true);
        assert!(p.enabled && !p.live());
        assert!(decide(&[], env(&[("CI", "false")]), true).live());
    }
}
