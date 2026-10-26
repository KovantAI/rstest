//! `rstest report`: a self-contained HTML dashboard over the per-test run
//! history (`.rstest_cache/history.msgpack`), without running anything. Where
//! `--html` shows one run, this shows the trend across runs: failing and
//! flaky tests per run, suite time per run, the flaky ranking (`rstest
//! flaky`), the slowest tests, duration drift (the `--doctor` rule) and the
//! runs themselves.
//!
//! One file, no external assets, readable with JavaScript off (dates then
//! read "3h ago"; the script localizes them and adds chart tooltips). Every
//! nodeid goes through [`esc`]; the tooltip script inserts text with
//! `textContent` only.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::explain::{ago, glyph, median};
use crate::reporting::history::{Entry, History, RunMeta};
use crate::reporting::html::{esc, CSS};
use crate::reporting::sink::Sink;

/// Default output, inside the cache dir so it never litters the project.
pub const FILE: &str = "report.html";
/// Rows in the flaky / slowest / drift tables.
const TOP: usize = 20;
/// Outcomes per test in the flaky table's strip.
const STRIP: usize = 20;

fn is_fail(o: &str) -> bool {
    matches!(o, "failed" | "errors" | "crashed" | "quarantined")
}

fn is_pass(o: &str) -> bool {
    matches!(o, "passed" | "flaky")
}

/// One recorded run, from the entries that still point at it.
#[derive(Debug, PartialEq)]
struct RunStat {
    epoch: u64,
    commit: Option<String>,
    tests: usize,
    failed: usize,
    flaky: usize,
    /// Summed call-phase seconds of the tests that recorded one.
    call_seconds: f64,
}

/// Per-run totals, oldest first. A run's oldest entries are trimmed per test
/// (`RSTEST_HISTORY_RUNS`), so the earliest runs can be partial.
fn run_stats(h: &History) -> Vec<RunStat> {
    let mut stats: Vec<RunStat> = h
        .runs
        .iter()
        .map(|r: &RunMeta| RunStat {
            epoch: r.epoch,
            commit: r.commit.clone(),
            tests: 0,
            failed: 0,
            flaky: 0,
            call_seconds: 0.0,
        })
        .collect();
    let index: std::collections::HashMap<u64, usize> =
        h.runs.iter().enumerate().map(|(i, r)| (r.seq, i)).collect();
    for e in h.tests.values().flatten() {
        let Some(&i) = index.get(&e.run) else {
            continue;
        };
        let s = &mut stats[i];
        s.tests += 1;
        s.failed += usize::from(is_fail(e.outcome_name()));
        s.flaky += usize::from(e.outcome_name() == "flaky");
        s.call_seconds += e.call.map_or(0.0, f64::from);
    }
    stats.sort_by_key(|s| s.epoch);
    stats
}

fn passing_calls(series: &std::collections::VecDeque<Entry>) -> Vec<f64> {
    series
        .iter()
        .filter(|e| is_pass(e.outcome_name()))
        .filter_map(|e| e.call.map(f64::from))
        .collect()
}

/// `(nodeid, median, p95, runs)` of the slowest tests by median call time,
/// over tests with at least 3 passing runs and a median of at least 1ms.
fn slowest(h: &History) -> Vec<(String, f64, f64, usize)> {
    let mut rows: Vec<(String, f64, f64, usize)> = h
        .tests
        .iter()
        .filter_map(|(id, s)| {
            let mut calls = passing_calls(s);
            if calls.len() < 3 {
                return None;
            }
            calls.sort_by(f64::total_cmp);
            let rank = (calls.len() * 95).div_ceil(100);
            let med = median(&calls).filter(|m| *m >= 0.001)?;
            Some((id.clone(), med, calls[rank - 1], calls.len()))
        })
        .collect();
    rows.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    rows.truncate(TOP);
    rows
}

/// `(nodeid, older, middle, newer, runs)` for tests the doctor's drift rule
/// catches, largest rise first.
fn drifting(h: &History) -> Vec<(String, f64, f64, f64, usize)> {
    let mut rows: Vec<(String, f64, f64, f64, usize)> = h
        .tests
        .iter()
        .filter_map(|(id, s)| {
            let calls = passing_calls(s);
            let (a, b, c) = crate::doctor::drift_of(&calls)?;
            Some((id.clone(), a, b, c, calls.len()))
        })
        .collect();
    rows.sort_by(|x, y| {
        (y.3 - y.1)
            .total_cmp(&(x.3 - x.1))
            .then_with(|| x.0.cmp(&y.0))
    });
    rows.truncate(TOP);
    rows
}

/// A tick step of 1, 2 or 5 x 10^k giving at most 4 steps up to `max`.
fn nice_step(max: f64) -> f64 {
    if max <= 0.0 {
        return 1.0;
    }
    let raw = max / 4.0;
    let mag = 10f64.powf(raw.log10().floor());
    [1.0, 2.0, 5.0, 10.0]
        .iter()
        .map(|m| m * mag)
        .find(|s| *s >= raw)
        .unwrap_or(10.0 * mag)
}

/// A tick label with no float noise: at most 2 decimals, trailing zeros cut.
fn fmt_tick(v: f64) -> String {
    let s = format!("{v:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// One column: stacked `(value, class)` segments bottom-up, and the tooltip
/// lines (first one is the heading).
struct Column {
    segments: Vec<(f64, &'static str)>,
    tip: Vec<String>,
    epoch: u64,
}

/// An SVG column chart: one baseline, columns <= 24px with a rounded top, a
/// 2px surface gap between stacked segments, hairline gridlines, and a
/// focusable hit area per column carrying its tooltip.
/// `whole`: the values are counts, so ticks step by whole numbers.
fn columns_svg(label: &str, cols: &[Column], unit: &str, whole: bool) -> String {
    const W: f64 = 760.0;
    const H: f64 = 200.0;
    const LEFT: f64 = 44.0;
    const TOP_PAD: f64 = 8.0;
    const BOTTOM: f64 = 8.0;
    let plot_w = W - LEFT - 4.0;
    let plot_h = H - TOP_PAD - BOTTOM;
    let max = cols
        .iter()
        .map(|c| c.segments.iter().map(|s| s.0).sum::<f64>())
        .fold(0.0, f64::max);
    let step = if whole {
        nice_step(max).max(1.0)
    } else {
        nice_step(max)
    };
    let top = (max / step).ceil().max(1.0) * step;
    let y = |v: f64| TOP_PAD + plot_h - v / top * plot_h;
    let mut svg = String::new();
    let _ = write!(
        svg,
        "<svg class=\"chart\" viewBox=\"0 0 {W} {H}\" role=\"img\" aria-label=\"{}\">",
        esc(label)
    );
    // Ticks by index, so they never accumulate float error.
    for k in 0..=(top / step).round() as u32 {
        let t = f64::from(k) * step;
        let _ = write!(
            svg,
            "<line class=\"grid\" x1=\"{LEFT}\" x2=\"{}\" y1=\"{yy:.1}\" y2=\"{yy:.1}\"/>\
             <text class=\"tick\" x=\"{}\" y=\"{:.1}\">{}{unit}</text>",
            W - 4.0,
            LEFT - 6.0,
            y(t) + 4.0,
            fmt_tick(t),
            yy = y(t),
        );
    }
    let slot = plot_w / cols.len().max(1) as f64;
    let bar = (slot * 0.7).clamp(1.0, 24.0);
    for (i, c) in cols.iter().enumerate() {
        let x0 = LEFT + slot * i as f64;
        let x = x0 + (slot - bar) / 2.0;
        let tip = esc(&c.tip.join("\n"));
        let _ = write!(
            svg,
            "<g class=\"col\" tabindex=\"0\" data-tip=\"{tip}\" data-epoch=\"{}\">\
             <rect class=\"hit\" x=\"{x0:.1}\" y=\"{TOP_PAD}\" width=\"{slot:.1}\" height=\"{plot_h}\"/>",
            c.epoch
        );
        let mut base = 0.0;
        let drawn: Vec<&(f64, &str)> = c.segments.iter().filter(|s| s.0 > 0.0).collect();
        for (k, (v, class)) in drawn.iter().enumerate() {
            let last = k + 1 == drawn.len();
            let y_top = y(base + v);
            // 2px surface gap above every segment but the top one.
            let y_bot = y(base) - if k > 0 { 2.0 } else { 0.0 };
            let h = (y_bot - y_top).max(0.5);
            let r = if last {
                4.0_f64.min(bar / 2.0).min(h)
            } else {
                0.0
            };
            let _ = write!(
                svg,
                "<path class=\"{class}\" d=\"M{x:.1},{y_bot:.1} V{:.1} Q{x:.1},{y_top:.1} {:.1},{y_top:.1} \
                 H{:.1} Q{:.1},{y_top:.1} {:.1},{:.1} V{y_bot:.1} Z\"/>",
                y_top + r,
                x + r,
                x + bar - r,
                x + bar,
                x + bar,
                y_top + r,
            );
            base += v;
        }
        svg.push_str("</g>");
    }
    let _ = write!(
        svg,
        "<line class=\"axis\" x1=\"{LEFT}\" x2=\"{}\" y1=\"{yy:.1}\" y2=\"{yy:.1}\"/></svg>",
        W - 4.0,
        yy = y(0.0),
    );
    svg
}

fn short(c: &Option<String>) -> String {
    c.as_deref()
        .map_or("-".into(), |c| c.chars().take(10).collect())
}

/// A date cell: "3h ago" without JS, localized by the script.
fn when(epoch: u64, now: u64) -> String {
    format!(
        "<time data-epoch=\"{epoch}\">{}</time>",
        esc(&ago(epoch, now))
    )
}

#[allow(clippy::write_with_newline)]
fn render(
    h: &History,
    auto: u32,
    green: Option<&crate::since_green::Snapshot>,
    now: u64,
) -> String {
    let runs = run_stats(h);
    let flaky = crate::flaky::gather(h, auto, TOP);
    let caught = h.flippers(auto.max(1)).len() * usize::from(auto > 0);
    let mut html = String::new();
    html.push_str(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>rstest history</title>\n<style>\n",
    );
    html.push_str(CSS);
    html.push_str(DASH_CSS);
    html.push_str("\n</style>\n</head>\n<body>\n<header>\n<h1>rstest history</h1>\n");
    let (first, last) = (
        runs.first().map_or(0, |r| r.epoch),
        runs.last().map_or(0, |r| r.epoch),
    );
    let _ = write!(
        html,
        "<p class=\"meta\">{} run{} recorded, {} to {}</p>\n",
        runs.len(),
        if runs.len() == 1 { "" } else { "s" },
        when(first, now),
        when(last, now),
    );
    let green_tile = match green {
        Some(g) => format!(
            "{}<small>{}</small>",
            esc(&short(&g.commit)),
            when(g.epoch, now)
        ),
        None => "none yet".into(),
    };
    let _ = write!(
        html,
        "<div class=\"tiles\">\
         <div class=\"tile\"><span>Tests tracked</span><b>{}</b></div>\
         <div class=\"tile\"><span>Flaky tests</span><b>{}</b></div>\
         <div class=\"tile\"><span>Auto-quarantined next run</span><b>{}</b></div>\
         <div class=\"tile\"><span>Last green run</span><b>{green_tile}</b></div>\
         </div>\n</header>\n",
        h.tests.len(),
        flaky.flaky_tests,
        caught,
    );

    // ---- Charts ----------------------------------------------------------
    let fail_cols: Vec<Column> = runs
        .iter()
        .map(|r| Column {
            segments: vec![(r.failed as f64, "s-fail"), (r.flaky as f64, "s-flaky")],
            tip: vec![
                short(&r.commit),
                format!("{} failed", r.failed),
                format!("{} flaky", r.flaky),
                format!("{} tests", r.tests),
            ],
            epoch: r.epoch,
        })
        .collect();
    let time_cols: Vec<Column> = runs
        .iter()
        .map(|r| Column {
            segments: vec![(r.call_seconds, "s-time")],
            tip: vec![
                short(&r.commit),
                format!("{:.2}s of test call time", r.call_seconds),
                format!("{} tests", r.tests),
            ],
            epoch: r.epoch,
        })
        .collect();
    let _ = write!(
        html,
        "<section>\n<h2>Failing and flaky tests per run</h2>\n\
         <p class=\"legend\"><span class=\"key s-fail\"></span>failed \
         <span class=\"key s-flaky\"></span>flaky (passed on rerun)</p>\n{}\n</section>\n",
        columns_svg(
            "Failing and flaky tests per run, oldest first",
            &fail_cols,
            "",
            true
        )
    );
    let _ = write!(
        html,
        "<section>\n<h2>Test call time per run</h2>\n\
         <p class=\"muted\">Summed call phase of the tests each run recorded; fixture time not included.</p>\n{}\n</section>\n",
        columns_svg(
            "Test call time per run in seconds, oldest first",
            &time_cols,
            "s",
            false
        )
    );

    // ---- Flaky -----------------------------------------------------------
    html.push_str("<section>\n<h2>Flaky tests</h2>\n");
    if flaky.tests.is_empty() {
        html.push_str("<p class=\"muted\">No test flipped between pass and fail.</p>\n");
    } else {
        html.push_str(
            "<table><thead><tr><th class=\"num\">flips</th><th>trend</th><th>recent runs</th>\
             <th class=\"num\">flaky</th><th class=\"num\">failed</th><th>last flip</th>\
             <th>test</th></tr></thead><tbody>\n",
        );
        for t in &flaky.tests {
            let strip: String = t
                .outcomes
                .iter()
                .skip(t.outcomes.len().saturating_sub(STRIP))
                .map(|o| glyph(o))
                .collect();
            let badge = if t.auto_quarantined {
                " <span class=\"badge auto\">auto-quarantined</span>"
            } else {
                ""
            };
            let _ = write!(
                html,
                "<tr><td class=\"num\">{}</td><td>{}</td><td class=\"strip\">{strip}</td>\
                 <td class=\"num\">{}</td><td class=\"num\">{}</td><td>{}</td>\
                 <td class=\"nid\">{}{badge}</td></tr>\n",
                t.flips,
                t.trend,
                t.flaky,
                t.failed,
                when(t.last_flip_epoch, now),
                esc(&t.nodeid),
            );
        }
        html.push_str("</tbody></table>\n");
        if flaky.flaky_tests > flaky.tests.len() {
            let _ = write!(
                html,
                "<p class=\"muted\">{} more: <code>rstest flaky --top 0</code> lists all.</p>\n",
                flaky.flaky_tests - flaky.tests.len()
            );
        }
    }
    html.push_str("</section>\n");

    // ---- Slowest ---------------------------------------------------------
    let slow = slowest(h);
    if !slow.is_empty() {
        html.push_str(
            "<section>\n<h2>Slowest tests</h2>\n<p class=\"muted\">Median call time over passing runs.</p>\n\
             <table><thead><tr><th class=\"num\">median</th><th class=\"num\">p95</th>\
             <th class=\"num\">runs</th><th>test</th></tr></thead><tbody>\n",
        );
        for (id, med, p95, n) in &slow {
            let _ = write!(
                html,
                "<tr><td class=\"num\">{med:.3}s</td><td class=\"num\">{p95:.3}s</td>\
                 <td class=\"num\">{n}</td><td class=\"nid\">{}</td></tr>\n",
                esc(id)
            );
        }
        html.push_str("</tbody></table>\n</section>\n");
    }

    // ---- Drift -----------------------------------------------------------
    let drift = drifting(h);
    if !drift.is_empty() {
        html.push_str(
            "<section>\n<h2>Duration drift</h2>\n<p class=\"muted\">Median call time of the \
             older, middle and newer third of each test's passing runs, rising step by step: \
             too slow a creep for <code>--durations-regress</code>.</p>\n\
             <table><thead><tr><th class=\"num\">older</th><th class=\"num\">middle</th>\
             <th class=\"num\">newer</th><th class=\"num\">runs</th><th>test</th></tr></thead><tbody>\n",
        );
        for (id, a, b, c, n) in &drift {
            let _ = write!(
                html,
                "<tr><td class=\"num\">{a:.3}s</td><td class=\"num\">{b:.3}s</td>\
                 <td class=\"num\">{c:.3}s</td><td class=\"num\">{n}</td><td class=\"nid\">{}</td></tr>\n",
                esc(id)
            );
        }
        html.push_str("</tbody></table>\n</section>\n");
    }

    // ---- Runs (the charts' table view) -----------------------------------
    html.push_str(
        "<section>\n<h2>Runs</h2>\n<table><thead><tr><th>when</th><th>commit</th>\
         <th class=\"num\">tests</th><th class=\"num\">failed</th>\
         <th class=\"num\">flaky</th><th class=\"num\">call time</th></tr></thead><tbody>\n",
    );
    for r in runs.iter().rev() {
        let _ = write!(
            html,
            "<tr><td>{}</td><td><code>{}</code></td>\
             <td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td>\
             <td class=\"num\">{:.2}s</td></tr>\n",
            when(r.epoch, now),
            esc(&short(&r.commit)),
            r.tests,
            r.failed,
            r.flaky,
            r.call_seconds,
        );
    }
    html.push_str("</tbody></table>\n<p class=\"muted\">The oldest runs can be partial: each test keeps only its newest recorded runs.</p>\n</section>\n");
    let _ = write!(
        html,
        "<div id=\"tip\" role=\"tooltip\" hidden></div>\n<script>\n{DASH_JS}\n</script>\n</body>\n</html>\n"
    );
    html
}

/// `rstest report [--out PATH]`.
pub fn run_report(sink: &mut Sink, out: Option<&Path>) -> Result<i32> {
    let h = crate::reporting::history::load();
    if h.runs.is_empty() {
        sink.out_line(
            "rstest: report: no run history yet (.rstest_cache/history.msgpack is written by every run)",
        );
        return Ok(0);
    }
    let cwd = std::env::current_dir()?;
    let auto = crate::config::rstest_settings(&cwd, &mut std::io::sink())
        .auto_quarantine
        .unwrap_or(crate::cli::AUTO_QUARANTINE);
    let html = render(
        &h,
        auto,
        crate::since_green::load().as_ref(),
        crate::time::now_epoch_secs(),
    );
    let path: PathBuf = out.map_or_else(|| crate::cache::file(FILE), Path::to_path_buf);
    crate::reporting::write_output(&path, html)?;
    sink.out_line(&format!("rstest: report: wrote {}", path.display()));
    Ok(0)
}

const DASH_CSS: &str = r#"
:root { --c-fail:#cf222e; --c-flaky:#0969da; --c-time:#4a3aa7; }
@media (prefers-color-scheme: dark) {
  :root { --c-fail:#f85149; --c-flaky:#3987e5; --c-time:#9085e9; } }
.tiles { display:grid; grid-template-columns:repeat(auto-fit,minmax(160px,1fr)); gap:.6rem; margin:.8rem 0; }
.tile { background:var(--card); border:1px solid var(--line); border-radius:8px; padding:.6rem .8rem; }
.tile span { display:block; color:var(--muted); font-size:.85rem; }
.tile b { font-size:1.5rem; font-weight:600; }
.tile small { display:block; font-size:.8rem; color:var(--muted); font-weight:400; }
svg.chart { width:100%; height:auto; display:block; }
.chart .grid { stroke:var(--line); stroke-width:1; }
.chart .axis { stroke:var(--muted); stroke-width:1; }
.chart .tick { fill:var(--muted); font-size:11px; text-anchor:end; font-variant-numeric:tabular-nums; }
.chart .hit { fill:transparent; }
.chart .col:hover path, .chart .col:focus path { opacity:.75; }
.chart .col:focus { outline:none; }
.chart .col:focus .hit { fill:var(--line); opacity:.4; }
.s-fail { fill:var(--c-fail); } .s-flaky { fill:var(--c-flaky); } .s-time { fill:var(--c-time); }
.legend { color:var(--muted); margin:.2rem 0 .4rem; }
.key { display:inline-block; width:10px; height:10px; border-radius:2px; margin:0 .3rem 0 .6rem; vertical-align:-1px; }
.key.s-fail { background:var(--c-fail); } .key.s-flaky { background:var(--c-flaky); }
td.strip { font-family:ui-monospace,SFMono-Regular,Menlo,monospace; white-space:nowrap; color:var(--muted); letter-spacing:1px; }
.badge.auto { background:transparent; color:var(--y); border:1px solid var(--y); }
#tip { position:fixed; pointer-events:none; background:var(--bg); color:var(--fg);
  border:1px solid var(--line); border-radius:6px; padding:.35rem .55rem; font-size:.82rem;
  box-shadow:0 2px 8px rgba(0,0,0,.15); white-space:nowrap; }
#tip b { display:block; }
"#;

const DASH_JS: &str = r#"
(function(){
  document.querySelectorAll('time[data-epoch]').forEach(function(t){
    var d = new Date(Number(t.getAttribute('data-epoch')) * 1000);
    t.title = t.textContent; t.textContent = d.toLocaleString();
  });
  var tip = document.getElementById('tip');
  function show(g, x, y){
    var lines = (g.getAttribute('data-tip') || '').split('\n');
    var when = new Date(Number(g.getAttribute('data-epoch')) * 1000).toLocaleString();
    tip.textContent = '';
    lines.slice(1).forEach(function(l){ var b = document.createElement('b'); b.textContent = l; tip.appendChild(b); });
    var s = document.createElement('span'); s.textContent = lines[0] + ' · ' + when; tip.appendChild(s);
    tip.hidden = false;
    var w = tip.offsetWidth, h = tip.offsetHeight;
    tip.style.left = Math.min(x + 12, window.innerWidth - w - 8) + 'px';
    tip.style.top = Math.max(8, y - h - 12) + 'px';
  }
  document.querySelectorAll('.chart .col').forEach(function(g){
    g.addEventListener('pointermove', function(e){ show(g, e.clientX, e.clientY); });
    g.addEventListener('pointerleave', function(){ tip.hidden = true; });
    g.addEventListener('focus', function(){ var r = g.getBoundingClientRect(); show(g, r.left + r.width/2, r.top); });
    g.addEventListener('blur', function(){ tip.hidden = true; });
  });
})();
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reporting::history::outcome_code;

    /// History of `runs` runs (epoch = 100 * seq); each test gets the given
    /// strip (`p` passed, `f` failed, `~` flaky) with call `secs`.
    fn history(tests: &[(&str, &str, f32)]) -> History {
        let mut h = History::default();
        let n = tests.iter().map(|t| t.1.len()).max().unwrap_or(0);
        for seq in 0..n as u64 {
            h.runs.push(RunMeta {
                seq,
                id: format!("r{seq}"),
                epoch: 100 * seq,
                // One clean commit: flips only count within a commit.
                commit: Some("caaaaaaaaaaa".into()),
                env: String::new(),
            });
        }
        for (id, strip, secs) in tests {
            let series = strip
                .chars()
                .enumerate()
                .map(|(i, c)| Entry {
                    run: i as u64,
                    outcome: outcome_code(match c {
                        'p' => "passed",
                        'f' => "failed",
                        _ => "flaky",
                    })
                    .unwrap(),
                    call: Some(*secs),
                    setup: 0.0,
                })
                .collect();
            h.tests.insert(id.to_string(), series);
        }
        h
    }

    #[test]
    fn run_stats_count_per_run() {
        let h = history(&[("a", "pf~", 1.0), ("b", "ppf", 0.5)]);
        let s = run_stats(&h);
        assert_eq!(s.len(), 3);
        assert_eq!((s[0].tests, s[0].failed, s[0].flaky), (2, 0, 0));
        assert_eq!((s[1].failed, s[1].flaky), (1, 0));
        assert_eq!((s[2].failed, s[2].flaky), (1, 1));
        assert!((s[2].call_seconds - 1.5).abs() < 1e-6);
    }

    #[test]
    fn tick_labels_have_no_float_noise() {
        assert_eq!(fmt_tick(0.6000000001), "0.6");
        assert_eq!(fmt_tick(2.0), "2");
        assert_eq!(fmt_tick(0.25), "0.25");
        let col = |v: f64, class| Column {
            segments: vec![(v, class)],
            tip: vec!["c".into()],
            epoch: 0,
        };
        // Count axes step by whole numbers, never 0.5.
        let svg = columns_svg("x", &[col(2.0, "s-fail")], "", true);
        assert!(!svg.contains(">0.5<") && svg.contains(">2<"), "{svg}");
        // Summing 0.2 three times gives 0.6000000000000001; ticks are k * step.
        let svg = columns_svg("x", &[col(0.57, "s-time")], "s", false);
        assert!(svg.contains(">0.6s<") && !svg.contains("0000"), "{svg}");
    }

    #[test]
    fn nice_steps() {
        assert_eq!(nice_step(0.0), 1.0);
        assert_eq!(nice_step(3.0), 1.0);
        assert_eq!(nice_step(7.0), 2.0);
        assert_eq!(nice_step(18.0), 5.0);
        assert_eq!(nice_step(140.0), 50.0);
        assert_eq!(nice_step(0.3), 0.1);
    }

    #[test]
    fn slowest_ranks_by_median_over_passing_runs() {
        let h = history(&[
            ("slow", "pppf", 2.0),
            ("fast", "ppp", 0.1),
            ("young", "pp", 9.0),
            ("instant", "ppp", 0.0001),
        ]);
        let rows = slowest(&h);
        let ids: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
        // "young" has 2 runs; "instant" is under 1ms: both left out.
        assert_eq!(ids, ["slow", "fast"]);
        assert_eq!(rows[0].3, 3); // the failed run doesn't count
    }

    #[test]
    fn render_has_charts_tables_and_escapes_nodeids() {
        let h = history(&[
            ("t.py::flap", "pfpfpfpfp", 0.2),
            ("t.py::x[<script>]", "ppppppppp", 1.0),
        ]);
        let doc = render(&h, 3, None, 900);
        assert!(doc.contains("<h2>Failing and flaky tests per run</h2>"));
        assert!(doc.contains("<h2>Test call time per run</h2>"));
        assert_eq!(
            doc.matches("class=\"col\"").count(),
            18,
            "9 runs x 2 charts"
        );
        // A failed segment and a legend naming both series.
        assert!(doc.contains("class=\"s-fail\""));
        assert!(doc.contains("<span class=\"key s-flaky\"></span>flaky"));
        // Flaky table: flap with its strip and the auto-quarantine badge.
        assert!(doc.contains("\u{2713}\u{2717}\u{2713}\u{2717}"));
        assert!(doc.contains("auto-quarantined</span>"));
        // Untrusted nodeid escaped, never live markup.
        assert!(doc.contains("t.py::x[&lt;script&gt;]"));
        assert!(!doc.contains("x[<script>]"));
        // Runs table, newest first, and no green snapshot.
        assert!(doc.contains("<h2>Runs</h2>"));
        assert!(doc.contains("none yet"));
        let runs = &doc[doc.find("<h2>Runs</h2>").unwrap()..];
        let newest = runs.find("data-epoch=\"800\"").unwrap();
        let oldest = runs.find("data-epoch=\"0\"").unwrap();
        assert!(newest < oldest);
        // No drift in flat data: section absent.
        assert!(!doc.contains("Duration drift"));
    }

    #[test]
    fn render_lists_drift() {
        let mut h = history(&[("t.py::creep", "pppppppppppp", 0.0)]);
        for (i, e) in h
            .tests
            .get_mut("t.py::creep")
            .unwrap()
            .iter_mut()
            .enumerate()
        {
            e.call = Some(0.4 + 0.05 * i as f32);
        }
        let doc = render(&h, 3, None, 2000);
        assert!(doc.contains("<h2>Duration drift</h2>"));
        assert!(doc.contains("t.py::creep"));
    }
}
