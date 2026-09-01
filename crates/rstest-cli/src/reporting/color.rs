//! ANSI palette, pytest's scheme: green=pass, red=fail/error,
//! yellow=skip/xfail/xpass. Enabled on a tty unless NO_COLOR is set;
//! the session's --color=yes/no flag (forwarded to pytest anyway)
//! overrides both directions.

use std::io::IsTerminal;

#[derive(Clone, Copy, Default)]
pub struct Palette {
    enabled: bool,
}

impl Palette {
    pub fn detect(session_args: &[String]) -> Self {
        let forced = session_args.iter().rev().find_map(|a| match a.as_str() {
            "--color=yes" => Some(true),
            "--color=no" => Some(false),
            _ => None,
        });
        let enabled = forced.unwrap_or_else(|| {
            std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
        });
        Self { enabled }
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

    #[test]
    fn forced_color_flags_override() {
        let on = Palette::detect(&v(&["--color=yes"]));
        assert_eq!(on.green("ok"), "\x1b[32mok\x1b[0m");
        let off = Palette::detect(&v(&["--color=no"]));
        assert_eq!(off.green("ok"), "ok");
        // last flag wins (pytest semantics for repeated flags)
        let last = Palette::detect(&v(&["--color=yes", "--color=no"]));
        assert_eq!(last.red("x"), "x");
    }

    #[test]
    fn outcome_palette() {
        let p = Palette::detect(&v(&["--color=yes"]));
        assert!(p.outcome(".").contains("32"));
        assert!(p.outcome("F").contains("31"));
        assert!(p.outcome("s").contains("33"));
    }
}
