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

/// Run `templates` against a synthetic `.gz` fixture. Asserts that the
/// expected per-category clusters appear with the expected counts and
/// placeholder substitutions, and that `--load-state` / `--save-state`
/// round-trips correctly (counts add up across runs).
#[test]
fn templates_extracts_clusters_and_round_trips_state() {
    let dir = test_dir("templates_extracts_clusters_and_round_trips_state");
    let fixture = dir.join("debug.log.gz");
    let state = dir.join("state.jsonl");

    // 100 `[net] received: ping … peer=N` + 50 `[validation] UpdateTip: … height=N`
    let mut content = String::new();
    for i in 1..=100 {
        content.push_str(&format!(
            "2026-06-06T12:34:56Z [net] received: ping (8 bytes) peer={i}\n"
        ));
    }
    let hash = "0".repeat(64);
    for i in 1..=50 {
        content.push_str(&format!(
            "2026-06-06T12:35:{i:02} [validation] UpdateTip: new best={hash} height={}\n",
            800_000 + i
        ));
    }
    write_gz(&fixture, &content);

    // First run: produce state + JSON output to assert against.
    let out = bin()
        .args(["templates", "--json", "--save-state"])
        .arg(&state)
        .arg(&fixture)
        .output()
        .expect("spawn templates");
    assert!(
        out.status.success(),
        "templates run 1 failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json1 = String::from_utf8(out.stdout).unwrap();
    let lines1: Vec<&str> = json1.lines().collect();
    assert_eq!(lines1.len(), 2, "expected 2 clusters; got: {json1}");
    let net = lines1
        .iter()
        .find(|l| l.contains(r#""category":"net""#))
        .unwrap();
    let val = lines1
        .iter()
        .find(|l| l.contains(r#""category":"validation""#))
        .unwrap();
    assert!(net.contains(r#""count":100"#), "net count line: {net}");
    assert!(net.contains("<PEER>"), "net should show <PEER>: {net}");
    assert!(net.contains("<BYTES>"), "net should show <BYTES>: {net}");
    assert!(val.contains(r#""count":50"#), "validation count: {val}");
    assert!(
        val.contains("<HASH>"),
        "validation should show <HASH>: {val}"
    );
    assert!(
        val.contains("<HEIGHT>"),
        "validation should show <HEIGHT>: {val}"
    );

    // Second run: --load-state from first, ingest same fixture, --save-state.
    // Counts should double.
    let out = bin()
        .args(["templates", "--json", "--load-state"])
        .arg(&state)
        .arg("--save-state")
        .arg(&state)
        .arg(&fixture)
        .output()
        .expect("spawn templates run 2");
    assert!(
        out.status.success(),
        "templates run 2 failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json2 = String::from_utf8(out.stdout).unwrap();
    assert!(
        json2.contains(r#""count":200"#),
        "expected net count 200 after second pass: {json2}"
    );
    assert!(
        json2.contains(r#""count":100"#),
        "expected validation count 100 after second pass: {json2}"
    );

    // Third run: --load-state only (no input). Counts unchanged.
    let out = bin()
        .args(["templates", "--json", "--load-state"])
        .arg(&state)
        .output()
        .expect("spawn templates run 3");
    assert!(
        out.status.success(),
        "templates run 3 failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json3 = String::from_utf8(out.stdout).unwrap();
    assert!(json3.contains(r#""count":200"#));
    assert!(json3.contains(r#""count":100"#));
}
