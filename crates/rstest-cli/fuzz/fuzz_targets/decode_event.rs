#![no_main]

//! Fuzz the event decoder - the orchestrator's only untrusted input surface.
//!
//! Every message a worker writes to its event pipe is decoded here into a
//! [`proto::Event`]. A worker can crash, wedge, or emit garbage mid-frame, so
//! the decoder must terminate with `Ok` or `Err` on ANY byte string - never
//! panic, never loop or allocate unbounded. libfuzzer drives the byte space
//! (including malformed msgpack length prefixes, the algorithmic-complexity /
//! DoS vector); any crash it finds is a real orchestrator robustness bug.
//!
//! Note: the per-message byte cap that bounds pathological length prefixes
//! lives in `EventReader` (worker.rs) and is unit-tested there. This target
//! exercises the raw decode the way `EventReader::recv` calls it.

use libfuzzer_sys::fuzz_target;
use rstest_cli::proto::Event;

fuzz_target!(|data: &[u8]| {
    let _ = rmp_serde::from_slice::<Event>(data);
});
