//! Orchestrator-side HTML report. pytest-html registers its writer only on a
//! node without `workerinput` (its xdist-master check); every rstest worker has
//! one, so at `-n ≥ 2` no writer is registered and `--html` produces nothing.
//! rstest instead renders a single self-contained file from the MERGED run.
//!
//! One file, no external assets: inline CSS + a small vanilla-JS layer for
//! sort/filter/search/expand. The static markup (summary, failures, the
//! "interesting" rows) is meaningful with JavaScript disabled; JS only enhances.
//! The full per-test dataset rides as an embedded schema-5 JSON blob, so passed
//! rows (omitted from the initial DOM to stay light at pandas scale) are
//! rendered on demand.
//!
//! SECURITY: nodeids, tracebacks, captured output, and skip reasons are
//! arbitrary test output. Every value interpolated into markup goes through
//! [`esc`]; the embedded JSON has its `<` escaped so it can't close the script.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::Result;

use crate::reporting::report::{Run, RunMeta};

/// How many slowest passing tests to surface as static rows (the rest ride the
/// embedded JSON and render on demand).
const SLOWEST_PASSED: usize = 10;

// Trailing `\n`s in the format strings are deliberate — they keep the emitted
// HTML source line-broken and readable, not a missing-writeln mistake.
#[allow(clippy::write_with_newline)]
pub fn write(path: &Path, run: &Run, meta: &RunMeta) -> Result<()> {
    let counts = run.counts();
    let total: u64 = counts.values().sum::<u64>() - counts["flaky"] - counts["collect_errors"];

    let mut html = String::new();
    html.push_str("<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n");
    html.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    html.push_str("<title>rstest report</title>\n<style>\n");
    html.push_str(CSS);
    html.push_str("\n</style>\n</head>\n<body>\n");

    // ---- Header / summary ------------------------------------------------
    let pass = counts["passed"];
    let fail = counts["failed"] + counts["errors"] + counts["collect_errors"];
    let warn = counts["skipped"] + counts["xfailed"] + counts["xpassed"] + counts["quarantined"];
    let _ = write!(
        html,
        "<header>\n<h1>rstest report</h1>\n<p class=\"summary {}\">{}</p>\n",
        if run.all_passed() { "ok" } else { "bad" },
        esc(&run.summary_line())
    );
    // Proportion bar (green/red/yellow) sized by outcome.
    let denom = (pass + fail + warn).max(1);
    let _ = write!(
        html,
        "<div class=\"bar\" title=\"{pass} passed / {fail} failed / {warn} other\">\
         <span class=\"g\" style=\"width:{:.3}%\"></span>\
         <span class=\"r\" style=\"width:{:.3}%\"></span>\
         <span class=\"y\" style=\"width:{:.3}%\"></span></div>\n",
        pass as f64 * 100.0 / denom as f64,
        fail as f64 * 100.0 / denom as f64,
        warn as f64 * 100.0 / denom as f64,
    );
    let _ = write!(
        html,
        "<p class=\"meta\">{total} tests · {:.2}s · {} worker{} · exit {}</p>\n",
        meta.duration_seconds,
        meta.workers,
        if meta.workers == 1 { "" } else { "s" },
        meta.exitstatus,
    );
    let _ = write!(
        html,
        "<p class=\"argv\"><code>{}</code></p>\n</header>\n",
        esc(&meta.argv.join(" "))
    );

    // ---- Collection errors ----------------------------------------------
    if !run.collect_errors().is_empty() {
        html.push_str("<section class=\"errors\">\n<h2>Collection errors</h2>\n");
        for (pathname, longrepr) in run.collect_errors() {
            let _ = write!(
                html,
                "<details><summary>{}</summary><pre>{}</pre></details>\n",
                esc(pathname),
                esc(longrepr)
            );
        }
        html.push_str("</section>\n");
    }

    // ---- Failures (expandable, static — work without JS) -----------------
    let mut failures: Vec<(&String, &_)> = run
        .tests()
        .iter()
        .filter(|(_, e)| matches!(e.outcome(), "failed" | "errors"))
        .collect();
    failures.sort_by(|a, b| a.0.cmp(b.0));
    if !failures.is_empty() {
        html.push_str("<section class=\"failures\">\n<h2>Failures</h2>\n");
        for (nodeid, entry) in &failures {
            let text = run
                .failure_text(nodeid)
                .unwrap_or("(no traceback captured)");
            let worker = entry
                .worker
                .as_deref()
                .map(|w| format!(" · {}", esc(w)))
                .unwrap_or_default();
            let _ = write!(
                html,
                "<details class=\"fail\"><summary><span class=\"badge {cls}\">{cls}</span> \
                 <span class=\"nid\">{nid}</span>{worker}</summary><pre>{tb}</pre></details>\n",
                cls = entry.outcome(),
                nid = esc(nodeid),
                worker = worker,
                tb = esc(text),
            );
        }
        html.push_str("</section>\n");
    }

    // ---- Flaky -----------------------------------------------------------
    if !run.flaky.is_empty() {
        html.push_str("<section class=\"flaky\">\n<h2>Flaky (passed after rerun)</h2>\n<ul>\n");
        for (nodeid, attempts) in &run.flaky {
            let _ = write!(
                html,
                "<li><span class=\"nid\">{}</span> <span class=\"muted\">({} rerun{})</span></li>\n",
                esc(nodeid),
                attempts,
                if *attempts > 1 { "s" } else { "" }
            );
        }
        html.push_str("</ul>\n</section>\n");
    }

    // ---- Test table: controls + interesting rows -------------------------
    html.push_str("<section class=\"tests\">\n<h2>Tests</h2>\n");
    html.push_str(
        "<div class=\"controls\">\
         <input id=\"q\" type=\"search\" placeholder=\"search node ids…\" aria-label=\"search\">\
         <span class=\"chips\">\
         <button data-f=\"all\" class=\"on\">all</button>\
         <button data-f=\"failed\">failed</button>\
         <button data-f=\"skipped\">skipped</button>\
         <button data-f=\"passed\">passed</button></span>\
         <label class=\"showpass\"><input type=\"checkbox\" id=\"showpass\"> show passed</label>\
         </div>\n",
    );
    html.push_str(
        "<table id=\"grid\"><thead><tr>\
         <th data-sort=\"nodeid\">node id</th>\
         <th data-sort=\"outcome\">outcome</th>\
         <th data-sort=\"duration\" class=\"num\">seconds</th>\
         <th>worker</th></tr></thead>\n<tbody>\n",
    );
    // Interesting rows now: everything not a plain pass, plus the slowest passes.
    let mut passed: Vec<(&String, f64)> = Vec::new();
    for (nodeid, entry) in run.tests() {
        let outcome = entry.outcome();
        let dur = entry.duration.unwrap_or(0.0);
        if outcome == "passed" && !entry.flaky {
            passed.push((nodeid, dur));
            continue;
        }
        html.push_str(&row(nodeid, entry));
    }
    passed.sort_by(|a, b| b.1.total_cmp(&a.1));
    for (nodeid, _) in passed.iter().take(SLOWEST_PASSED) {
        if let Some(entry) = run.tests().get(*nodeid) {
            html.push_str(&row(nodeid, entry));
        }
    }
    html.push_str("</tbody></table>\n</section>\n");

    // ---- Embedded data + script -----------------------------------------
    let json = serde_json::to_string(&run.snapshot_value(meta)).unwrap_or_else(|_| "{}".into());
    // Escape `<` so the payload can never close this <script> (or open a comment).
    let json = json.replace('<', "\\u003c");
    let _ = write!(
        html,
        "<script type=\"application/json\" id=\"data\">{json}</script>\n<script>\n{JS}\n</script>\n"
    );
    html.push_str("</body>\n</html>\n");

    std::fs::write(path, html)?;
    Ok(())
}

/// One `<tr>` for the grid, carrying data-* attributes the JS filters/sorts on.
fn row(nodeid: &str, entry: &crate::reporting::report::TestEntry) -> String {
    let outcome = entry.outcome();
    let dur = entry.duration.unwrap_or(0.0);
    let mut badges = format!("<span class=\"badge {outcome}\">{outcome}</span>");
    if entry.flaky {
        badges.push_str(" <span class=\"badge flaky\">flaky</span>");
    }
    format!(
        "<tr data-outcome=\"{outcome}\" data-duration=\"{dur}\" data-nodeid=\"{nidl}\">\
         <td class=\"nid\">{nid}</td><td>{badges}</td>\
         <td class=\"num\">{dur:.4}</td><td>{worker}</td></tr>\n",
        nidl = esc(&nodeid.to_lowercase()),
        nid = esc(nodeid),
        worker = entry.worker.as_deref().map(esc).unwrap_or_default(),
    )
}

/// HTML-escape text that may contain arbitrary test output.
fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            _ => o.push(c),
        }
    }
    o
}

const CSS: &str = r#"
:root { --bg:#fff; --fg:#1a1a1a; --muted:#666; --line:#e2e2e2; --card:#fafafa;
  --g:#1a7f37; --r:#cf222e; --y:#9a6700; --accent:#0969da; }
@media (prefers-color-scheme: dark) {
  :root { --bg:#0d1117; --fg:#e6edf3; --muted:#8b949e; --line:#30363d; --card:#161b22;
    --g:#3fb950; --r:#f85149; --y:#d29922; --accent:#58a6ff; } }
* { box-sizing: border-box; }
body { margin:0; padding:1.5rem; max-width:1100px; margin:0 auto;
  font:14px/1.5 -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,sans-serif;
  background:var(--bg); color:var(--fg); }
h1 { font-size:1.4rem; margin:0 0 .3rem; } h2 { font-size:1.1rem; margin:1.5rem 0 .6rem; }
.summary { font-size:1.1rem; font-weight:600; margin:.2rem 0; }
.summary.ok { color:var(--g); } .summary.bad { color:var(--r); }
.meta,.argv { color:var(--muted); margin:.2rem 0; } .argv code { word-break:break-all; }
.bar { display:flex; height:10px; border-radius:5px; overflow:hidden; background:var(--line); margin:.5rem 0; }
.bar .g{background:var(--g)} .bar .r{background:var(--r)} .bar .y{background:var(--y)}
section { border-top:1px solid var(--line); padding-top:.5rem; }
details { background:var(--card); border:1px solid var(--line); border-radius:6px;
  margin:.4rem 0; padding:.3rem .6rem; }
summary { cursor:pointer; }
pre { overflow-x:auto; background:var(--bg); border:1px solid var(--line); border-radius:6px;
  padding:.6rem; white-space:pre-wrap; word-break:break-word; }
.nid { font-family:ui-monospace,SFMono-Regular,Menlo,monospace; }
.muted { color:var(--muted); }
.badge { display:inline-block; padding:.05rem .4rem; border-radius:10px; font-size:.78rem;
  font-weight:600; color:#fff; }
.badge.passed{background:var(--g)} .badge.failed,.badge.errors{background:var(--r)}
.badge.skipped,.badge.xfailed,.badge.xpassed,.badge.quarantined{background:var(--y)}
.badge.flaky{background:var(--accent)} .badge.cached{background:var(--muted)}
.controls { display:flex; gap:.6rem; align-items:center; flex-wrap:wrap; margin:.5rem 0; }
#q { flex:1; min-width:180px; padding:.35rem .5rem; border:1px solid var(--line);
  border-radius:6px; background:var(--bg); color:var(--fg); }
.chips button { border:1px solid var(--line); background:var(--bg); color:var(--fg);
  border-radius:14px; padding:.2rem .7rem; cursor:pointer; margin-right:.2rem; }
.chips button.on { background:var(--accent); color:#fff; border-color:var(--accent); }
table { width:100%; border-collapse:collapse; }
th,td { text-align:left; padding:.35rem .5rem; border-bottom:1px solid var(--line); }
th[data-sort] { cursor:pointer; user-select:none; }
td.num,th.num { text-align:right; font-variant-numeric:tabular-nums; }
"#;

const JS: &str = r#"
(function(){
  var data = JSON.parse(document.getElementById('data').textContent);
  var tests = data.tests || {};
  var grid = document.getElementById('grid');
  var tbody = grid.querySelector('tbody');
  var q = document.getElementById('q');
  var showpass = document.getElementById('showpass');
  var filter = 'all';
  var passedInjected = false;

  function outcomeOf(e){
    if (e.quarantined) return 'quarantined';
    if (e.setup==='failed' || e.teardown==='failed') return 'errors';
    if (e.setup==='skipped' || e.call==='skipped') return e.wasxfail?'xfailed':'skipped';
    if (e.call==='passed') return e.wasxfail?'xpassed':'passed';
    if (e.call==='failed') return 'failed';
    return 'errors';
  }
  function esc(s){ return String(s).replace(/[&<>"']/g, function(c){
    return {'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]; }); }

  // Add every passed test the static markup omitted (once), so search/filter can reach them.
  function injectPassed(){
    if (passedInjected) return; passedInjected = true;
    var seen = {}; Array.prototype.forEach.call(tbody.querySelectorAll('tr'), function(tr){
      seen[tr.getAttribute('data-nodeid')] = 1; });
    var frag = document.createDocumentFragment();
    Object.keys(tests).forEach(function(nid){
      var e = tests[nid], o = outcomeOf(e);
      if (o!=='passed' || e.flaky) return;               // interesting rows already present
      if (seen[nid.toLowerCase()]) return;
      var dur = e.duration||0;
      var tr = document.createElement('tr');
      tr.setAttribute('data-outcome', o);
      tr.setAttribute('data-duration', dur);
      tr.setAttribute('data-nodeid', nid.toLowerCase());
      tr.innerHTML = '<td class="nid">'+esc(nid)+'</td>'+
        '<td><span class="badge passed">passed</span>'+(e.cached?' <span class="badge cached">cached</span>':'')+'</td>'+
        '<td class="num">'+dur.toFixed(4)+'</td><td>'+esc(e.worker||'')+'</td>';
      frag.appendChild(tr);
    });
    tbody.appendChild(frag);
  }

  function apply(){
    var term = q.value.trim().toLowerCase();
    if ((filter==='passed' || showpass.checked || term) && !passedInjected) injectPassed();
    Array.prototype.forEach.call(tbody.querySelectorAll('tr'), function(tr){
      var o = tr.getAttribute('data-outcome');
      var passish = (o==='passed');
      var okFilter = filter==='all' ? (!passish || showpass.checked || term)
        : filter==='failed' ? (o==='failed'||o==='errors')
        : filter==='skipped' ? (o==='skipped'||o==='xfailed'||o==='xpassed'||o==='quarantined')
        : (o==='passed');
      var okTerm = !term || tr.getAttribute('data-nodeid').indexOf(term)>=0;
      tr.style.display = (okFilter && okTerm) ? '' : 'none';
    });
  }

  q.addEventListener('input', apply);
  showpass.addEventListener('change', apply);
  Array.prototype.forEach.call(document.querySelectorAll('.chips button'), function(b){
    b.addEventListener('click', function(){
      document.querySelectorAll('.chips button').forEach(function(x){x.classList.remove('on');});
      b.classList.add('on'); filter = b.getAttribute('data-f'); apply();
    });
  });

  var sortDir = {};
  grid.querySelectorAll('th[data-sort]').forEach(function(th){
    th.addEventListener('click', function(){
      var key = th.getAttribute('data-sort');
      if (key!=='outcome') injectPassed();
      var dir = sortDir[key] = -(sortDir[key]||1);
      var rows = Array.prototype.slice.call(tbody.querySelectorAll('tr'));
      rows.sort(function(a,b){
        var av,bv;
        if (key==='duration'){ av=parseFloat(a.getAttribute('data-duration')); bv=parseFloat(b.getAttribute('data-duration')); }
        else { av=a.getAttribute('data-'+(key==='nodeid'?'nodeid':'outcome')); bv=b.getAttribute('data-'+(key==='nodeid'?'nodeid':'outcome')); }
        return av<bv?dir:av>bv?-dir:0;
      });
      rows.forEach(function(r){ tbody.appendChild(r); });
    });
  });
})();
"#;
#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduling::proto;

    fn rep(nodeid: &str, when: &str, outcome: &str, longrepr: Option<&str>) -> proto::Report {
        proto::Report {
            nodeid: nodeid.into(),
            when: when.into(),
            outcome: outcome.into(),
            duration: 0.5,
            longrepr: longrepr.map(|s| s.into()),
            wasxfail: false,
            skip_reason: None,
            cpu: None,
            sections: Vec::new(),
            lineno: None,
            thread_delta: None,
            fd_delta: None,
        }
    }

    fn meta() -> RunMeta {
        RunMeta {
            exitstatus: 1,
            duration_seconds: 1.25,
            started_at_epoch: 0,
            workers: 2,
            argv: vec!["rstest".into()],
        }
    }

    #[test]
    fn escapes_all_html_metacharacters() {
        assert_eq!(
            esc("<a> & \"x\" 'y'"),
            "&lt;a&gt; &amp; &quot;x&quot; &#39;y&#39;"
        );
    }

    #[test]
    fn renders_and_escapes_untrusted_output() {
        let mut run = Run::default();
        run.record(None, rep("t.py::ok", "call", "passed", None));
        // A traceback carrying markup must never render as live HTML.
        let evil = "boom <script>alert(1)</script> & <img src=x>";
        run.record(None, rep("t.py::bad", "call", "failed", Some(evil)));

        let out = std::env::temp_dir().join(format!("rstest-html-{}.html", std::process::id()));
        write(&out, &run, &meta()).unwrap();
        let doc = std::fs::read_to_string(&out).unwrap();
        let _ = std::fs::remove_file(&out);

        assert!(doc.contains("rstest report"));
        // The failing nodeid appears; its traceback is escaped, not live.
        assert!(doc.contains("t.py::bad"));
        assert!(doc.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(
            !doc.contains("<script>alert(1)"),
            "unescaped payload leaked into the document"
        );
        // Summary counts both tests.
        assert!(
            doc.contains("1 passed") && doc.contains("1 failed"),
            "{}",
            &doc[..400]
        );
        // Embedded data blob present, with its `<` neutralized.
        assert!(doc.contains("id=\"data\""));
        assert!(!doc.contains("</script>alert"));
    }

    #[test]
    fn passing_only_run_is_marked_ok() {
        let mut run = Run::default();
        run.record(None, rep("t.py::a", "call", "passed", None));
        let out = std::env::temp_dir().join(format!("rstest-html-ok-{}.html", std::process::id()));
        write(&out, &run, &meta()).unwrap();
        let doc = std::fs::read_to_string(&out).unwrap();
        let _ = std::fs::remove_file(&out);
        assert!(doc.contains("summary ok"));
        assert!(!doc.contains("<section class=\"failures\">"));
    }

    #[test]
    fn renders_and_escapes_collection_errors() {
        let mut run = Run::default();
        run.record(None, rep("t.py::ok", "call", "passed", None));
        run.collect_error("bad<dir>/t.py".into(), "ImportError: <boom> & bang".into());

        let out =
            std::env::temp_dir().join(format!("rstest-html-collect-{}.html", std::process::id()));
        write(&out, &run, &meta()).unwrap();
        let doc = std::fs::read_to_string(&out).unwrap();
        let _ = std::fs::remove_file(&out);

        assert!(doc.contains("<section class=\"errors\">"));
        assert!(doc.contains("Collection errors"));
        // Path and longrepr both escaped, not live.
        assert!(doc.contains("bad&lt;dir&gt;/t.py"));
        assert!(doc.contains("ImportError: &lt;boom&gt; &amp; bang"));
    }

    #[test]
    fn renders_flaky_section_and_badge() {
        let mut run = Run::default();
        run.record(None, rep("t.py::a", "call", "passed", None));
        run.record(None, rep("t.py::flap", "call", "passed", None));
        run.mark_flaky("t.py::flap".into(), 2);

        let out =
            std::env::temp_dir().join(format!("rstest-html-flaky-{}.html", std::process::id()));
        write(&out, &run, &meta()).unwrap();
        let doc = std::fs::read_to_string(&out).unwrap();
        let _ = std::fs::remove_file(&out);

        // Flaky section (html.rs 125-135), with pluralized rerun count.
        assert!(doc.contains("<section class=\"flaky\">"));
        assert!(doc.contains("Flaky (passed after rerun)"));
        assert!(doc.contains("(2 reruns)"));
        // Flaky badge in the grid row (html.rs 197).
        assert!(doc.contains("<span class=\"badge flaky\">flaky</span>"));
    }
}
