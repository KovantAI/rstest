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
    /// Optional `--stream-json` sink: a side channel that receives one NDJSON
    /// object per test-phase report as the run progresses, independent of the
    /// human stdout/stderr. `None` unless `--stream-json` was given.
    stream: Option<Box<dyn Write + Send>>,
}

impl Sink {
    /// The real sink writing to the process stdout/stderr.
    pub fn stdio(palette: Palette) -> Self {
        Self {
            out: Box::new(io::stdout()),
            err: Box::new(io::stderr()),
            palette,
            stream: None,
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
            stream: None,
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

    /// Attach a `--stream-json` sink. Every subsequent [`Sink::emit_report`] /
    /// [`Sink::emit_event`] writes one NDJSON line here, flushed immediately so
    /// a fifo/tailing reader (a Test Explorer) sees each event live.
    pub fn attach_stream(&mut self, stream: Box<dyn Write + Send>) {
        self.stream = Some(stream);
    }

    /// Emit one NDJSON `testreport` object to the stream sink (no-op unless
    /// `--stream-json` is attached). Uses the exact same serializer as
    /// `--output json`, so the side channel and the stdout stream carry a
    /// byte-identical schema — one live mirror of a [`proto::Report`] per phase.
    /// Write/flush errors are swallowed like the rest of `Sink`: a reader that
    /// hung up must not abort the run.
    pub fn emit_report(&mut self, worker: Option<usize>, r: &crate::scheduling::proto::Report) {
        if self.stream.is_some() {
            let line = crate::reporting::progress::testreport_json(worker, r);
            self.emit_event(line);
        }
    }

    /// Emit one NDJSON `collecterror` object to the stream sink (no-op unless
    /// `--stream-json` is attached). Same serializer and schema as `--output
    /// json`, so an import/collection failure surfaces live on the side channel.
    pub fn emit_collect_error(&mut self, path: &str, longrepr: &str) {
        if self.stream.is_some() {
            let line = crate::reporting::progress::collecterror_json(path, longrepr);
            self.emit_event(line);
        }
    }

    /// Emit an arbitrary NDJSON event to the stream sink (e.g. the closing
    /// `sessionfinish` envelope). No-op unless a `--stream-json` sink is
    /// attached. Flushed per line so a fifo/tailing reader sees it immediately.
    pub fn emit_event(&mut self, value: serde_json::Value) {
        if let Some(s) = self.stream.as_mut() {
            let _ = writeln!(s, "{value}");
            let _ = s.flush();
        }
    }

    /// Test seam: route the `--stream-json` side channel into an in-memory
    /// buffer and return a handle to read it, so cross-module tests (fold /
    /// finalize wiring) can assert what the stream emitted.
    #[cfg(test)]
    pub fn attach_captured_stream(&mut self) -> Arc<Mutex<Vec<u8>>> {
        let buf = SharedBuf::default();
        let handle = buf.0.clone();
        self.stream = Some(Box::new(buf));
        handle
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

    fn report(nodeid: &str, when: &str, outcome: &str) -> crate::scheduling::proto::Report {
        crate::scheduling::proto::Report {
            nodeid: nodeid.into(),
            when: when.into(),
            outcome: outcome.into(),
            duration: 0.5,
            longrepr: None,
            wasxfail: false,
            skip_reason: None,
            cpu: None,
            thread_delta: None,
            fd_delta: None,
            sections: vec![],
            lineno: Some(12),
        }
    }

    #[test]
    fn emit_report_writes_a_testreport_line_matching_output_json() {
        let (mut sink, _cap) = Sink::captured();
        let buf = SharedBuf::default();
        let reader = buf.clone();
        sink.attach_stream(Box::new(buf));
        sink.emit_report(Some(0), &report("t.py::a", "call", "passed"));
        sink.emit_event(serde_json::json!({"event": "sessionfinish", "exitstatus": 1}));

        let text = String::from_utf8_lossy(&reader.0.lock().unwrap()).into_owned();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        // Same schema as `--output json`: event "testreport", omitted-when-absent
        // worker/lineno present here.
        assert_eq!(first["event"], "testreport");
        assert_eq!(first["nodeid"], "t.py::a");
        assert_eq!(first["outcome"], "passed");
        assert_eq!(first["worker"], "gw0");
        assert_eq!(first["lineno"], 12);
        let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second["event"], "sessionfinish");
    }

    #[test]
    fn emit_collect_error_writes_a_collecterror_line() {
        let (mut sink, _cap) = Sink::captured();
        let buf = SharedBuf::default();
        let reader = buf.clone();
        sink.attach_stream(Box::new(buf));
        sink.emit_collect_error("tests/test_x.py", "ImportError: boom");

        let text = String::from_utf8_lossy(&reader.0.lock().unwrap()).into_owned();
        let obj: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(obj["event"], "collecterror");
        assert_eq!(obj["path"], "tests/test_x.py");
        assert_eq!(obj["longrepr"], "ImportError: boom");
    }

    #[test]
    fn emit_report_is_a_noop_without_an_attached_stream() {
        // No --stream-json sink: emit must not touch stdout/stderr.
        let (mut sink, cap) = Sink::captured();
        sink.emit_report(None, &report("t.py::a", "call", "passed"));
        sink.emit_event(serde_json::json!({"event": "done", "exitstatus": 0}));
        assert_eq!(cap.out(), "");
        assert_eq!(cap.err(), "");
    }
}
