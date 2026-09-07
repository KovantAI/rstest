//! The output sink: the single owner of the process's stdout/stderr streams
//! plus the output policy (palette, quiet). Every line of user-facing output
//! flows through here instead of a bare `println!`/`eprintln!`, which (a) lets
//! tests capture output without spawning a process and (b) puts stream choice,
//! flushing, and the live [`StatusFooter`](super::status::StatusFooter)
//! repaint in one place.
//!
//! Convention: **stdout** carries machine-consumable streams (JSON/TAP/…),
//! banners, and result summaries; **stderr** carries diagnostics. Use
//! [`Sink::out_line`]/[`Sink::out_inline`] for the former, [`Sink::warn`] for
//! the latter.
//!
//! Broken-pipe writes are swallowed rather than panicking: a run piped into
//! `head` (or a consumer that exits early) must not abort the test runner the
//! way a bare `println!` would.

use std::io::{self, Write};
#[cfg(test)]
use std::sync::{Arc, Mutex};

use crate::reporting::color::Palette;

/// Owns the two output streams and the output policy. Threaded as
/// `&mut Sink` through the run pipeline.
pub struct Sink {
    out: Box<dyn Write + Send>,
    err: Box<dyn Write + Send>,
    palette: Palette,
}

impl Sink {
    /// The real sink writing to the process stdout/stderr.
    pub fn stdio(palette: Palette) -> Self {
        Self {
            out: Box::new(io::stdout()),
            err: Box::new(io::stderr()),
            palette,
        }
    }

    /// A sink whose streams are in-memory buffers, for tests. The returned
    /// [`Captured`] shares the buffers, so output written after this call is
    /// visible through it. Palette is colorless by default (plain assertions).
    #[cfg(test)]
    pub fn captured() -> (Self, Captured) {
        let out = SharedBuf::default();
        let err = SharedBuf::default();
        let captured = Captured {
            out: out.0.clone(),
            err: err.0.clone(),
        };
        let sink = Self {
            out: Box::new(out),
            err: Box::new(err),
            palette: Palette::default(),
        };
        (sink, captured)
    }

    /// The active palette (Copy). Callers colorize their own strings, then
    /// hand the finished text to [`Sink::out_line`].
    pub fn palette(&self) -> Palette {
        self.palette
    }

    /// A full stdout line (newline appended).
    pub fn out_line(&mut self, text: &str) {
        let _ = writeln!(self.out, "{text}");
    }

    /// stdout without a trailing newline, flushed immediately (progress dots,
    /// prompts). Bare writes to stdout are otherwise line-buffered on a tty.
    pub fn out_inline(&mut self, text: &str) {
        let _ = write!(self.out, "{text}");
        let _ = self.out.flush();
    }

    /// A stderr diagnostic line. Use for degraded modes, ignored flags, and
    /// informational notices — anything that isn't the machine-readable
    /// stdout stream.
    pub fn warn(&mut self, text: &str) {
        let _ = writeln!(self.err, "{text}");
    }

    /// Raw stdout handle, for callers that stream a large pre-formatted blob
    /// (merged child reports, JSON envelopes). The live footer, if any, must
    /// already be finished — this bypasses footer repaint.
    pub fn out(&mut self) -> &mut dyn Write {
        &mut self.out
    }

    /// Raw stderr handle, counterpart to [`Sink::out`].
    pub fn err(&mut self) -> &mut dyn Write {
        &mut self.err
    }
}

/// Shared handle to a [`Sink::captured`] sink's buffers.
#[cfg(test)]
pub struct Captured {
    out: Arc<Mutex<Vec<u8>>>,
    err: Arc<Mutex<Vec<u8>>>,
}

#[cfg(test)]
impl Captured {
    /// Everything written to stdout so far, lossily decoded.
    pub fn out(&self) -> String {
        String::from_utf8_lossy(&self.out.lock().unwrap()).into_owned()
    }

    /// Everything written to stderr so far, lossily decoded.
    pub fn err(&self) -> String {
        String::from_utf8_lossy(&self.err.lock().unwrap()).into_owned()
    }
}

/// A `Write` backed by a shared byte buffer. Clones share one buffer, so the
/// sink can own one clone while [`Captured`] reads another.
#[cfg(test)]
#[derive(Clone, Default)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

#[cfg(test)]
impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn out_and_err_route_to_separate_streams() {
        let (mut sink, cap) = Sink::captured();
        sink.out_line("result");
        sink.warn("degraded");
        assert_eq!(cap.out(), "result\n");
        assert_eq!(cap.err(), "degraded\n");
    }

    #[test]
    fn out_inline_appends_without_newline() {
        let (mut sink, cap) = Sink::captured();
        sink.out_inline(".");
        sink.out_inline(".");
        assert_eq!(cap.out(), "..");
    }

    #[test]
    fn raw_handles_write_through() {
        let (mut sink, cap) = Sink::captured();
        let mid = 'b';
        write!(sink.out(), "a{mid}c").unwrap();
        write!(sink.err(), "x").unwrap();
        assert_eq!(cap.out(), "abc");
        assert_eq!(cap.err(), "x");
    }
}
