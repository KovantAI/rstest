//! Cache maintenance around a run: `cache-compact`, retention policy, and the
//! `--cache-pull` / `--cache-push` preflight.

use anyhow::{Context, Result};

use crate::cli::Cli;
use crate::remote;
use crate::reporting::sink::Sink;

/// `cache-compact` subcommand: fold policy-selected remote segments into a
/// fresh base and prune them, then exit without running tests. Resolves the
/// remote from the `--cache-remote` flag or `RSTEST_CACHE_REMOTE`, and the
/// retention window from the flags or `RSTEST_CACHE_KEEP_LAST` /
/// `RSTEST_CACHE_MAX_AGE`.
pub(super) fn run_cache_compact(
    cli: &Cli,
    sink: &mut Sink,
    keep_last: Option<usize>,
    max_age: Option<&str>,
) -> Result<i32> {
    let remote = cli
        .cache_remote
        .clone()
        .or_else(|| std::env::var("RSTEST_CACHE_REMOTE").ok())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("cache-compact needs --cache-remote (or RSTEST_CACHE_REMOTE)")
        })?;
    let policy = resolve_retention_policy(keep_last, max_age)?;
    let t = remote::transport_for(&remote)?;
    let (folded, retained) =
        remote::compact_remote_with(t.as_ref(), sink, crate::time::now_epoch_secs(), &policy)
            .with_context(|| format!("compacting shared cache at {remote}"))?;
    sink.warn(&format!(
        "rstest: cache: compacted {folded} segment(s) into base at {remote} \
         ({retained} retained by policy)"
    ));
    Ok(0)
}

/// Build a [`remote::RetentionPolicy`] from the flags, falling back to
/// `RSTEST_CACHE_KEEP_LAST` / `RSTEST_CACHE_MAX_AGE`. An unparseable flag/env is
/// a hard error (silently folding everything would defeat the intent).
pub(super) fn resolve_retention_policy(
    keep_last: Option<usize>,
    max_age: Option<&str>,
) -> Result<remote::RetentionPolicy> {
    let keep_last = match keep_last {
        Some(n) => Some(n),
        None => match std::env::var("RSTEST_CACHE_KEEP_LAST") {
            Ok(s) if !s.is_empty() => Some(
                s.parse::<usize>()
                    .with_context(|| format!("invalid RSTEST_CACHE_KEEP_LAST {s:?}"))?,
            ),
            _ => None,
        },
    };
    let max_age_raw = max_age.map(str::to_string).or_else(|| {
        std::env::var("RSTEST_CACHE_MAX_AGE")
            .ok()
            .filter(|s| !s.is_empty())
    });
    let max_age = match max_age_raw {
        Some(s) => Some(parse_duration_secs(&s)?),
        None => None,
    };
    Ok(remote::RetentionPolicy { keep_last, max_age })
}

/// Parse a duration: a bare integer is seconds; a trailing `s`/`m`/`h`/`d`/`w`
/// scales it. Used by `cache-compact --max-age`.
pub(super) fn parse_duration_secs(s: &str) -> Result<u64> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: u64 = num.parse().with_context(|| {
        format!("invalid duration {s:?} (expected a number, optional s/m/h/d/w)")
    })?;
    let mult = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        "w" => 7 * 24 * 60 * 60,
        other => anyhow::bail!("invalid duration unit {other:?} in {s:?} (use s/m/h/d/w)"),
    };
    Ok(n.saturating_mul(mult))
}

/// Warm the local cache from the remote (`--cache-pull`) BEFORE anything reads
/// it (scheduling, selection, the regression baseline). No-op without the flag.
pub(super) fn pull_shared_cache(
    cli: &Cli,
    cache_remote: &Option<String>,
    sink: &mut Sink,
) -> Result<()> {
    if !cli.cache_pull {
        return Ok(());
    }
    let remote = cache_remote.as_deref().unwrap(); // validated by preflight_cache
    let t = remote::transport_for(remote)?;
    let merged = remote::pull(t.as_ref(), sink)
        .with_context(|| format!("pulling shared cache from {remote}"))?;
    sink.warn(&format!(
        "rstest: cache: pulled {} duration(s), {} flake record(s), {} history run(s) from {remote}",
        merged.durations.len(),
        merged.flakes.len(),
        merged.history.runs.len()
    ));
    remote::write_local(&merged);
    Ok(())
}

/// The shared-cache flags need a resolved remote. (`cache-compact` is now a
/// run-less subcommand whose combination with `--cache-pull/--cache-push` is
/// rejected by clap, since those flags aren't global.)
pub(super) fn validate_cache_flags(pull: bool, push: bool, remote_present: bool) -> Result<()> {
    if (pull || push) && !remote_present {
        anyhow::bail!("--cache-pull/--cache-push need --cache-remote (or RSTEST_CACHE_REMOTE)");
    }
    Ok(())
}

/// Cache-flag preflight for a normal run: validate that `--cache-pull` /
/// `--cache-push` have a resolved remote, then warn when the `--cache-remote`
/// FLAG is set with neither requested (it would silently do nothing). Gate the
/// warn on the flag, NOT the env-resolved value: `RSTEST_CACHE_REMOTE` is
/// ambient config a CI sets once, and plain runs that don't opt into pull/push
/// must not be nagged every invocation.
pub(super) fn preflight_cache(
    cli: &Cli,
    cache_remote: &Option<String>,
    sink: &mut Sink,
) -> Result<()> {
    validate_cache_flags(cli.cache_pull, cli.cache_push, cache_remote.is_some())?;
    if cli.cache_remote.is_some() && !cli.cache_pull && !cli.cache_push {
        sink.warn(
            "rstest: cache: --cache-remote is set but no --cache-pull/--cache-push \
             was requested; the shared cache is not being used",
        );
    }
    Ok(())
}
