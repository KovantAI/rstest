//! Scheduling and execution: dispatching the suite across worker processes and
//! deciding what runs where. The multi-worker pool (`pool`) and lazy-collection
//! pool (`lazy`), the worker process handle (`worker`) and the
//! orchestrator<->worker wire protocol (`proto`), the duration cache that drives
//! long-pole-first ordering (`durations`), CI sharding (`shard`), and the
//! SIGINT/SIGTERM stop of a parallel run (`interrupt`).

pub mod durations;
pub(crate) mod interrupt;
pub mod lazy;
pub(crate) mod orchestrator;
pub mod pool;
pub mod proto;
pub mod shard;
pub mod worker;
