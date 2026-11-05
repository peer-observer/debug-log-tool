//! Fuzz targets for the parsers in this repo.
//!
//! Each target is a `fn(&[u8])` that feeds the bytes to one parser and
//! asserts its invariants. The harnesses in `fuzz_targets/` hand them to
//! libFuzzer; `tests/corpus.rs` runs the same functions over a corpus on the
//! pinned stable toolchain.

use std::io::Cursor;

use drnlog::{Analyzer, Slot, TokenKind, line};

// The CLI's timestamp parser is std-only and self-contained, so it can be
// compiled straight into this crate. A binary crate's modules can't be
// imported any other way.
#[allow(dead_code)]
#[path = "../../crates/debug-log-tool/src/timestamp.rs"]
mod cli_timestamp;

/// One fuzz target: bytes in, assertions on the way out.
pub type Target = fn(&[u8]);

/// Every target, by the name of its corpus directory.
pub const TARGETS: &[(&str, Target)] = &[
    ("timestamp", timestamp),
    ("line_parse", line_parse),
    ("tokenize", tokenize),
    ("ingest_line", ingest_line),
    ("state_load", state_load),
    ("state_round_trip", state_round_trip),
];

/// Is `needle` a slice *of* `haystack` rather than a copy of part of it?
/// Holds the parsers to their promise of borrowing from the input.
fn is_slice_of(haystack: &str, needle: &str) -> bool {
    let outer = haystack.as_bytes().as_ptr_range();
    let inner = needle.as_bytes().as_ptr_range();
    outer.start <= inner.start && inner.end <= outer.end
}

/// `<ts> [thread] [category] <content>` splitting.
pub fn line_parse(data: &[u8]) {
    let text = String::from_utf8_lossy(data);
    let parsed = line::parse(&text);
    let trimmed = text.trim_end_matches(['\r', '\n']);

    // Non-empty pieces borrow from the input; an empty one carries no bytes
    // and can be the `""` literal the parser falls back to.
    for part in [
        Some(parsed.content),
        parsed.timestamp,
        parsed.thread,
        parsed.category,
    ] {
        if let Some(part) = part.filter(|p| !p.is_empty()) {
            assert!(
                is_slice_of(&text, part),
                "{part:?} is not a slice of the input"
            );
        }
    }
    assert!(
        trimmed.ends_with(parsed.content),
        "content {:?} is not the tail of {trimmed:?}",
        parsed.content,
    );
    // A thread is only ever reported alongside a category.
    if parsed.thread.is_some() {
        assert!(parsed.category.is_some(), "thread without a category");
    }
    // With no timestamp the whole line is content, carried through untouched.
    if parsed.timestamp.is_none() {
        assert_eq!(parsed.content, trimmed);
    }
    assert_eq!(line::parse(&text), parsed, "parse is not deterministic");
}

/// Atom splitting plus classification.
pub fn tokenize(data: &[u8]) {
    let text = String::from_utf8_lossy(data);
    let tokens = drnlog::tokenize(&text);

    // Tokens are non-empty, in order, and never overlap: the template of a
    // line has to be made of the line's own bytes, left to right.
    let base = text.as_ptr() as usize;
    let mut reached = 0usize;
    for token in &tokens {
        let slice = token.text();
        assert!(!slice.is_empty(), "empty token from {text:?}");
        assert!(
            is_slice_of(&text, slice),
            "token {slice:?} is not a slice of the input"
        );
        let start = slice.as_ptr() as usize - base;
        assert!(
            start >= reached,
            "token {slice:?} overlaps or precedes the last one"
        );
        reached = start + slice.len();

        // Classifying the atom on its own has to agree with its kind in the
        // line. Byte counts are the exception: they fold in the next atom.
        if token.kind() != Some(TokenKind::ByteCount) {
            let (alone, consumed) = drnlog::classify(slice, None);
            assert_eq!(
                alone.kind(),
                token.kind(),
                "{slice:?} classified differently alone"
            );
            assert_eq!(consumed, 1);
        }
    }
    assert!(reached <= text.len());
}

/// The timestamp parser the merge orders lines by.
pub fn timestamp(data: &[u8]) {
    let Some(key) = cli_timestamp::parse_timestamp(data) else {
        return;
    };
    assert!(key.nanos < 1_000_000_000, "nanos out of range: {key:?}");

    // Documented contract: anything after the timestamp is ignored, so a
    // whole log line parses the same as the timestamp alone.
    let mut longer = data.to_vec();
    longer.extend_from_slice(b" trailing bytes");
    assert_eq!(
        cli_timestamp::parse_timestamp(&longer),
        Some(key),
        "trailing bytes changed the key",
    );

    // The whole-seconds head is the same instant without the fraction, which
    // is what makes the two timestamp shapes comparable.
    let head = cli_timestamp::parse_timestamp(&data[..19]).expect("19-byte head must parse");
    assert_eq!(
        head,
        cli_timestamp::TimestampKey { nanos: 0, ..key },
        "head of the timestamp parsed differently",
    );
    assert!(head <= key, "fraction made the key smaller");
}

/// One line through the whole pipeline: parse, tokenize, cluster.
pub fn ingest_line(data: &[u8]) {
    let text = String::from_utf8_lossy(data);
    let mut analyzer = Analyzer::new(4, 0.5);
    analyzer.ingest_line(&text);

    assert_eq!(analyzer.templates().count(), 1, "one line is one cluster");
    let parsed = line::parse(&text);
    let (category, cluster) = analyzer.templates().next().unwrap();
    assert_eq!(category, parsed.category);
    assert_eq!(
        cluster.template.len(),
        drnlog::tokenize(parsed.content).len()
    );
    assert_eq!(cluster.first_seen.as_deref(), parsed.timestamp);
    assert_eq!(cluster.last_seen.as_deref(), parsed.timestamp);
    assert!(
        !cluster.template.contains(&Slot::Star),
        "a single line starred a slot: {:?}",
        cluster.template,
    );

    // The line that created a cluster must always find it again.
    analyzer.ingest_line(&text);
    assert_eq!(
        analyzer.templates().count(),
        1,
        "the same line made a second cluster"
    );
    assert_eq!(analyzer.templates().next().unwrap().1.count, 2);
}

/// The hand-rolled JSONL state parser, on arbitrary bytes.
pub fn state_load(data: &[u8]) {
    let text = String::from_utf8_lossy(data);
    let Ok(loaded) = Analyzer::load(Cursor::new(text.as_bytes())) else {
        return;
    };
    assert_round_trips(&loaded);
}

/// A catalogue built from the input, then put through save and load.
pub fn state_round_trip(data: &[u8]) {
    let text = String::from_utf8_lossy(data);
    let mut analyzer = Analyzer::new(4, 0.5);
    for line in text.lines() {
        analyzer.ingest_line(line);
    }
    assert_round_trips(&analyzer);
}

/// Whatever `save` writes has to load back as the same catalogue, and saving
/// that again has to give the same bytes.
fn assert_round_trips(analyzer: &Analyzer) {
    let saved = save(analyzer);
    let reloaded = Analyzer::load(Cursor::new(saved.clone()))
        .unwrap_or_else(|e| panic!("saved state did not load: {e}\n{}", show(&saved)));
    assert_eq!(
        snapshot(analyzer),
        snapshot(&reloaded),
        "clusters changed across save -> load\n{}",
        show(&saved),
    );
    assert_eq!(
        saved,
        save(&reloaded),
        "save -> load -> save is not stable\n{}",
        show(&saved),
    );
}

fn save(analyzer: &Analyzer) -> Vec<u8> {
    let mut buf = Vec::new();
    analyzer
        .save(&mut buf)
        .expect("writing to a Vec cannot fail");
    buf
}

/// Everything `save` is supposed to preserve, one comparable row per
/// cluster. `Slot` is not `Ord`, and the rows come out of a `HashMap`, so
/// they are rendered and sorted as text.
fn snapshot(analyzer: &Analyzer) -> Vec<String> {
    let mut rows: Vec<String> = analyzer
        .templates()
        .map(|(category, c)| {
            format!(
                "{category:?} id={} count={} first={:?} last={:?} {:?}",
                c.id, c.count, c.first_seen, c.last_seen, c.template,
            )
        })
        .collect();
    rows.sort();
    rows
}

fn show(saved: &[u8]) -> String {
    String::from_utf8_lossy(saved).escape_debug().to_string()
}
