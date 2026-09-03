//! Reporting: turning the stream of worker reports into human- and
//! machine-readable output. The run/result model and JSON (`report`), merged
//! junitxml (`junit`), live progress line (`progress`), per-worker status
//! footer (`status`), the ANSI palette (`color`), cross-run flake history
//! (`flakes`), CI-platform annotations (`ci`), and the self-contained HTML
//! report (`html`).

pub mod ci;
pub mod color;
pub mod flakes;
pub mod html;
pub mod junit;
pub mod progress;
pub mod report;
pub mod status;
