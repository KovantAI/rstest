//! The `--python` / `.python-version` grammar: parse a value into either a
//! concrete interpreter path or a version-and-implementation request, and match
//! a probed interpreter against a request.

use std::path::{Path, PathBuf};

use super::probe::Probe;

/// What a `--python` value or `.python-version` entry resolves to: either a
/// concrete interpreter path (authoritative - probed, never fallen back from)
/// or a version/implementation request matched against discovered candidates.
#[derive(Debug, PartialEq)]
pub(super) enum PyArg {
    Path(PathBuf),
    Request(Request),
}

/// A version-and-implementation request, e.g. `>=3.12,<3.13`, `pypy@3.10`,
/// `3.13t`. All constraints are ANDed.
#[derive(Debug, Default, Clone, PartialEq)]
pub(super) struct Request {
    /// `cpython`, `pypy`, ... matched case-insensitively. None = any.
    implementation: Option<String>,
    /// Free-threaded build required (the `t` suffix, e.g. `3.13t`).
    freethreaded: bool,
    constraints: Vec<Constraint>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Op {
    Eq,
    Ge,
    Le,
    Gt,
    Lt,
}

/// A single version constraint at the precision the user wrote: `3` pins only
/// major, `3.12` major+minor, `3.12.4` all three.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Constraint {
    op: Op,
    major: u8,
    minor: Option<u8>,
    micro: Option<u8>,
}

impl Constraint {
    fn matches(&self, v: (u8, u8, u8)) -> bool {
        let target = (self.major, self.minor.unwrap_or(0), self.micro.unwrap_or(0));
        match self.op {
            // Equality compares only the components the user specified, so a
            // bare `3.12` matches any 3.12.x.
            Op::Eq => {
                v.0 == self.major
                    && self.minor.is_none_or(|m| v.1 == m)
                    && self.micro.is_none_or(|m| v.2 == m)
            }
            Op::Ge => v >= target,
            Op::Le => v <= target,
            Op::Gt => v > target,
            Op::Lt => v < target,
        }
    }
}

impl std::fmt::Display for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(i) = &self.implementation {
            write!(f, "{i}@")?;
        }
        let parts: Vec<String> = self
            .constraints
            .iter()
            .map(|c| {
                let op = match c.op {
                    Op::Eq => "",
                    Op::Ge => ">=",
                    Op::Le => "<=",
                    Op::Gt => ">",
                    Op::Lt => "<",
                };
                let mut s = format!("{op}{}", c.major);
                if let Some(m) = c.minor {
                    s.push_str(&format!(".{m}"));
                }
                if let Some(m) = c.micro {
                    s.push_str(&format!(".{m}"));
                }
                s
            })
            .collect();
        write!(f, "{}", parts.join(","))?;
        if self.freethreaded {
            write!(f, "t")?;
        }
        Ok(())
    }
}

/// Does a probed interpreter satisfy a request?
pub(super) fn matches(p: &Probe, r: &Request) -> bool {
    if let Some(want) = &r.implementation {
        if !p.implementation.eq_ignore_ascii_case(want) {
            return false;
        }
    }
    if r.freethreaded && !p.freethreaded {
        return false;
    }
    r.constraints.iter().all(|c| c.matches(p.version))
}

/// Interpret a `--python` / `.python-version` value. An existing path is taken
/// verbatim; otherwise we try to parse a version request; failing that we still
/// treat it as a path so the probe step produces a clear "not runnable" error.
pub(super) fn parse_pyarg(s: &str) -> PyArg {
    let s = s.trim();
    if Path::new(s).exists() {
        return PyArg::Path(PathBuf::from(s));
    }
    match parse_request(s) {
        Some(r) => PyArg::Request(r),
        None => PyArg::Path(PathBuf::from(s)),
    }
}

/// Parse `[impl@]constraints[t]`, e.g. `pypy@>=3.10,<3.12`, `3.13t`, `3`.
/// None when the version portion isn't numeric (so the caller can fall back to
/// treating the whole string as a path).
fn parse_request(s: &str) -> Option<Request> {
    let mut req = Request::default();
    let ver_part = match s.split_once('@') {
        Some((impl_, rest)) => {
            req.implementation = Some(impl_.to_ascii_lowercase());
            rest
        }
        // A leading letter with no '@' is a bare implementation name (`pypy`).
        None if s.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) => {
            req.implementation = Some(s.to_ascii_lowercase());
            ""
        }
        None => s,
    };

    let ver_part = match ver_part.strip_suffix('t') {
        // `t` is the free-threaded marker only after a digit (`3.13t`), not a
        // stray trailing letter.
        Some(head) if head.chars().last().is_some_and(|c| c.is_ascii_digit()) => {
            req.freethreaded = true;
            head
        }
        _ => ver_part,
    };

    if !ver_part.is_empty() {
        for tok in ver_part.split(',') {
            req.constraints.push(parse_constraint(tok)?);
        }
    }

    // Reject an empty request (no impl, no constraints): that's not a spec.
    if req.implementation.is_none() && req.constraints.is_empty() {
        return None;
    }
    Some(req)
}

fn parse_constraint(tok: &str) -> Option<Constraint> {
    let tok = tok.trim();
    let (op, rest) = if let Some(r) = tok.strip_prefix(">=") {
        (Op::Ge, r)
    } else if let Some(r) = tok.strip_prefix("<=") {
        (Op::Le, r)
    } else if let Some(r) = tok.strip_prefix("==") {
        (Op::Eq, r)
    } else if let Some(r) = tok.strip_prefix('>') {
        (Op::Gt, r)
    } else if let Some(r) = tok.strip_prefix('<') {
        (Op::Lt, r)
    } else {
        (Op::Eq, tok)
    };
    let mut nums = rest.split('.');
    let major = nums.next()?.parse().ok()?;
    let minor = nums.next().map(|s| s.parse()).transpose().ok()?;
    let micro = nums.next().map(|s| s.parse()).transpose().ok()?;
    if nums.next().is_some() {
        return None; // too many components
    }
    Some(Constraint {
        op,
        major,
        minor,
        micro,
    })
}

#[cfg(test)]
mod tests {
    use super::{parse_constraint, parse_request, Op};

    #[test]
    fn constraint_parses_operator_and_precision() {
        let c = parse_constraint(">=3.12").unwrap();
        assert_eq!(
            (c.op, c.major, c.minor, c.micro),
            (Op::Ge, 3, Some(12), None)
        );

        let c = parse_constraint("<=3").unwrap();
        assert_eq!((c.op, c.major, c.minor, c.micro), (Op::Le, 3, None, None));

        let c = parse_constraint("==3.11.4").unwrap();
        assert_eq!(
            (c.op, c.major, c.minor, c.micro),
            (Op::Eq, 3, Some(11), Some(4))
        );

        // No operator => equality at the written precision.
        let c = parse_constraint("3.13").unwrap();
        assert_eq!(
            (c.op, c.major, c.minor, c.micro),
            (Op::Eq, 3, Some(13), None)
        );

        assert_eq!(parse_constraint(">3").unwrap().op, Op::Gt);
        assert_eq!(parse_constraint("<3.13").unwrap().op, Op::Lt);
    }

    #[test]
    fn constraint_rejects_garbage_and_overlong() {
        assert!(parse_constraint("abc").is_none());
        assert!(parse_constraint(">=x").is_none());
        assert!(parse_constraint("3.12.4.5").is_none()); // too many components
        assert!(parse_constraint("").is_none());
    }

    #[test]
    fn constraint_matches_respects_written_precision() {
        // Bare `3.12` (Eq) matches any 3.12.x but not 3.13.
        let c = parse_constraint("3.12").unwrap();
        assert!(c.matches((3, 12, 0)));
        assert!(c.matches((3, 12, 9)));
        assert!(!c.matches((3, 13, 0)));
        assert!(!c.matches((3, 11, 9)));

        // Full-precision Eq pins all three.
        let c = parse_constraint("==3.12.4").unwrap();
        assert!(c.matches((3, 12, 4)));
        assert!(!c.matches((3, 12, 5)));
    }

    #[test]
    fn constraint_matches_ordered_ops() {
        assert!(parse_constraint(">=3.12").unwrap().matches((3, 13, 0)));
        assert!(!parse_constraint(">=3.12").unwrap().matches((3, 11, 9)));
        assert!(parse_constraint("<3.13").unwrap().matches((3, 12, 5)));
        assert!(!parse_constraint("<3.13").unwrap().matches((3, 13, 0)));
        assert!(parse_constraint(">3.12").unwrap().matches((3, 12, 1)));
        assert!(!parse_constraint(">3.12").unwrap().matches((3, 12, 0)));
    }

    #[test]
    fn request_parses_impl_freethreaded_and_constraints() {
        // `3.13t`: free-threaded marker after a digit, single Eq constraint.
        let r = parse_request("3.13t").unwrap();
        assert!(r.implementation.is_none());
        assert!(r.freethreaded);
        assert_eq!(r.constraints.len(), 1);

        // impl@ range: two ANDed constraints, implementation lowercased.
        let r = parse_request("PyPy@>=3.10,<3.12").unwrap();
        assert_eq!(r.implementation.as_deref(), Some("pypy"));
        assert!(!r.freethreaded);
        assert_eq!(r.constraints.len(), 2);

        // Bare implementation name, no constraints.
        let r = parse_request("pypy").unwrap();
        assert_eq!(r.implementation.as_deref(), Some("pypy"));
        assert!(r.constraints.is_empty());

        // impl + free-threaded + version.
        let r = parse_request("pypy@3.10t").unwrap();
        assert_eq!(r.implementation.as_deref(), Some("pypy"));
        assert!(r.freethreaded);
        assert_eq!(r.constraints.len(), 1);
    }

    #[test]
    fn request_rejects_non_specs() {
        assert!(parse_request("").is_none()); // empty => not a spec
        assert!(parse_request("3.13.4.5").is_none()); // bad constraint kills whole parse
    }

    #[test]
    fn request_display_roundtrips() {
        // Display is the inverse of parse for these canonical forms.
        assert_eq!(
            parse_request(">=3.12,<3.13").unwrap().to_string(),
            ">=3.12,<3.13"
        );
        assert_eq!(
            parse_request("pypy@3.13t").unwrap().to_string(),
            "pypy@3.13t"
        );
        assert_eq!(parse_request("==3.11.4").unwrap().to_string(), "3.11.4");
    }
}
