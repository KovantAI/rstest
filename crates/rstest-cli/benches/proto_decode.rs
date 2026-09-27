//! Wire-protocol decode throughput. The orchestrator decodes one
//! [`Event`] per message off every worker's pipe, so decode latency is on the
//! hot path of every run. This is the speed baseline the fuzz/proptest stages
//! guard the *worst* case of - here we measure the *typical* case so a
//! regression (e.g. an accidental O(n^2) in a decode path) shows up as a
//! throughput drop.
//!
//! Run: `cargo bench --bench proto_decode`
//! Frames are built as msgpack maps (exactly how the Python worker encodes
//! them), then decoded into the real `Event` type.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use rstest_cli::proto::Event;

fn frame(v: serde_json::Value) -> Vec<u8> {
    rmp_serde::encode::to_vec_named(&v).unwrap()
}

fn bench_decode(c: &mut Criterion) {
    let mut group = c.benchmark_group("decode_event");

    // The overwhelmingly common frame: a single per-phase report.
    let report = frame(serde_json::json!({
        "kind": "report",
        "payload": {
            "nodeid": "tests/test_module.py::test_something",
            "when": "call",
            "outcome": "passed",
            "duration": 0.0123,
            "longrepr": null,
        }
    }));
    group.throughput(Throughput::Bytes(report.len() as u64));
    group.bench_function("report", |b| {
        b.iter(|| {
            let e: Event = rmp_serde::from_slice(black_box(&report)).unwrap();
            black_box(e);
        })
    });

    // The big frame: CollectionDone ships the whole id list from one worker.
    // This is where a decode regression would bite a large suite hardest.
    for &n in &[1_000usize, 10_000, 100_000] {
        let ids: Vec<String> = (0..n)
            .map(|i| format!("tests/test_module.py::test_case_{i}"))
            .collect();
        let bytes = frame(serde_json::json!({
            "kind": "collection_done",
            "payload": { "count": n, "hash": "deadbeefcafef00d", "ids": ids }
        }));
        group.throughput(Throughput::Bytes(bytes.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("collection_done", n),
            &bytes,
            |b, bytes| {
                b.iter(|| {
                    let e: Event = rmp_serde::from_slice(black_box(bytes)).unwrap();
                    black_box(e);
                })
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_decode);
criterion_main!(benches);
