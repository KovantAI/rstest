//! `rstest install-skills`: copy the bundled agent skills into a project.
//!
//! The skills (`migrate-to-rstest`, `rstest-triage`) live in
//! `plugins/rstest/skills/` and ship two ways: as a Claude Code plugin served
//! from the repo's marketplace, and embedded in this binary. The embedded copy
//! is what this subcommand writes, so a `pip install rstest` user gets skills
//! that name exactly the flags and subcommands of the binary they installed.
//! Run-less and interpreter-free, like `explain` / `shard-verify`.
//!
//! Default target is `.claude/skills/` under the current directory (Claude
//! Code's project skills, committed so teammates get them too); `--user` writes
//! `~/.claude/skills/`, `--agents` writes `.agents/skills/` (Codex and other
//! Agent Skills readers), and `--dir` names any directory. An installed skill
//! whose files differ from the bundled copy is left alone unless `--force`;
//! `--force` overwrites the shipped files but never deletes files the user
//! added to a skill directory.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::reporting::sink::Sink;

/// One shipped file: `(skill, path relative to the skill dir, contents)`.
type SkillFile = (&'static str, &'static str, &'static [u8]);

macro_rules! skill_file {
    ($skill:literal, $path:literal) => {
        (
            $skill,
            $path,
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../plugins/rstest/skills/",
                $skill,
                "/",
                $path
            )),
        )
    };
}

/// Every file the skills need at runtime. `evals/` is skill-development only
/// and stays out. `bundle_matches_plugin_dir` (test) fails if this list drifts
/// from `plugins/rstest/skills/`.
const FILES: &[SkillFile] = &[
    skill_file!("migrate-to-rstest", "SKILL.md"),
    skill_file!("migrate-to-rstest", "references/doctor-playbook.md"),
    skill_file!("migrate-to-rstest", "references/fix-playbook.md"),
    skill_file!("migrate-to-rstest", "references/flag-map.md"),
    skill_file!("rstest-triage", "SKILL.md"),
    skill_file!("rstest-triage", "references/commands.md"),
    skill_file!("rstest-triage", "scripts/journal_bisect.py"),
];

/// Where `install-skills` writes, from its flags (`--dir` wins, then `--user`,
/// then `--agents`; the default is the project's `.claude/skills/`).
pub fn target_dir(dir: Option<&Path>, user: bool, agents: bool) -> Result<PathBuf> {
    if let Some(dir) = dir {
        return Ok(dir.to_path_buf());
    }
    let sub = if agents { ".agents" } else { ".claude" };
    let base = if user {
        home_dir().context("cannot locate the home directory (HOME / USERPROFILE unset)")?
    } else {
        std::env::current_dir()?
    };
    Ok(base.join(sub).join("skills"))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// Names of the bundled skills, in install order.
fn skill_names() -> Vec<&'static str> {
    let mut names: Vec<&str> = FILES.iter().map(|(s, _, _)| *s).collect();
    names.dedup();
    names
}

/// State of one skill under the target directory.
#[derive(Debug, PartialEq, Eq)]
enum Status {
    Missing,
    UpToDate,
    Differs,
}

fn status(target: &Path, skill: &str) -> Status {
    let root = target.join(skill);
    if !root.exists() {
        return Status::Missing;
    }
    let same = FILES
        .iter()
        .filter(|(s, _, _)| *s == skill)
        .all(|(_, rel, body)| std::fs::read(root.join(rel)).is_ok_and(|on_disk| on_disk == *body));
    if same {
        Status::UpToDate
    } else {
        Status::Differs
    }
}

/// Install the bundled skills into `target`. Returns the exit code: 0 when
/// every skill is installed or already up to date, 1 when one was skipped
/// because it differs and `--force` was not given.
pub fn run_install(sink: &mut Sink, target: &Path, force: bool) -> Result<i32> {
    if target.exists() && !target.is_dir() {
        bail!("{} exists and is not a directory", target.display());
    }
    let version = env!("CARGO_PKG_VERSION");
    let mut skipped = 0;
    for skill in skill_names() {
        let root = target.join(skill);
        let verb = match status(target, skill) {
            Status::UpToDate => {
                sink.out_line(&format!("  {skill}: up to date"));
                continue;
            }
            Status::Differs if !force => {
                sink.out_line(&format!(
                    "  {skill}: differs from the rstest {version} copy, left alone \
                     (--force to overwrite)"
                ));
                skipped += 1;
                continue;
            }
            Status::Differs => "updated",
            Status::Missing => "installed",
        };
        for (_, rel, body) in FILES.iter().filter(|(s, _, _)| *s == skill) {
            let path = root.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
        }
        sink.out_line(&format!("  {skill}: {verb}"));
    }
    sink.out_line(&format!(
        "rstest {version} skills in {}. Claude Code picks up project and user \
         skills live; if they don't show up, start a new session.",
        target.display()
    ));
    Ok(if skipped > 0 { 1 } else { 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin_skills_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/rstest/skills")
    }

    fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, base, out);
            } else {
                let rel = path.strip_prefix(base).unwrap();
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }

    #[test]
    fn bundle_matches_plugin_dir() {
        // Every shipped file is embedded, and nothing is embedded that the
        // plugin no longer ships. evals/ and eval scratch workspaces are
        // skill-development only.
        let base = plugin_skills_dir();
        let mut on_disk = Vec::new();
        for entry in std::fs::read_dir(&base).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if path.is_dir() && !name.ends_with("-workspace") {
                walk(&path, &base, &mut on_disk);
            }
        }
        on_disk.retain(|p| !p.split('/').nth(1).is_some_and(|d| d == "evals"));
        on_disk.retain(|p| !p.ends_with(".DS_Store"));
        on_disk.sort();
        let mut embedded: Vec<String> = FILES
            .iter()
            .map(|(s, rel, _)| format!("{s}/{rel}"))
            .collect();
        embedded.sort();
        assert_eq!(embedded, on_disk, "update FILES in skills.rs");
    }

    #[test]
    fn install_writes_then_reports_up_to_date() {
        let dir = tempdir();
        let (mut sink, _) = Sink::captured();
        assert_eq!(run_install(&mut sink, &dir, false).unwrap(), 0);
        for (skill, rel, body) in FILES {
            assert_eq!(std::fs::read(dir.join(skill).join(rel)).unwrap(), *body);
        }
        for skill in skill_names() {
            assert_eq!(status(&dir, skill), Status::UpToDate);
        }
        assert_eq!(run_install(&mut sink, &dir, false).unwrap(), 0);
    }

    #[test]
    fn edited_skill_is_kept_unless_forced() {
        let dir = tempdir();
        let (mut sink, _) = Sink::captured();
        run_install(&mut sink, &dir, false).unwrap();
        let skill_md = dir.join("rstest-triage/SKILL.md");
        let extra = dir.join("rstest-triage/notes.md");
        std::fs::write(&skill_md, "local edit").unwrap();
        std::fs::write(&extra, "mine").unwrap();

        assert_eq!(run_install(&mut sink, &dir, false).unwrap(), 1);
        assert_eq!(std::fs::read_to_string(&skill_md).unwrap(), "local edit");

        assert_eq!(run_install(&mut sink, &dir, true).unwrap(), 0);
        assert_eq!(status(&dir, "rstest-triage"), Status::UpToDate);
        // --force overwrites shipped files only; the user's own file survives.
        assert_eq!(std::fs::read_to_string(&extra).unwrap(), "mine");
    }

    #[test]
    fn target_dir_precedence() {
        let explicit = Path::new("/some/where");
        assert_eq!(target_dir(Some(explicit), true, true).unwrap(), explicit);
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(
            target_dir(None, false, false).unwrap(),
            cwd.join(".claude/skills")
        );
        assert_eq!(
            target_dir(None, false, true).unwrap(),
            cwd.join(".agents/skills")
        );
    }

    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "rstest-skills-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
