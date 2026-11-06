//! Benchmarks for the per-line work `templates` does, plus the timestamp
//! parse `merge` does on every line.
//!
//! The sample lines are real bitcoind output, one per shape that shows up in
//! quantity: the four that dominate a `[net]`-heavy log, a `[bench]` line
//! thick with durations, an addrman line with addresses, and a continuation
//! line with no timestamp.
//!
//!     cargo bench                       # everything
//!     cargo bench -- tokenize           # one group
//!     cargo bench -- --save-baseline before
//!     cargo bench -- --baseline before  # after a change

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use std::hint::black_box;

#[path = "../../crates/debug-log-tool/src/timestamp.rs"]
mod cli_timestamp;

/// One real line per shape, timestamp and all.
const LINES: &[(&str, &str)] = &[
    (
        "received",
        "2025-11-15T00:00:36.192751Z [msghand] [net] received: inv (37 bytes) peer=406247",
    ),
    (
        "got_inv",
        "2025-11-15T00:00:36.192817Z [msghand] [net] got inv: wtx 6582bc540eae31af0544e68e8407a585048ada4d1330c863e378b3b9892b52b1  new peer=406247",
    ),
    (
        "mempool_accept",
        "2025-11-15T00:00:36.190886Z [msghand] [mempool] AcceptToMemoryPool: peer=9: accepted e58383544f3494d359fa6c79f7367216eb3c45717c59ec28f9e46098895b89a2 (wtxid=efc1b11648ecc949f71c4c42bb02253d49934ae1a6ccd3747ec9ce0d0e90a9d6) (poolsz 49874 txn, 154979 kB)",
    ),
    (
        "version_handshake",
        "2025-11-15T00:01:02.117429Z [msghand] [net] receive version message: /Satoshi:27.1.0/: version 70016, blocks=870000, us=[::]:0, txrelay=1, peer=12 peeraddr=192.0.2.44:8333",
    ),
    (
        "bench_connect",
        "2025-11-15T00:04:11.880120Z [msghand] [bench] - Connect 2843 transactions: 48.21ms (0.017ms/tx, 0.008ms/txin) [12.43s (4.21ms/blk)]",
    ),
    (
        "addrman_ipv6",
        "2025-11-15T00:02:55.001122Z [msghand] [addrman] Added [2a01:4f8:c010:abcd::1]:8333 to new[64][12]",
    ),
    ("continuation", "\tat SomeFunction (validation.cpp:1234)"),
];

/// A whole day looks like this: `got inv` and `received` dominate.
const MIX: &[(&str, usize)] = &[
    ("got_inv", 42),
    ("received", 15),
    ("mempool_accept", 2),
    ("version_handshake", 1),
    ("bench_connect", 1),
    ("addrman_ipv6", 1),
    ("continuation", 1),
];

fn line(name: &str) -> &'static str {
    LINES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, l)| *l)
        .expect("unknown sample line")
}

/// The realistic mix, as one block of lines.
fn mixed_lines() -> Vec<&'static str> {
    let mut out = Vec::new();
    for (name, weight) in MIX {
        for _ in 0..*weight {
            out.push(line(name));
        }
    }
    out
}

fn bench_timestamp(c: &mut Criterion) {
    let mut group = c.benchmark_group("timestamp");
    for (name, text) in LINES {
        group.bench_function(*name, |b| {
            let bytes = text.as_bytes();
            b.iter(|| black_box(cli_timestamp::parse_timestamp(black_box(bytes))));
        });
    }
    group.finish();
}

fn bench_line_parse(c: &mut Criterion) {
    let mut group = c.benchmark_group("line_parse");
    for (name, text) in LINES {
        group.bench_function(*name, |b| {
            b.iter(|| black_box(drnlog::line::parse(black_box(text))));
        });
    }
    group.finish();
}

fn bench_tokenize(c: &mut Criterion) {
    let mut group = c.benchmark_group("tokenize");
    for (name, text) in LINES {
        // Tokenizing starts after the timestamp and category are stripped.
        let content = drnlog::line::parse(text).content;
        group.bench_function(*name, |b| {
            b.iter(|| black_box(drnlog::tokenize(black_box(content))));
        });
    }
    group.finish();
}

/// Parse, tokenize and cluster, which is what `templates` does per line.
fn bench_ingest(c: &mut Criterion) {
    let lines = mixed_lines();
    let mut group = c.benchmark_group("ingest");
    group.throughput(criterion::Throughput::Elements(lines.len() as u64));
    group.bench_function("mixed_day", |b| {
        b.iter_batched(
            || drnlog::Analyzer::new(4, 0.5),
            |mut analyzer| {
                for line in &lines {
                    analyzer.ingest_line(line);
                }
                analyzer
            },
            BatchSize::SmallInput,
        );
    });
    // Separately: the steady state, where every line folds into a cluster
    // that already exists. That is the case a long run spends its time in.
    group.bench_function("mixed_day_warm", |b| {
        let mut analyzer = drnlog::Analyzer::new(4, 0.5);
        for line in &lines {
            analyzer.ingest_line(line);
        }
        b.iter(|| {
            for line in &lines {
                analyzer.ingest_line(black_box(line));
            }
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_timestamp,
    bench_line_parse,
    bench_tokenize,
    bench_ingest
);
criterion_main!(benches);
