//! Scheduling and execution: dispatching the suite across worker processes and
//! deciding what runs where. The multi-worker pool (`pool`) and lazy-collection
//! pool (`lazy`), the worker process handle (`worker`) and the
//! orchestrator<->worker wire protocol (`proto`), the duration cache that drives
//! long-pole-first ordering (`durations`), and CI sharding (`shard`).

pub mod durations;
pub mod lazy;
pub mod pool;
pub mod proto;
pub mod shard;
pub mod worker;
