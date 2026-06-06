//! Integration tests that drive the real binary end-to-end.
//!
//! Two self-contained tests build synthetic gzipped fixtures, run
//! `merge` / `split`, and assert exact content. A third test scans
//! `tests/data/` for real fixtures the user has dropped in and round-trips
//! those; it skips cleanly when no fixtures are present so the test suite
//! stays green on a fresh checkout.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use flate2::Compression;
use flate2::read::MultiGzDecoder;
use flate2::write::GzEncoder;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_debug-log-tool"))
}

fn test_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run_ok(cmd: &mut Command, what: &str) {
    let output = cmd.output().expect("failed to spawn debug-log-tool");
    if !output.status.success() {
        panic!(
            "{what} failed: status={:?}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

fn write_gz(path: &Path, content: &str) {
    let f = std::fs::File::create(path).unwrap();
    let mut enc = GzEncoder::new(f, Compression::default());
    enc.write_all(content.as_bytes()).unwrap();
    enc.finish().unwrap();
}

fn read_gz(path: &Path) -> String {
    let f = std::fs::File::open(path).unwrap();
    let mut dec = MultiGzDecoder::new(f);
    let mut s = String::new();
    dec.read_to_string(&mut s).unwrap();
    s
}

fn read_zst(path: &Path) -> String {
    let f = std::fs::File::open(path).unwrap();
    let mut dec = zstd::Decoder::new(f).unwrap();
    let mut s = String::new();
    dec.read_to_string(&mut s).unwrap();
    s
}

#[test]
fn merge_interleaves_by_timestamp() {
    let dir = test_dir("merge_interleaves_by_timestamp");
    let inputs = dir.join("inputs");
    std::fs::create_dir_all(&inputs).unwrap();

    write_gz(
        &inputs.join("debug.log-20260515-node1.gz"),
        "2026-05-15T00:06:30 first-on-node1\n\
         2026-05-15T00:06:32.500000Z third-on-node1\n",
    );
    write_gz(
        &inputs.join("debug.log-20260515-node2.gz"),
        "2026-05-15T00:06:31.149660Z second-on-node2\n\
         2026-05-15T00:06:33 fourth-on-node2\n",
    );

    let out = dir.join("merged.zst");
    run_ok(bin().args(["merge", "-o"]).arg(&out).arg(&inputs), "merge");

    assert_eq!(
        read_zst(&out),
        "node1 2026-05-15T00:06:30 first-on-node1\n\
         node2 2026-05-15T00:06:31.149660Z second-on-node2\n\
         node1 2026-05-15T00:06:32.500000Z third-on-node1\n\
         node2 2026-05-15T00:06:33 fourth-on-node2\n",
    );
}

#[test]
fn merge_then_split_round_trips_synthetic_logs() {
    let dir = test_dir("merge_then_split_round_trips_synthetic_logs");
    let orig = dir.join("orig");
    let split_out = dir.join("split-out");
    std::fs::create_dir_all(&orig).unwrap();

    // node1 includes a continuation line (no leading timestamp) to exercise
    // the "inherit previous timestamp" path on both sides.
    let node1 = "2026-05-15T00:06:30 first-on-node1\n\
                 \tcontinuation-line\n\
                 2026-05-15T00:06:32.500000Z third-on-node1\n";
    let node2 = "2026-05-15T00:06:31.149660Z second-on-node2\n\
                 2026-05-15T00:06:33 fourth-on-node2\n";

    write_gz(&orig.join("debug.log-20260515-node1.gz"), node1);
    write_gz(&orig.join("debug.log-20260515-node2.gz"), node2);

    let merged = dir.join("merged.zst");
    run_ok(bin().args(["merge", "-o"]).arg(&merged).arg(&orig), "merge");
    run_ok(
        bin().args(["split", "-o"]).arg(&split_out).arg(&merged),
        "split",
    );

    assert_eq!(
        read_gz(&split_out.join("debug.log-20260515-node1.gz")),
        node1,
    );
    assert_eq!(
        read_gz(&split_out.join("debug.log-20260515-node2.gz")),
        node2,
    );
}

/// Round-trips every `debug.log-<date>-<node>.gz` fixture in `tests/data/`,
/// asserting byte-exact equivalence after merge -> split. Skipped (prints a
/// message) when the directory is missing or contains no fixtures.
#[test]
fn round_trips_sample_fixtures_if_present() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    if !fixtures.is_dir() {
        eprintln!(
            "skipping: no tests/data/ directory; drop debug.log-<date>-<node>.gz \
             fixtures there to enable this test"
        );
        return;
    }

    let inputs: Vec<PathBuf> = std::fs::read_dir(&fixtures)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("debug.log-") && n.ends_with(".gz"))
        })
        .collect();

    if inputs.is_empty() {
        eprintln!("skipping: tests/data/ has no debug.log-*-*.gz fixtures");
        return;
    }

    let dir = test_dir("round_trips_sample_fixtures_if_present");
    let merged = dir.join("merged.zst");
    let split_out = dir.join("split-out");

    run_ok(
        bin().args(["merge", "-o"]).arg(&merged).arg(&fixtures),
        "merge (fixtures)",
    );
    run_ok(
        bin().args(["split", "-o"]).arg(&split_out).arg(&merged),
        "split (fixtures)",
    );

    for input in &inputs {
        let name = input.file_name().unwrap();
        let round = split_out.join(name);
        assert!(
            round.exists(),
            "round-tripped output missing for fixture {name:?}",
        );
        assert_eq!(
            read_gz(input),
            read_gz(&round),
            "fixture {name:?} did not round-trip byte-exact",
        );
    }
}
