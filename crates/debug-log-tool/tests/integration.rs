//! Integration tests that drive the real binary end-to-end.
//!
//! Two self-contained tests build synthetic gzipped fixtures, run
//! `merge` / `split`, and assert exact content. A third test scans
//! `tests/data/` for real fixtures the user has dropped in and round-trips
//! those; it skips cleanly when no fixtures are present so the test suite
//! stays green on a fresh checkout.

use std::io::{BufRead, Read, Write};
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

    let out_dir = dir.join("merged");
    run_ok(
        bin().args(["merge", "-o"]).arg(&out_dir).arg(&inputs),
        "merge",
    );

    assert_eq!(
        read_zst(&out_dir.join("debug.log-20260515.zst")),
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

    // node1 includes a continuation line (no leading timestamp) to check that
    // merge still prefixes it with `<node> ` and split reattaches it correctly.
    let node1 = "2026-05-15T00:06:30 first-on-node1\n\
                 \tcontinuation-line\n\
                 2026-05-15T00:06:32.500000Z third-on-node1\n";
    let node2 = "2026-05-15T00:06:31.149660Z second-on-node2\n\
                 2026-05-15T00:06:33 fourth-on-node2\n";

    write_gz(&orig.join("debug.log-20260515-node1.gz"), node1);
    write_gz(&orig.join("debug.log-20260515-node2.gz"), node2);

    let merged_dir = dir.join("merged");
    run_ok(
        bin().args(["merge", "-o"]).arg(&merged_dir).arg(&orig),
        "merge",
    );
    run_ok(
        bin()
            .args(["split", "-o"])
            .arg(&split_out)
            .arg(merged_dir.join("debug.log-20260515.zst")),
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

/// `merge --check` verifies the round-trip via content hashes, writes nothing,
/// and reports one line per input file plus a summary.
#[test]
fn merge_check_verifies_round_trip_without_writing() {
    let dir = test_dir("merge_check_verifies_round_trip_without_writing");
    let inputs = dir.join("inputs");
    std::fs::create_dir_all(&inputs).unwrap();

    // Two days so the check exercises grouping; node1 has a continuation line.
    write_gz(
        &inputs.join("debug.log-20260515-node1.gz"),
        "2026-05-15T00:06:30 first-on-node1\n\
         \tcontinuation-line\n\
         2026-05-15T00:06:32.500000Z third-on-node1\n",
    );
    write_gz(
        &inputs.join("debug.log-20260515-node2.gz"),
        "2026-05-15T00:06:31.149660Z second-on-node2\n",
    );
    write_gz(
        &inputs.join("debug.log-20260516-node1.gz"),
        "2026-05-16T00:00:00 next-day-node1\n",
    );

    let out = bin()
        .args(["merge", "--check"])
        .arg(&inputs)
        .output()
        .expect("spawn merge --check");
    assert!(
        out.status.success(),
        "check should pass: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // 3 input files, all round-tripping, and no lines dropped/duplicated.
    assert!(
        stdout.contains("3/3 files round-trip identical"),
        "summary missing: {stdout}"
    );
    assert!(!stdout.contains("FAIL"), "unexpected failure: {stdout}");
    // Per-file line counts, keyed by day since node1 appears in both.
    assert!(
        stdout.contains("20260515  node1          OK    3 lines"),
        "{stdout}"
    );
    assert!(
        stdout.contains("20260515  node2          OK    1 lines"),
        "{stdout}"
    );
    assert!(
        stdout.contains("20260516  node1          OK    1 lines"),
        "{stdout}"
    );
    // Nothing was written: no output dir was passed and none was created.
    assert!(!dir.join("merged").exists());
}

/// `merge --check` combined with `--output-dir` writes the merged output and
/// verifies the round-trip in the same pass. The written file must both exist
/// and split back to the originals.
#[test]
fn merge_check_with_output_writes_and_verifies() {
    let dir = test_dir("merge_check_with_output_writes_and_verifies");
    let orig = dir.join("orig");
    std::fs::create_dir_all(&orig).unwrap();

    let node1 = "2026-05-15T00:06:30 first-on-node1\n\
                 2026-05-15T00:06:32.500000Z third-on-node1\n";
    let node2 = "2026-05-15T00:06:31.149660Z second-on-node2\n";
    write_gz(&orig.join("debug.log-20260515-node1.gz"), node1);
    write_gz(&orig.join("debug.log-20260515-node2.gz"), node2);

    let merged_dir = dir.join("merged");
    let out = bin()
        .args(["merge", "--check", "-o"])
        .arg(&merged_dir)
        .arg(&orig)
        .output()
        .expect("spawn merge --check -o");
    assert!(
        out.status.success(),
        "combined write+check should pass: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("writing output too"),
        "scope note: {stdout}"
    );
    assert!(
        stdout.contains("2/2 files round-trip identical"),
        "summary: {stdout}"
    );

    // The merged file was actually written and splits back to the originals.
    let merged = merged_dir.join("debug.log-20260515.zst");
    assert!(merged.exists(), "merged output should have been written");
    let split_out = dir.join("split-out");
    run_ok(
        bin().args(["split", "-o"]).arg(&split_out).arg(&merged),
        "split",
    );
    assert_eq!(
        read_gz(&split_out.join("debug.log-20260515-node1.gz")),
        node1
    );
    assert_eq!(
        read_gz(&split_out.join("debug.log-20260515-node2.gz")),
        node2
    );
}

/// Without `--check`, `merge` still requires an output directory and fails
/// cleanly (non-zero exit) when one isn't given.
#[test]
fn merge_without_output_dir_or_check_errors() {
    let dir = test_dir("merge_without_output_dir_or_check_errors");
    let inputs = dir.join("inputs");
    std::fs::create_dir_all(&inputs).unwrap();
    write_gz(
        &inputs.join("debug.log-20260515-node1.gz"),
        "2026-05-15T00:06:30 only-line\n",
    );

    let out = bin()
        .arg("merge")
        .arg(&inputs)
        .output()
        .expect("spawn merge");
    assert!(
        !out.status.success(),
        "merge with no -o/--check should fail"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--output-dir is required"),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A line whose timestamp crosses midnight into the next day must still
/// round-trip byte-for-byte into its *filename's* day file. `split` keys the
/// output date off the input filename, not the line timestamps, so the
/// cross-midnight line stays where `merge` grouped it rather than leaking into
/// a `debug.log-20260516-*` file.
#[test]
fn split_keys_date_off_filename_not_line_timestamps() {
    let dir = test_dir("split_keys_date_off_filename_not_line_timestamps");
    let orig = dir.join("orig");
    let split_out = dir.join("split-out");
    std::fs::create_dir_all(&orig).unwrap();

    // Last line is timestamped on the 16th but lives in the 15th's file.
    let node1 = "2026-05-15T23:59:59 late-on-the-15th\n\
                 2026-05-16T00:00:01 just-past-midnight\n";
    write_gz(&orig.join("debug.log-20260515-node1.gz"), node1);

    let merged_dir = dir.join("merged");
    run_ok(
        bin().args(["merge", "-o"]).arg(&merged_dir).arg(&orig),
        "merge",
    );
    run_ok(
        bin()
            .args(["split", "-o"])
            .arg(&split_out)
            .arg(merged_dir.join("debug.log-20260515.zst")),
        "split",
    );

    assert_eq!(
        read_gz(&split_out.join("debug.log-20260515-node1.gz")),
        node1,
        "cross-midnight line must round-trip into the filename's day",
    );
    assert!(
        !split_out.join("debug.log-20260516-node1.gz").exists(),
        "no line should leak into a next-day file",
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
    let merged_dir = dir.join("merged");
    let split_out = dir.join("split-out");

    run_ok(
        bin().args(["merge", "-o"]).arg(&merged_dir).arg(&fixtures),
        "merge (fixtures)",
    );
    // merge emits one `debug.log-<date>.zst` per day; split each back into the
    // shared output dir so every original fixture is reconstructed.
    for entry in std::fs::read_dir(&merged_dir).unwrap() {
        let merged = entry.unwrap().path();
        run_ok(
            bin().args(["split", "-o"]).arg(&split_out).arg(&merged),
            "split (fixtures)",
        );
    }

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

/// A last line without a newline must not run into the next node's line.
#[test]
fn merge_terminates_unterminated_final_line() {
    let dir = test_dir("merge_terminates_unterminated_final_line");
    let orig = dir.join("orig");
    let split_out = dir.join("split-out");
    std::fs::create_dir_all(&orig).unwrap();

    // node1's last line has no trailing newline.
    write_gz(
        &orig.join("debug.log-20260515-node1.gz"),
        "2026-05-15T00:06:30 first-on-node1\n\
         2026-05-15T00:06:32 third-on-node1",
    );
    let node2 = "2026-05-15T00:06:31 second-on-node2\n\
                 2026-05-15T00:06:33 fourth-on-node2\n";
    write_gz(&orig.join("debug.log-20260515-node2.gz"), node2);

    let merged_dir = dir.join("merged");
    let merged = merged_dir.join("debug.log-20260515.zst");
    run_ok(
        bin()
            .args(["merge", "--check", "-o"])
            .arg(&merged_dir)
            .arg(&orig),
        "merge",
    );
    assert_eq!(
        read_zst(&merged),
        "node1 2026-05-15T00:06:30 first-on-node1\n\
         node2 2026-05-15T00:06:31 second-on-node2\n\
         node1 2026-05-15T00:06:32 third-on-node1\n\
         node2 2026-05-15T00:06:33 fourth-on-node2\n",
    );

    run_ok(
        bin().args(["split", "-o"]).arg(&split_out).arg(&merged),
        "split",
    );
    // node2 keeps both lines; node1 gets back the newline merge added.
    assert_eq!(
        read_gz(&split_out.join("debug.log-20260515-node2.gz")),
        node2
    );
    assert_eq!(
        read_gz(&split_out.join("debug.log-20260515-node1.gz")),
        "2026-05-15T00:06:30 first-on-node1\n\
         2026-05-15T00:06:32 third-on-node1\n",
    );
}

/// Two `--save-state` runs over the same input must write identical files.
#[test]
fn save_state_is_byte_identical_across_runs() {
    let dir = test_dir("save_state_is_byte_identical_across_runs");
    let fixture = dir.join("debug.log");

    // Enough categories and templates that any hash ordering would show up.
    let mut content = String::new();
    for i in 1..=20 {
        for cat in ["net", "mempool", "validation", "bench", "addrman"] {
            content.push_str(&format!(
                "2026-06-06T12:34:56Z [{cat}] event-{i} happened on peer={i}\n"
            ));
        }
        content.push_str(&format!("\tcontinuation line {i} with no timestamp\n"));
    }
    std::fs::write(&fixture, &content).unwrap();

    let state_a = dir.join("a.jsonl");
    let state_b = dir.join("b.jsonl");
    for state in [&state_a, &state_b] {
        run_ok(
            bin()
                .args(["templates", "--save-state"])
                .arg(state)
                .arg(&fixture),
            "templates --save-state",
        );
    }

    let a = std::fs::read_to_string(&state_a).unwrap();
    let b = std::fs::read_to_string(&state_b).unwrap();
    assert!(a.lines().count() > 20, "expected many clusters: {a}");
    assert_eq!(a, b, "state files must be reproducible across runs");
}

/// `templates` on a merged `.zst` must strip the `<node> ` prefix merge added,
/// otherwise every line looks timestamp-less and loses its category.
#[test]
fn templates_strips_the_node_prefix_from_merged_input() {
    let dir = test_dir("templates_strips_the_node_prefix_from_merged_input");
    let inputs = dir.join("inputs");
    std::fs::create_dir_all(&inputs).unwrap();

    let per_node = "2026-05-15T00:00:01 [net] received: ping (8 bytes) peer=1\n\
                    2026-05-15T00:00:02 [validation] UpdateTip: new best=aa\n";
    for node in ["alpha", "beta"] {
        write_gz(
            &inputs.join(format!("debug.log-20260515-{node}.gz")),
            per_node,
        );
    }
    let merged_dir = dir.join("merged");
    run_ok(
        bin().args(["merge", "-o"]).arg(&merged_dir).arg(&inputs),
        "merge",
    );

    let out = bin()
        .arg("templates")
        .arg(merged_dir.join("debug.log-20260515.zst"))
        .output()
        .expect("spawn templates");
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("=== net ==="), "categories: {stdout}");
    assert!(stdout.contains("=== validation ==="), "{stdout}");
    assert!(
        !stdout.contains("alpha") && !stdout.contains("beta"),
        "node name leaked into a template: {stdout}"
    );
    assert!(
        !stdout.contains("(no category)"),
        "prefix not stripped: {stdout}"
    );
    // Two nodes, one line each per template.
    assert!(stdout.contains("     2 received"), "{stdout}");
}

/// `--min-count`, `--top` and `--category` each narrow the report.
#[test]
fn templates_filters_by_count_rank_and_category() {
    let dir = test_dir("templates_filters_by_count_rank_and_category");
    let fixture = dir.join("debug.log");

    let mut content = String::new();
    for _ in 0..5 {
        content.push_str("2026-06-06T12:00:00Z [net] common line one two\n");
    }
    for _ in 0..2 {
        content.push_str("2026-06-06T12:00:00Z [net] rare line three four\n");
    }
    content.push_str("2026-06-06T12:00:00Z [bench] other category line\n");
    std::fs::write(&fixture, &content).unwrap();

    let report = |args: &[&str]| -> String {
        let out = bin()
            .arg("templates")
            .arg(&fixture)
            .args(args)
            .output()
            .expect("spawn templates");
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap()
    };

    let all = report(&[]);
    assert!(all.contains("common") && all.contains("rare") && all.contains("other"));
    // Categories in order, one blank line between the blocks and none before.
    assert!(all.starts_with("=== bench ==="), "{all}");
    assert!(all.contains("\n\n=== net ==="), "{all}");

    let min = report(&["--min-count", "3"]);
    assert!(min.contains("common"), "{min}");
    assert!(!min.contains("rare"), "count 2 should be dropped: {min}");
    assert!(!min.contains("other"), "count 1 should be dropped: {min}");

    // Sorted by count descending, so the top one is the common template.
    let top = report(&["--top", "1"]);
    assert!(top.contains("common") && !top.contains("rare"), "{top}");
    assert!(top.contains("other"), "--top is per category: {top}");

    let only_net = report(&["--category", "net"]);
    assert!(only_net.contains("common") && only_net.contains("rare"));
    assert!(!only_net.contains("other"), "{only_net}");
    assert!(!only_net.contains("=== bench ==="), "{only_net}");
}

/// Only `debug.log-<date>-<node>.gz` names are picked up, and a node name
/// holding a space is refused rather than merged into a file split can't undo.
#[test]
fn merge_input_discovery_rejects_unusable_names() {
    let dir = test_dir("merge_input_discovery_rejects_unusable_names");
    let inputs = dir.join("inputs");
    std::fs::create_dir_all(&inputs).unwrap();

    let line = "2026-05-15T00:00:01 only-real-line\n";
    write_gz(&inputs.join("debug.log-20260515-node1.gz"), line);
    // None of these match the shape and all must be ignored.
    for ignored in [
        "debug.log-20260515-node2.txt",
        "other.log-20260515-node3.gz",
        "debug.log-20260515.gz",
        "debug.log--node4.gz",
        "debug.log-20260515-.gz",
    ] {
        write_gz(&inputs.join(ignored), "2026-05-15T00:00:02 ignored-line\n");
    }

    let merged_dir = dir.join("merged");
    run_ok(
        bin().args(["merge", "-o"]).arg(&merged_dir).arg(&inputs),
        "merge",
    );
    assert_eq!(
        read_zst(&merged_dir.join("debug.log-20260515.zst")),
        format!("node1 {line}"),
    );

    // A space in the node name is an error, not a silently broken merge.
    write_gz(&inputs.join("debug.log-20260515-node 5.gz"), line);
    let out = bin()
        .args(["merge", "-o"])
        .arg(dir.join("merged2"))
        .arg(&inputs)
        .output()
        .expect("spawn merge");
    assert!(!out.status.success(), "should have failed");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("node 5"), "stderr: {stderr}");
}

/// A debug.log can hold bytes that are not valid UTF-8 (anything a peer sent,
/// or a rotated file cut mid-character). Those must round-trip untouched.
#[test]
fn non_utf8_bytes_round_trip_and_still_cluster() {
    let dir = test_dir("non_utf8_bytes_round_trip_and_still_cluster");
    let orig = dir.join("orig");
    let split_out = dir.join("split-out");
    std::fs::create_dir_all(&orig).unwrap();

    let mut node1: Vec<u8> = b"2026-05-15T00:00:01 [net] receive version message: /".to_vec();
    node1.extend_from_slice(&[0xff, 0xfe, 0x80]);
    node1.extend_from_slice(b"/ peer=1\n2026-05-15T00:00:03 [net] plain line peer=2\n");
    let node2 = b"2026-05-15T00:00:02 [net] plain line peer=3\n".to_vec();

    let write_raw = |path: &Path, bytes: &[u8]| {
        let f = std::fs::File::create(path).unwrap();
        let mut enc = GzEncoder::new(f, Compression::default());
        enc.write_all(bytes).unwrap();
        enc.finish().unwrap();
    };
    write_raw(&orig.join("debug.log-20260515-node1.gz"), &node1);
    write_raw(&orig.join("debug.log-20260515-node2.gz"), &node2);

    let merged_dir = dir.join("merged");
    let merged = merged_dir.join("debug.log-20260515.zst");
    run_ok(
        bin()
            .args(["merge", "--check", "-o"])
            .arg(&merged_dir)
            .arg(&orig),
        "merge",
    );
    run_ok(
        bin().args(["split", "-o"]).arg(&split_out).arg(&merged),
        "split",
    );

    let read_raw = |path: &Path| -> Vec<u8> {
        let f = std::fs::File::open(path).unwrap();
        let mut dec = MultiGzDecoder::new(f);
        let mut v = Vec::new();
        dec.read_to_end(&mut v).unwrap();
        v
    };
    assert_eq!(
        read_raw(&split_out.join("debug.log-20260515-node1.gz")),
        node1
    );
    assert_eq!(
        read_raw(&split_out.join("debug.log-20260515-node2.gz")),
        node2
    );

    // `templates` reads the same file without giving up on it.
    let out = bin()
        .arg("templates")
        .arg(&merged)
        .output()
        .expect("spawn templates");
    assert!(
        out.status.success(),
        "templates failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("=== net ==="), "{stdout}");
    assert!(stdout.contains("plain line <PEER>"), "{stdout}");
}

/// An empty input file contributes no lines but still has to be reported and
/// counted as round-tripping.
#[test]
fn merge_handles_an_empty_input_file() {
    let dir = test_dir("merge_handles_an_empty_input_file");
    let inputs = dir.join("inputs");
    std::fs::create_dir_all(&inputs).unwrap();
    write_gz(&inputs.join("debug.log-20260515-empty.gz"), "");
    write_gz(
        &inputs.join("debug.log-20260515-full.gz"),
        "2026-05-15T00:00:01 only-line\n",
    );

    let merged_dir = dir.join("merged");
    let out = bin()
        .args(["merge", "--check", "-o"])
        .arg(&merged_dir)
        .arg(&inputs)
        .output()
        .expect("spawn merge --check");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("empty          OK    0 lines"), "{stdout}");
    assert!(
        stdout.contains("2/2 files round-trip identical"),
        "{stdout}"
    );
    assert_eq!(
        read_zst(&merged_dir.join("debug.log-20260515.zst")),
        "full 2026-05-15T00:00:01 only-line\n",
    );

    // The empty node gets an empty file back, not no file at all.
    let split_out = dir.join("split-out");
    run_ok(
        bin()
            .args(["split", "-o"])
            .arg(&split_out)
            .arg(merged_dir.join("debug.log-20260515.zst")),
        "split",
    );
    assert!(!split_out.join("debug.log-20260515-empty.gz").exists());
}

/// `merge` and `split` fail cleanly, with a message, on inputs they can't use.
#[test]
fn merge_and_split_reject_unusable_inputs() {
    let dir = test_dir("merge_and_split_reject_unusable_inputs");
    let empty_dir = dir.join("no-logs");
    std::fs::create_dir_all(&empty_dir).unwrap();

    let out = bin()
        .args(["merge", "-o"])
        .arg(dir.join("merged"))
        .arg(&empty_dir)
        .output()
        .expect("spawn merge");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no debug.log-<date>-<node>.gz files"),
        "{stderr}"
    );

    // A merged log with a line that has no `<node> ` prefix.
    let bad = dir.join("debug.log-20260515.zst");
    {
        let f = std::fs::File::create(&bad).unwrap();
        let mut enc = zstd::Encoder::new(f, 1).unwrap();
        enc.write_all(b"node1 2026-05-15T00:00:01 fine\nno-prefix-here\n")
            .unwrap();
        enc.finish().unwrap();
    }
    let out = bin()
        .args(["split", "-o"])
        .arg(dir.join("split-out"))
        .arg(&bad)
        .output()
        .expect("spawn split");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("line 2"), "should name the line: {stderr}");
    assert!(stderr.contains("missing `<node> ` prefix"), "{stderr}");

    // And an input whose name carries no date.
    let misnamed = dir.join("merged-log.zst");
    std::fs::copy(&bad, &misnamed).unwrap();
    let out = bin()
        .args(["split", "-o"])
        .arg(dir.join("split-out2"))
        .arg(&misnamed)
        .output()
        .expect("spawn split");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("is not of the form"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Whether lines carry merge's `<node> ` prefix is a property of the content,
/// not of the compression: a per-node log that happens to be zstd must not
/// have its timestamp mistaken for a node name.
#[test]
fn templates_does_not_strip_a_prefix_that_is_not_there() {
    let dir = test_dir("templates_does_not_strip_a_prefix_that_is_not_there");
    // Leads with an indented continuation line that embeds a timestamp, the
    // shape most likely to be mistaken for a node prefix.
    let log = " 2026-05-15T00:00:00 indented continuation\n\
               2026-05-15T00:00:01 [net] received: ping (8 bytes) peer=1\n\
               \tcontinuation line of its own\n\
               2026-05-15T00:00:02 [net] received: ping (9 bytes) peer=2\n";

    // The same per-node log plain, gzipped and zstd-compressed.
    let plain = dir.join("debug.log");
    std::fs::write(&plain, log).unwrap();
    let gz = dir.join("debug.log.gz");
    write_gz(&gz, log);
    let zst = dir.join("debug.log.zst");
    {
        let f = std::fs::File::create(&zst).unwrap();
        let mut enc = zstd::Encoder::new(f, 1).unwrap();
        enc.write_all(log.as_bytes()).unwrap();
        enc.finish().unwrap();
    }

    let report = |path: &Path| -> String {
        let out = bin().arg("templates").arg(path).output().expect("spawn");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    let expected = report(&plain);
    assert!(expected.contains("=== net ==="), "{expected}");
    assert!(
        expected.contains("received : ping ( <BYTES> ) <PEER>"),
        "{expected}"
    );
    assert_eq!(report(&gz), expected, "gzip read differently");
    assert_eq!(report(&zst), expected, "zstd read differently");
}

/// `--json` output has to escape whatever control bytes a log line carried.
#[test]
fn templates_json_escapes_control_characters() {
    let dir = test_dir("templates_json_escapes_control_characters");
    let fixture = dir.join("debug.log");
    let mut bytes: Vec<u8> = b"2026-05-15T00:00:01 [net] odd".to_vec();
    bytes.push(0x01);
    bytes.extend_from_slice(b"byte here\n");
    std::fs::write(&fixture, &bytes).unwrap();

    let out = bin()
        .args(["templates", "--json"])
        .arg(&fixture)
        .output()
        .expect("spawn templates");
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("\\u0001"),
        "unescaped control byte: {stdout:?}"
    );
    assert!(
        !stdout.contains('\u{1}'),
        "raw control byte in JSON: {stdout:?}"
    );
    assert!(
        !stdout.contains("\\u0020"),
        "spaces stay literal: {stdout:?}"
    );
}

/// Writes a gzip file truncated mid-stream, which decompresses part-way and
/// then errors.
fn write_truncated_gz(path: &Path, lines: usize) {
    let mut content = String::new();
    for i in 0..lines {
        content.push_str(&format!("2026-05-15T00:00:01.{i:06}Z line-{i}\n"));
    }
    let mut gz = Vec::new();
    {
        let mut enc = GzEncoder::new(&mut gz, Compression::default());
        enc.write_all(content.as_bytes()).unwrap();
        enc.finish().unwrap();
    }
    gz.truncate(gz.len() / 2);
    std::fs::write(path, &gz).unwrap();
}

/// A merge that fails part-way must leave no merged log at all — not a
/// truncated one, and not in place of a good one from an earlier run.
#[test]
fn a_failed_merge_leaves_no_output_behind() {
    let dir = test_dir("a_failed_merge_leaves_no_output_behind");
    let inputs = dir.join("inputs");
    let merged_dir = dir.join("merged");
    std::fs::create_dir_all(&inputs).unwrap();

    let good = "2026-05-15T00:00:09 good-line\n";
    write_gz(&inputs.join("debug.log-20260515-good.gz"), good);

    // First run succeeds and is the output we must not lose.
    run_ok(
        bin().args(["merge", "-o"]).arg(&merged_dir).arg(&inputs),
        "merge",
    );
    let merged = merged_dir.join("debug.log-20260515.zst");
    assert_eq!(read_zst(&merged), format!("good {good}"));

    // Second run has an input that dies mid-stream.
    write_truncated_gz(&inputs.join("debug.log-20260515-broken.gz"), 20_000);
    let out = bin()
        .args(["merge", "-o"])
        .arg(&merged_dir)
        .arg(&inputs)
        .output()
        .expect("spawn merge");
    assert!(!out.status.success(), "merge should have failed");

    assert!(
        String::from_utf8_lossy(&out.stderr).contains("debug.log-20260515-broken.gz"),
        "error should name the file: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The earlier good output survived, and no temp file was left lying around.
    assert_eq!(read_zst(&merged), format!("good {good}"));
    let leftovers: Vec<_> = std::fs::read_dir(&merged_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".tmp"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temp files left behind: {leftovers:?}"
    );

    // Same from scratch: a failed first run writes nothing at all.
    let fresh = dir.join("fresh");
    let out = bin()
        .args(["merge", "-o"])
        .arg(&fresh)
        .arg(&inputs)
        .output()
        .expect("spawn merge");
    assert!(!out.status.success());
    let left: Vec<_> = std::fs::read_dir(&fresh)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(left.is_empty(), "output dir should be empty: {left:?}");
}

/// Lines sharing a timestamp come out in node order, and each node's own
/// lines keep their order whatever else lands between them.
#[test]
fn equal_timestamps_order_by_node_and_keep_file_order() {
    let dir = test_dir("equal_timestamps_order_by_node_and_keep_file_order");
    let inputs = dir.join("inputs");
    std::fs::create_dir_all(&inputs).unwrap();

    // Every line shares one timestamp, so only the tie-break decides.
    let ts = "2026-05-15T00:00:01.000000Z";
    for node in ["charlie", "alice", "bob"] {
        write_gz(
            &inputs.join(format!("debug.log-20260515-{node}.gz")),
            &format!("{ts} {node}-first\n{ts} {node}-second\n"),
        );
    }

    let merged_dir = dir.join("merged");
    run_ok(
        bin().args(["merge", "-o"]).arg(&merged_dir).arg(&inputs),
        "merge",
    );
    let merged = read_zst(&merged_dir.join("debug.log-20260515.zst"));
    let nodes: Vec<&str> = merged
        .lines()
        .map(|l| l.split_once(' ').unwrap().0)
        .collect();
    // Ties break on reader index and readers are ordered by path, so the
    // first node empties its run of same-instant lines before the next.
    assert_eq!(
        nodes,
        vec!["alice", "alice", "bob", "bob", "charlie", "charlie"]
    );
    for node in ["alice", "bob", "charlie"] {
        let own: Vec<&str> = merged
            .lines()
            .filter(|l| l.starts_with(&format!("{node} ")))
            .map(|l| l.rsplit_once('-').unwrap().1)
            .collect();
        assert_eq!(own, vec!["first", "second"], "{node} lines reordered");
    }
}

/// The merged *content* must not depend on the compression level; only the
/// zstd framing may differ.
#[test]
fn compression_level_does_not_change_the_content() {
    let dir = test_dir("compression_level_does_not_change_the_content");
    let inputs = dir.join("inputs");
    std::fs::create_dir_all(&inputs).unwrap();
    for node in ["node1", "node2"] {
        let mut content = String::new();
        for i in 0..200 {
            content.push_str(&format!(
                "2026-05-15T00:00:{:02}.{i:06}Z {node}-{i}\n",
                i % 60
            ));
        }
        write_gz(
            &inputs.join(format!("debug.log-20260515-{node}.gz")),
            &content,
        );
    }

    let at_level = |level: &str| -> String {
        let out_dir = dir.join(format!("merged-{level}"));
        run_ok(
            bin()
                .args(["merge", "-l", level, "-o"])
                .arg(&out_dir)
                .arg(&inputs),
            "merge",
        );
        read_zst(&out_dir.join("debug.log-20260515.zst"))
    };
    let low = at_level("1");
    assert_eq!(low.lines().count(), 400);
    assert_eq!(at_level("19"), low);
    assert_eq!(at_level("22"), low);
}

/// Node names are taken whole from the filename: dashes, dots, non-ASCII and
/// one name being a prefix of another all have to survive the round-trip.
#[test]
fn awkward_node_names_round_trip() {
    let dir = test_dir("awkward_node_names_round_trip");
    let orig = dir.join("orig");
    let split_out = dir.join("split-out");
    std::fs::create_dir_all(&orig).unwrap();

    let nodes = ["a", "ab", "node-with-dashes", "node.with.dots", "nøde"];
    for (i, node) in nodes.iter().enumerate() {
        write_gz(
            &orig.join(format!("debug.log-20260515-{node}.gz")),
            &format!("2026-05-15T00:00:{i:02} line-for-{node}\n"),
        );
    }

    let merged_dir = dir.join("merged");
    run_ok(
        bin()
            .args(["merge", "--check", "-o"])
            .arg(&merged_dir)
            .arg(&orig),
        "merge",
    );
    run_ok(
        bin()
            .args(["split", "-o"])
            .arg(&split_out)
            .arg(merged_dir.join("debug.log-20260515.zst")),
        "split",
    );
    for (i, node) in nodes.iter().enumerate() {
        assert_eq!(
            read_gz(&split_out.join(format!("debug.log-20260515-{node}.gz"))),
            format!("2026-05-15T00:00:{i:02} line-for-{node}\n"),
            "node {node} did not round-trip",
        );
    }
}

/// Lines a log can contain that aren't plain ASCII text: CRLF endings, a NUL
/// byte, a very long line, and a leading line with no timestamp at all.
#[test]
fn awkward_line_shapes_round_trip() {
    let dir = test_dir("awkward_line_shapes_round_trip");
    let orig = dir.join("orig");
    let split_out = dir.join("split-out");
    std::fs::create_dir_all(&orig).unwrap();

    let mut node1: Vec<u8> = b"no timestamp on this first line\r\n".to_vec();
    node1.extend_from_slice(b"2026-05-15T00:00:02 crlf-line\r\n");
    node1.extend_from_slice(b"2026-05-15T00:00:03 nul");
    node1.push(0);
    node1.extend_from_slice(b"byte\n");
    node1.extend_from_slice(b"2026-05-15T00:00:04 ");
    node1.extend_from_slice(&vec![b'x'; 300_000]);
    node1.push(b'\n');
    let node2 = b"2026-05-15T00:00:01 before-everything\n".to_vec();

    let write_raw = |path: &Path, bytes: &[u8]| {
        let f = std::fs::File::create(path).unwrap();
        let mut enc = GzEncoder::new(f, Compression::default());
        enc.write_all(bytes).unwrap();
        enc.finish().unwrap();
    };
    write_raw(&orig.join("debug.log-20260515-node1.gz"), &node1);
    write_raw(&orig.join("debug.log-20260515-node2.gz"), &node2);

    let merged_dir = dir.join("merged");
    let merged = merged_dir.join("debug.log-20260515.zst");
    run_ok(
        bin()
            .args(["merge", "--check", "-o"])
            .arg(&merged_dir)
            .arg(&orig),
        "merge",
    );
    run_ok(
        bin().args(["split", "-o"]).arg(&split_out).arg(&merged),
        "split",
    );

    let read_raw = |path: &Path| -> Vec<u8> {
        let f = std::fs::File::open(path).unwrap();
        let mut dec = MultiGzDecoder::new(f);
        let mut v = Vec::new();
        dec.read_to_end(&mut v).unwrap();
        v
    };
    assert_eq!(
        read_raw(&split_out.join("debug.log-20260515-node1.gz")),
        node1
    );
    assert_eq!(
        read_raw(&split_out.join("debug.log-20260515-node2.gz")),
        node2
    );

    // A leading line with no timestamp inherits the zero key, so it sorts
    // ahead of everything — including the other node's earlier line.
    let f = std::fs::File::open(&merged).unwrap();
    let mut first = Vec::new();
    std::io::BufReader::new(zstd::Decoder::new(f).unwrap())
        .read_until(b'\n', &mut first)
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&first),
        "node1 no timestamp on this first line\r\n"
    );
}

/// A rotated log is often several gzip members in one file. All of them have
/// to be read, not just the first.
#[test]
fn a_multi_member_gzip_is_read_whole() {
    let dir = test_dir("a_multi_member_gzip_is_read_whole");
    let inputs = dir.join("inputs");
    std::fs::create_dir_all(&inputs).unwrap();

    // Two gzip streams concatenated, as `cat a.gz b.gz` would give.
    let mut concatenated = Vec::new();
    for part in [
        "2026-05-15T00:00:01 member-one\n",
        "2026-05-15T00:00:03 member-two\n",
    ] {
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(part.as_bytes()).unwrap();
        concatenated.extend_from_slice(&enc.finish().unwrap());
    }
    std::fs::write(inputs.join("debug.log-20260515-rotated.gz"), &concatenated).unwrap();
    write_gz(
        &inputs.join("debug.log-20260515-other.gz"),
        "2026-05-15T00:00:02 in-between\n",
    );

    let merged_dir = dir.join("merged");
    let out = bin()
        .args(["merge", "--check", "-o"])
        .arg(&merged_dir)
        .arg(&inputs)
        .output()
        .expect("spawn merge");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("rotated        OK    2 lines"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(
        read_zst(&merged_dir.join("debug.log-20260515.zst")),
        "rotated 2026-05-15T00:00:01 member-one\n\
         other 2026-05-15T00:00:02 in-between\n\
         rotated 2026-05-15T00:00:03 member-two\n",
    );
}

/// A timestamp as the merge orders it: whole seconds and fractional seconds
/// have to compare against each other, so the fraction is scaled to nanos
/// rather than compared as text.
fn timestamp_key(ts: &str) -> (String, u64) {
    match ts.split_once('.') {
        Some((head, fraction)) => {
            let digits: String = fraction.chars().take_while(char::is_ascii_digit).collect();
            let nanos = digits.parse::<u64>().unwrap() * 10u64.pow(9 - digits.len() as u32);
            (head.to_string(), nanos)
        }
        None => (ts.to_string(), 0),
    }
}

/// Deterministic little PRNG so a failure is reproducible from the seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Builds one node's log. Timestamps only move forwards — a k-way merge can
/// only order inputs that are each already chronological — but often stand
/// still, so ties are common. Mixed in are the line shapes that have broken
/// things before.
fn random_log(rng: &mut Rng, date: &str, lines: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut second = 0u64;
    let mut micros = 0u64;
    for i in 0..lines {
        let shape = rng.below(10);
        // Whole-second lines read as .000000, so they can only follow a
        // fractional one on a later second.
        let whole = shape != 0 && !(4..=7).contains(&shape);
        if whole && (micros != 0 || rng.below(3) != 0) {
            second += 1 + rng.below(3);
            micros = 0;
        } else if !whole && rng.below(3) != 0 {
            micros += 1 + rng.below(1000);
        }
        let (m, s) = (second / 60, second % 60);
        match shape {
            // Continuation line, no timestamp of its own.
            0 => out.extend_from_slice(format!("\tcontinuation {i}\n").as_bytes()),
            1..=3 => {
                out.extend_from_slice(format!("{date}T00:{m:02}:{s:02} whole-{i}\n").as_bytes())
            }
            4..=7 => out.extend_from_slice(
                format!(
                    "{date}T00:{m:02}:{s:02}.{micros:06}Z frac-{i} peer={}\n",
                    rng.below(500)
                )
                .as_bytes(),
            ),
            8 => out.extend_from_slice(format!("{date}T00:{m:02}:{s:02} crlf-{i}\r\n").as_bytes()),
            // Bytes that are not valid UTF-8, plus a NUL.
            _ => {
                out.extend_from_slice(format!("{date}T00:{m:02}:{s:02} raw-{i} ").as_bytes());
                out.extend_from_slice(&[0xff, 0x00, 0xc3, 0x28]);
                out.push(b'\n');
            }
        }
    }
    out
}

/// merge -> split over randomly generated messy logs: every input file has to
/// come back byte-identical, every line has to survive exactly once, and the
/// merged stream has to be ordered.
#[test]
fn randomized_merge_split_round_trip() {
    for seed in 1..=25u64 {
        let dir = test_dir(&format!("randomized_merge_split_round_trip-{seed}"));
        let orig = dir.join("orig");
        let split_out = dir.join("split-out");
        std::fs::create_dir_all(&orig).unwrap();

        let mut rng = Rng(seed);
        let node_count = 2 + rng.below(5) as usize;
        let dates = ["2026-05-15", "2026-05-16"];
        let mut written: Vec<(String, String, Vec<u8>)> = Vec::new();

        for n in 0..node_count {
            let node = format!("node{n}");
            for date in dates {
                let lines = rng.below(40) as usize;
                let mut content = random_log(&mut rng, date, lines);
                // Every few files, drop the final newline.
                if !content.is_empty() && rng.below(4) == 0 {
                    content.pop();
                }
                let compact = date.replace('-', "");
                let f = std::fs::File::create(orig.join(format!("debug.log-{compact}-{node}.gz")))
                    .unwrap();
                let mut enc = GzEncoder::new(f, Compression::default());
                enc.write_all(&content).unwrap();
                enc.finish().unwrap();
                written.push((compact, node.clone(), content));
            }
        }

        let merged_dir = dir.join("merged");
        let out = bin()
            .args(["merge", "--check", "-o"])
            .arg(&merged_dir)
            .arg(&orig)
            .output()
            .expect("spawn merge");
        assert!(
            out.status.success(),
            "seed {seed}: merge failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains(&format!(
                "{0}/{0} files round-trip identical",
                written.len()
            )),
            "seed {seed}: {stdout}"
        );

        for date in dates {
            let compact = date.replace('-', "");
            run_ok(
                bin()
                    .args(["split", "-o"])
                    .arg(&split_out)
                    .arg(merged_dir.join(format!("debug.log-{compact}.zst"))),
                "split",
            );
        }

        for (compact, node, content) in &written {
            let path = split_out.join(format!("debug.log-{compact}-{node}.gz"));
            if content.is_empty() {
                assert!(!path.exists(), "seed {seed}: empty log produced a file");
                continue;
            }
            let f = std::fs::File::open(&path)
                .unwrap_or_else(|e| panic!("seed {seed}: {}: {e}", path.display()));
            let mut got = Vec::new();
            MultiGzDecoder::new(f).read_to_end(&mut got).unwrap();
            // merge terminates a last line that had no newline; otherwise the
            // bytes have to match exactly.
            let mut want = content.clone();
            if want.last() != Some(&b'\n') {
                want.push(b'\n');
            }
            assert_eq!(
                got, want,
                "seed {seed}: {compact}-{node} did not round-trip",
            );
        }

        // Timestamps in the merged stream never go backwards, and no line is
        // lost or duplicated.
        for date in dates {
            let compact = date.replace('-', "");
            let f =
                std::fs::File::open(merged_dir.join(format!("debug.log-{compact}.zst"))).unwrap();
            let mut reader = std::io::BufReader::new(zstd::Decoder::new(f).unwrap());
            let mut line = Vec::new();
            let mut previous = (String::new(), 0u64);
            let mut count = 0usize;
            loop {
                line.clear();
                if reader.read_until(b'\n', &mut line).unwrap() == 0 {
                    break;
                }
                count += 1;
                let text = String::from_utf8_lossy(&line);
                let rest = text.split_once(' ').expect("node prefix").1;
                if let Some(ts) = rest.split_whitespace().next()
                    && ts.starts_with(date)
                {
                    let key = timestamp_key(ts);
                    assert!(
                        key >= previous,
                        "seed {seed}: {ts} came after {}",
                        previous.0
                    );
                    previous = key;
                }
            }
            let expected: usize = written
                .iter()
                .filter(|(d, _, _)| d == &compact)
                .map(|(_, _, c)| {
                    c.iter().filter(|&&b| b == b'\n').count()
                        + usize::from(!c.is_empty() && c.last() != Some(&b'\n'))
                })
                .sum();
            assert_eq!(count, expected, "seed {seed}: line count for {compact}");
        }
    }
}

/// Two merges into the same output directory must not share a temp file, or
/// one could rename the other's half-written bytes into place.
#[test]
fn concurrent_merges_do_not_share_a_temp_file() {
    let dir = test_dir("concurrent_merges_do_not_share_a_temp_file");
    let inputs = dir.join("inputs");
    let merged_dir = dir.join("merged");
    std::fs::create_dir_all(&inputs).unwrap();

    let mut content = String::new();
    for i in 0..5_000 {
        content.push_str(&format!(
            "2026-05-15T00:00:{:02}.{i:06}Z line-{i}\n",
            i % 60
        ));
    }
    write_gz(&inputs.join("debug.log-20260515-node1.gz"), &content);

    let mut children: Vec<_> = (0..4)
        .map(|_| {
            bin()
                .args(["merge", "-o"])
                .arg(&merged_dir)
                .arg(&inputs)
                .spawn()
                .expect("spawn merge")
        })
        .collect();
    for child in &mut children {
        assert!(child.wait().unwrap().success());
    }

    // Whichever run renamed last, the file is one complete merge.
    let merged = read_zst(&merged_dir.join("debug.log-20260515.zst"));
    assert_eq!(merged.lines().count(), 5_000);
    assert!(merged.starts_with("node1 2026-05-15T00:00:00.000000Z line-0\n"));
    let leftovers: Vec<_> = std::fs::read_dir(&merged_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".tmp"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temp files left behind: {leftovers:?}"
    );
}
