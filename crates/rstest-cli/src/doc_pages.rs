//! Test-only access to the published docs (`docs/**/*.md`), for tests that
//! hold a page's claim to the code that implements it: a renamed constant or a
//! changed parser then fails the test instead of leaving the docs stale.

use std::path::{Path, PathBuf};

/// Every markdown page under `docs/`, as (repo-relative path, text), sorted.
pub(crate) fn pages() -> Vec<(String, String)> {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crate sits two levels under the repo root");
    let mut files = Vec::new();
    walk(&repo.join("docs"), &mut files);
    files.sort();
    let pages: Vec<(String, String)> = files
        .into_iter()
        .map(|p| {
            let rel = p
                .strip_prefix(repo)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let text = std::fs::read_to_string(&p).unwrap();
            (rel, text)
        })
        .collect();
    assert!(pages.len() > 10, "docs/ not found next to the crate");
    pages
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            walk(&path, out);
        } else if path.extension().is_some_and(|e| e == "md") {
            out.push(path);
        }
    }
}

/// The page's prose split into sentences, each with its whitespace (line
/// breaks included) collapsed to single spaces. A sentence ends at `.`, `:`
/// or `;` followed by whitespace, outside an inline code span, so a command
/// like `rstest --python .venv/bin/python try` stays in one piece. Fenced
/// code blocks are dropped.
pub(crate) fn sentences(text: &str) -> Vec<String> {
    let mut prose = String::new();
    let mut fenced = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
        } else if !fenced {
            prose.push_str(line.trim());
            prose.push(' ');
        }
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_code = false;
    let mut chars = prose.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '`' {
            in_code = !in_code;
        }
        if c.is_whitespace() {
            if !cur.ends_with(' ') && !cur.is_empty() {
                cur.push(' ');
            }
            continue;
        }
        cur.push(c);
        let at_end = chars.peek().is_none_or(|n| n.is_whitespace());
        if !in_code && matches!(c, '.' | ':' | ';') && at_end {
            out.push(std::mem::take(&mut cur).trim().to_string());
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// The inline code spans of `sentence`, without their backticks.
pub(crate) fn code_spans(sentence: &str) -> Vec<&str> {
    sentence.split('`').skip(1).step_by(2).collect()
}

#[cfg(test)]
mod tests {
    use super::{code_spans, sentences};

    #[test]
    fn sentences_keep_code_spans_whole_and_join_lines() {
        let s = sentences(
            "Run `rstest\n--python .venv/bin/python try`: it works. Next\none;\n\n```\nnot. prose\n```\nlast",
        );
        assert_eq!(
            s,
            vec![
                "Run `rstest --python .venv/bin/python try`:",
                "it works.",
                "Next one;",
                "last"
            ]
        );
        assert_eq!(
            code_spans(&s[0]),
            vec!["rstest --python .venv/bin/python try"]
        );
    }
}
