//! Live per-worker status footer (nextest-style): a sticky line per worker
//! plus progress/ETA header. Interactive terminals only (see
//! [`Palette::live`]); a no-op elsewhere. RELATIVE cursor moves survive
//! scroll; all output must flow through print_line/print_inline. Every
//! footer line is cut to the terminal width, so none wraps and the
//! cursor-up count stays exact.

use std::io::Write;
use std::time::Instant;

use crate::reporting::color::Palette;

/// The sticky per-worker footer + progress header. All run output must flow
/// through [`StatusFooter::print_line`]/[`StatusFooter::print_inline`] so the
/// footer is erased and repainted around it. A no-op off a tty.
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
    /// Create a footer for `workers` slots. `live` is the palette's
    /// interactive-terminal decision; without it every call is a no-op.
    pub fn new(workers: usize, live: bool) -> Self {
        Self {
            enabled: live,
            running: vec![None; workers],
            done: 0,
            total: None,
            started: Instant::now(),
            painted_lines: 0,
            tail_line: String::new(),
            bar: false,
        }
    }

    /// Set the total test count (drives the header counter / bar).
    pub fn set_total(&mut self, total: usize) {
        self.total = Some(total);
    }

    /// Toggle bar-mode header (filled progress bar vs. `[done/total]`).
    pub fn set_bar(&mut self, on: bool) {
        self.bar = on;
    }

    /// Record that `worker` started `nodeid`; repaints the footer.
    pub fn item_started(&mut self, w: &mut dyn Write, worker: usize, nodeid: String) {
        if let Some(slot) = self.running.get_mut(worker) {
            *slot = Some((nodeid, Instant::now()));
        }
        self.refresh(w);
    }

    /// Record that `worker` finished its test; bumps the done count and
    /// repaints.
    pub fn item_finished(&mut self, w: &mut dyn Write, worker: usize) {
        if let Some(slot) = self.running.get_mut(worker) {
            *slot = None;
        }
        self.done += 1;
        self.refresh(w);
    }

    /// Periodic tick: refresh elapsed times.
    pub fn tick(&mut self, w: &mut dyn Write) {
        self.refresh(w);
    }

    /// Print a full line of run output (failure blocks, verbose lines...).
    pub fn print_line(&mut self, w: &mut dyn Write, text: &str) {
        self.erase(w);
        let _ = writeln!(w, "{text}");
        self.tail_line.clear();
        self.repaint(w);
    }

    /// Print without newline (progress dots). The text accrues onto the
    /// current real-output line so the rest cursor can be restored after the
    /// footer is repainted.
    pub fn print_inline(&mut self, w: &mut dyn Write, text: &str) {
        self.erase(w);
        let _ = write!(w, "{text}");
        if self.enabled {
            self.tail_line.push_str(text);
        }
        self.repaint(w);
    }

    /// Remove the footer for good (before summary/doctor output).
    pub fn finish(&mut self, w: &mut dyn Write) {
        self.erase(w);
        self.enabled = false;
        let _ = w.flush();
    }

    fn refresh(&mut self, w: &mut dyn Write) {
        self.erase(w);
        self.repaint(w);
    }

    /// Clear the footer. Precondition: cursor is at the rest position (end of
    /// real output) with the footer BELOW it, so `CSI 0J` wipes the footer
    /// without touching prior output. Scroll-safe: no absolute cursor used.
    fn erase(&mut self, w: &mut dyn Write) {
        if !self.enabled || self.painted_lines == 0 {
            return;
        }
        let _ = write!(w, "\x1b[0J");
        self.painted_lines = 0;
    }

    fn repaint(&mut self, w: &mut dyn Write) {
        if !self.enabled {
            let _ = w.flush();
            return;
        }
        let width = terminal_width();
        // Footer lines stop one column short of the edge: a line that fills
        // the row leaves some terminals in a pending-wrap state.
        let room = width.saturating_sub(1).max(1);
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
                    // Narrow terminal: shrink the bar, keep the numbers.
                    let numbers = bar_header(self.done, t, 0).chars().count() + eta.len();
                    let bar = room.saturating_sub(numbers).min(BAR_WIDTH);
                    format!("{}{eta}", bar_header(self.done, t, bar))
                } else {
                    format!("[{}/{t}{eta}]", self.done)
                }
            }
            // total unknown: a bar has no denominator, fall back to a counter
            _ => format!("[{} done]", self.done),
        };
        let progress = head(progress.trim_start(), room);
        out.push_str(&format!("\x1b[2m{progress}\x1b[0m\n"));
        for (i, slot) in self.running.iter().enumerate() {
            let line = match slot {
                Some((nodeid, since)) => {
                    let secs = since.elapsed().as_secs_f64();
                    let label = format!("gw{i:<2} {secs:>5.1}s");
                    let label = head(&label, room);
                    let left = room.saturating_sub(label.chars().count() + 1);
                    if left == 0 {
                        format!("\x1b[2m{label}\x1b[0m")
                    } else {
                        format!("\x1b[2m{label}\x1b[0m {}", tail(nodeid, left))
                    }
                }
                None => format!("\x1b[2m{}\x1b[0m", head(&format!("gw{i:<2}   idle"), room)),
            };
            out.push_str(&line);
            out.push('\n');
        }
        // Lines printed below the rest cursor: the leading blank, the
        // progress header, and one per worker.
        let lines = 1 + 1 + self.running.len();
        // Move the cursor back UP to the rest line (relative - survives any
        // scroll the paint triggered), return to column 0, and reprint the
        // part of the pending real-output line on that row so the cursor
        // lands at its true end.
        out.push_str(&format!(
            "\x1b[{lines}A\r{}",
            last_row(&self.tail_line, width)
        ));
        let _ = write!(w, "{out}");
        self.painted_lines = lines;
        let _ = w.flush();
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

/// The last `max` chars of a nodeid (one column per char), never longer.
fn tail(s: &str, max: usize) -> &str {
    let n = s.chars().count();
    if n <= max {
        return s;
    }
    let cut = s.char_indices().nth(n - max).map_or(s.len(), |(i, _)| i);
    &s[cut..]
}

/// The first `max` chars of `s` (plain text, one column per char).
fn head(s: &str, max: usize) -> &str {
    let cut = s.char_indices().nth(max).map_or(s.len(), |(i, _)| i);
    &s[..cut]
}

/// The part of a pending output line (progress chars, possibly wrapped by
/// the terminal) that sits on its last screen row: what to reprint after a
/// `\r` to put the cursor back at the line's true end. ANSI color escapes
/// take no column and stay attached to the char they precede.
fn last_row(line: &str, width: usize) -> &str {
    // Byte offset where each visible char's unit starts (its leading
    // escapes included).
    let mut starts = Vec::new();
    let mut unit_start = 0;
    let mut chars = line.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == '\x1b' {
            // CSI: ESC [ params final-byte
            if chars.peek().is_some_and(|&(_, c)| c == '[') {
                chars.next();
                for (_, c) in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        starts.push(unit_start);
        unit_start = i + c.len_utf8();
    }
    let n = starts.len();
    if width == 0 || n <= width {
        return line;
    }
    // A row-filling line ends on its last full row (pending wrap).
    let first = (n - 1) / width * width;
    &line[starts[first]..]
}

/// Columns of the terminal on stdout: the tty's own size, else `COLUMNS`,
/// else 80. Re-read on every repaint so a resize takes effect.
fn terminal_width() -> usize {
    #[cfg(unix)]
    {
        let mut ws = libc::winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: TIOCGWINSZ only writes into the winsize we own and pass by
        // pointer; on failure it stays zeroed, which falls through below.
        let rc = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };
        if rc == 0 && ws.ws_col > 0 {
            return ws.ws_col as usize;
        }
    }
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.trim().parse().ok())
        .filter(|&c: &usize| c > 0)
        .unwrap_or(80)
}

#[cfg(test)]
mod tests {
    use super::{bar_header, head, last_row, summary_bar, tail, BAR_WIDTH};
    use crate::reporting::color::Palette;

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
    fn tail_counts_chars_not_bytes() {
        // A multibyte char near the cut: never longer than asked, never a
        // broken codepoint, never a panic.
        let s = format!("{}é{}", "a".repeat(8), "b".repeat(3));
        assert_eq!(tail(&s, 4), "ébbb");
        assert_eq!(tail(&s, 3), "bbb");
        let t = format!("{}écho", "a".repeat(8));
        assert_eq!(tail(&t, 5), "aécho");
        assert_eq!(tail("ééé", 0), "");
    }

    #[test]
    fn head_cuts_on_chars() {
        assert_eq!(head("████ 50%", 3), "███");
        assert_eq!(head("short", 90), "short");
    }

    #[test]
    fn last_row_reprints_only_the_wrapped_remainder() {
        // Fits: the whole line.
        assert_eq!(last_row("....", 10), "....");
        // 12 chars at width 5: rows of 5, 5, 2; the cursor's row holds 2.
        assert_eq!(last_row("abcdefghijkl", 5), "kl");
        // Exactly two rows: the second full row (pending wrap).
        assert_eq!(last_row("abcdefghij", 5), "fghij");
        // Color escapes take no column and stay with their char.
        let dots = "\x1b[32m.\x1b[0m".repeat(7);
        assert_eq!(
            last_row(&dots, 5),
            "\x1b[0m\x1b[32m.\x1b[0m\x1b[32m.\x1b[0m"
        );
    }

    #[test]
    fn narrow_footer_lines_fit_the_width() {
        use super::StatusFooter;
        let mut f = StatusFooter::new(2, true);
        f.set_total(29);
        f.set_bar(true);
        let mut buf: Vec<u8> = Vec::new();
        f.item_started(&mut buf, 0, format!("tests/{}::test_x", "y".repeat(80)));
        let text = String::from_utf8(buf).unwrap();
        let plain = regex::Regex::new(r"\x1b\[[0-9;]*[A-Za-z]")
            .unwrap()
            .replace_all(&text, "");
        // The width comes from the tty, else COLUMNS, else 80: any of
        // them is at most this generous bound once the 90-col cut is gone.
        let width = super::terminal_width();
        for line in plain.split(['\n', '\r']) {
            assert!(line.chars().count() < width, "{line:?} wider than {width}");
        }
    }
}
