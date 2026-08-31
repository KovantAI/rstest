//! Live per-worker status footer (nextest-style): a sticky line per worker
//! plus progress/ETA header. TTY-only (no-op elsewhere). RELATIVE cursor
//! moves survive scroll; all output must flow through print_line/print_inline.

use std::io::{IsTerminal, Write};
use std::time::Instant;

use crate::color::Palette;

pub struct StatusFooter {
    enabled: bool,
    /// nodeid + start time per worker slot for in-flight items.
    running: Vec<Option<(String, Instant)>>,
    done: usize,
    total: Option<usize>,
    started: Instant,
    /// Number of footer lines currently painted below the rest cursor (0 when
    /// nothing is on screen). Used to move back up by a RELATIVE amount, which
    /// survives terminal scroll - an absolute saved cursor (DECSC) does not.
    painted_lines: usize,
    /// The current real-output line that has not been terminated with a
    /// newline yet (progress dots). Reprinted after each repaint so the rest
    /// cursor lands at the true end of output, not column 0.
    tail_line: String,
    /// Bar mode: render a filled progress bar as the header instead of the
    /// plain `[done/total]` counter.
    bar: bool,
}

const BAR_WIDTH: usize = 30;

impl StatusFooter {
    pub fn new(workers: usize) -> Self {
        Self {
            enabled: std::io::stdout().is_terminal(),
            running: vec![None; workers],
            done: 0,
            total: None,
            started: Instant::now(),
            painted_lines: 0,
            tail_line: String::new(),
            bar: false,
        }
    }

    pub fn set_total(&mut self, total: usize) {
        self.total = Some(total);
    }

    pub fn set_bar(&mut self, on: bool) {
        self.bar = on;
    }

    pub fn item_started(&mut self, worker: usize, nodeid: String) {
        if let Some(slot) = self.running.get_mut(worker) {
            *slot = Some((nodeid, Instant::now()));
        }
        self.refresh();
    }

    pub fn item_finished(&mut self, worker: usize) {
        if let Some(slot) = self.running.get_mut(worker) {
            *slot = None;
        }
        self.done += 1;
        self.refresh();
    }

    /// Periodic tick: refresh elapsed times.
    pub fn tick(&mut self) {
        self.refresh();
    }

    /// Print a full line of run output (failure blocks, verbose lines...).
    pub fn print_line(&mut self, text: &str) {
        self.erase();
        println!("{text}");
        self.tail_line.clear();
        self.repaint();
    }

    /// Print without newline (progress dots). The text accrues onto the
    /// current real-output line so the rest cursor can be restored after the
    /// footer is repainted.
    pub fn print_inline(&mut self, text: &str) {
        self.erase();
        print!("{text}");
        if self.enabled {
            self.tail_line.push_str(text);
        }
        self.repaint();
    }

    /// Remove the footer for good (before summary/doctor output).
    pub fn finish(&mut self) {
        self.erase();
        self.enabled = false;
        let _ = std::io::stdout().flush();
    }

    fn refresh(&mut self) {
        self.erase();
        self.repaint();
    }

    /// Clear the footer. Precondition: cursor is at the rest position (end of
    /// real output) with the footer BELOW it, so `CSI 0J` wipes the footer
    /// without touching prior output. Scroll-safe: no absolute cursor used.
    fn erase(&mut self) {
        if !self.enabled || self.painted_lines == 0 {
            return;
        }
        print!("\x1b[0J");
        self.painted_lines = 0;
    }

    fn repaint(&mut self) {
        if !self.enabled {
            let _ = std::io::stdout().flush();
            return;
        }
        // Footer body is built into `out`, each line newline-terminated, with
        // a leading blank line separating it from the run output above.
        let mut out = String::from("\n");
        let elapsed = self.started.elapsed().as_secs_f64();
        let progress = match self.total {
            Some(t) if t > 0 => {
                let eta = if self.done > 0 && self.done < t {
                    let rate = elapsed / self.done as f64;
                    format!(" ~{:.0}s left", rate * (t - self.done) as f64)
                } else {
                    String::new()
                };
                if self.bar {
                    format!("{}{eta}", bar_header(self.done, t, BAR_WIDTH))
                } else {
                    format!("[{}/{t}{eta}]", self.done)
                }
            }
            // total unknown: a bar has no denominator, fall back to a counter
            _ => format!("[{} done]", self.done),
        };
        out.push_str(&format!("\x1b[2m{progress}\x1b[0m\n"));
        for (i, slot) in self.running.iter().enumerate() {
            match slot {
                Some((nodeid, since)) => {
                    let secs = since.elapsed().as_secs_f64();
                    let id = tail(nodeid, 90);
                    out.push_str(&format!("\x1b[2mgw{i:<2} {secs:>5.1}s\x1b[0m {id}\n"));
                }
                None => out.push_str(&format!("\x1b[2mgw{i:<2}   idle\x1b[0m\n")),
            }
        }
        // Lines printed below the rest cursor: the leading blank, the
        // progress header, and one per worker.
        let lines = 1 + 1 + self.running.len();
        // Move the cursor back UP to the rest line (relative - survives any
        // scroll the paint triggered), return to column 0, and reprint the
        // pending real-output line so the cursor lands at its true end.
        out.push_str(&format!("\x1b[{lines}A\r{}", self.tail_line));
        print!("{out}");
        self.painted_lines = lines;
        let _ = std::io::stdout().flush();
    }
}

/// A filled bar header: `█████░░░░░  56% (16/29)`. `done` is clamped to
/// `total` so a fabricated over-count can't overflow the bar.
fn bar_header(done: usize, total: usize, width: usize) -> String {
    let done = done.min(total);
    let filled = (width * done).checked_div(total).unwrap_or(0).min(width);
    let pct = (done * 100).checked_div(total).unwrap_or(0);
    format!(
        "{}{} {pct:>3}% ({done}/{total})",
        "█".repeat(filled),
        "░".repeat(width - filled),
    )
}

/// Final summary bar for Bar mode: a fully-filled bar segmented by outcome
/// (green passed / red failed+error / yellow skipped+xfail+xpass). Rounding
/// slack goes to the dominant bucket so no spurious segment appears.
pub fn summary_bar(green: usize, red: usize, yellow: usize, palette: &Palette) -> String {
    let total = green + red + yellow;
    if total == 0 {
        return palette.dim(&"░".repeat(BAR_WIDTH));
    }
    let mut g = BAR_WIDTH * green / total;
    let mut r = BAR_WIDTH * red / total;
    let mut y = BAR_WIDTH * yellow / total;
    let slack = BAR_WIDTH - (g + r + y);
    if green >= red && green >= yellow {
        g += slack;
    } else if red >= yellow {
        r += slack;
    } else {
        y += slack;
    }
    format!(
        "{}{}{}",
        palette.green(&"█".repeat(g)),
        palette.red(&"█".repeat(r)),
        palette.yellow(&"█".repeat(y)),
    )
}

/// Last `max` bytes of a nodeid (char-boundary safe enough for ascii ids;
/// falls back to full string on non-ascii boundaries).
fn tail(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let cut = s.len() - max;
    if s.is_char_boundary(cut) {
        &s[cut..]
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::{bar_header, summary_bar, tail, BAR_WIDTH};
    use crate::color::Palette;

    #[test]
    fn summary_bar_segments_total_width() {
        let p = Palette::default(); // colorless: output is raw block chars
                                    // all green, full width
        assert_eq!(summary_bar(4, 0, 0, &p), "█".repeat(BAR_WIDTH));
        // empty run → dim empty bar (no color when palette off)
        assert_eq!(summary_bar(0, 0, 0, &p), "░".repeat(BAR_WIDTH));
        // mixed: segments sum to exactly BAR_WIDTH, no spurious slack
        let mixed = summary_bar(3, 1, 0, &p);
        assert_eq!(mixed.chars().filter(|c| *c == '█').count(), BAR_WIDTH);
    }

    #[test]
    fn summary_bar_slack_skips_empty_buckets() {
        let p = Palette::default();
        // yellow==0: rounding slack must not paint yellow blocks. With the
        // palette off we can't see color, but the count must still be full
        // and the construction must not panic on uneven division.
        let s = summary_bar(2, 1, 0, &p);
        assert_eq!(s.chars().filter(|c| *c == '█').count(), BAR_WIDTH);
    }

    #[test]
    fn bar_header_fills_proportionally() {
        assert_eq!(bar_header(0, 4, 4), "░░░░   0% (0/4)");
        assert_eq!(bar_header(2, 4, 4), "██░░  50% (2/4)");
        assert_eq!(bar_header(4, 4, 4), "████ 100% (4/4)");
    }

    #[test]
    fn bar_header_clamps_overcount() {
        // a crash-fabricated over-count must not overflow the bar / pct
        assert_eq!(bar_header(9, 4, 4), "████ 100% (4/4)");
    }

    #[test]
    fn tail_truncates_long_ids() {
        assert_eq!(tail("short", 90), "short");
        let long = "x".repeat(100);
        assert_eq!(tail(&long, 90).len(), 90);
    }

    #[test]
    fn tail_refuses_to_split_multibyte() {
        // cut lands mid-é: documented fallback is the FULL string (never
        // a panic, never a broken codepoint)
        let s = format!("{}é{}", "a".repeat(8), "b".repeat(3));
        assert_eq!(tail(&s, 4), s);
        // boundary-clean cut still truncates
        let t = format!("{}écho", "a".repeat(8));
        assert_eq!(tail(&t, 5), "écho");
    }
}
