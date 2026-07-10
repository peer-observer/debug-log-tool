//! Interleave gzipped per-node `debug.log` files into one timestamp-ordered
//! zstd log *per day*, with each line prefixed by the node name parsed from
//! its source filename. Inputs are grouped by the `<date>` segment of their
//! filename, so a directory holding several days' logs yields one
//! `debug.log-<date>.zst` per day.
//!
//! Each day's merge is a streaming k-way merge: at any time the heap holds at
//! most one pending line per input file, so memory use is bounded by the
//! number of inputs rather than the total log size.

use std::cmp::Reverse;
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BinaryHeap, HashMap};
use std::fs::File;
use std::hash::Hasher;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use flate2::read::MultiGzDecoder;

use crate::split;
use crate::timestamp::{TimestampKey, parse_timestamp};

const FILE_PREFIX: &str = "debug.log-";
const FILE_SUFFIX: &str = ".gz";

pub struct MergeInput {
    pub path: PathBuf,
    pub node: String,
    pub date: String,
}

/// Scan `dir` for files named `debug.log-<date>-<node>.gz`. Anything that
/// doesn't match that shape is silently skipped. The returned list is sorted
/// by path so merge output is deterministic across runs.
pub fn discover_inputs(dir: &Path) -> io::Result<Vec<MergeInput>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let Some(fname) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !fname.starts_with(FILE_PREFIX) || !fname.ends_with(FILE_SUFFIX) {
            continue;
        }
        let middle = &fname[FILE_PREFIX.len()..fname.len() - FILE_SUFFIX.len()];
        // `middle` looks like `<date>-<node>`; the node is everything after
        // the first `-` so node names containing dashes are preserved.
        let Some((date, node)) = middle.split_once('-') else {
            continue;
        };
        if date.is_empty() || node.is_empty() {
            continue;
        }
        let node = node.to_string();
        let date = date.to_string();
        out.push(MergeInput { path, node, date });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

struct Reader {
    node: String,
    inner: BufReader<MultiGzDecoder<File>>,
    // Timestamp of the most recent line we successfully parsed. Lines without
    // their own timestamp (continuation lines, multi-line errors) inherit it
    // so they stay glued to the entry they belong to.
    last_key: TimestampKey,
    // In `--check` mode, a running hash of this file's raw decompressed content
    // (every line as read). Compared against the hash of what `split` would
    // reconstruct for this node to prove the round-trip. `None` when not
    // checking so a normal merge pays no hashing cost.
    input_hasher: Option<DefaultHasher>,
}

impl Reader {
    fn open(input: MergeInput, check: bool) -> io::Result<Self> {
        let f = File::open(&input.path)?;
        Ok(Self {
            node: input.node,
            inner: BufReader::new(MultiGzDecoder::new(f)),
            last_key: TimestampKey::default(),
            input_hasher: check.then(DefaultHasher::new),
        })
    }

    fn next_line(&mut self) -> io::Result<Option<(TimestampKey, String)>> {
        let mut buf = String::new();
        if self.inner.read_line(&mut buf)? == 0 {
            return Ok(None);
        }
        if let Some(h) = self.input_hasher.as_mut() {
            h.write(buf.as_bytes());
        }
        let key = parse_timestamp(&buf).unwrap_or(self.last_key);
        self.last_key = key;
        Ok(Some((key, buf)))
    }
}

#[derive(Eq, PartialEq)]
struct HeapEntry {
    key: TimestampKey,
    reader_idx: usize,
    line: String,
}

impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key
            .cmp(&other.key)
            .then(self.reader_idx.cmp(&other.reader_idx))
    }
}
impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

pub fn run(input_dir: &Path, output_dir: Option<&Path>, level: i32, check: bool) -> io::Result<()> {
    let inputs = discover_inputs(input_dir)?;
    if inputs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "no {FILE_PREFIX}<date>-<node>{FILE_SUFFIX} files in {}",
                input_dir.display()
            ),
        ));
    }
    if output_dir.is_none() && !check {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "merge: --output-dir is required unless --check is given",
        ));
    }

    // Group inputs by their filename `<date>` so each day merges into its own
    // output file. `BTreeMap` keeps the days — and thus which file we write
    // when — deterministic across runs.
    let mut by_date: BTreeMap<String, Vec<MergeInput>> = BTreeMap::new();
    for input in inputs {
        by_date.entry(input.date.clone()).or_default().push(input);
    }

    if let Some(dir) = output_dir {
        std::fs::create_dir_all(dir)?;
    }
    if check {
        let scope = if output_dir.is_some() {
            "verifying merge → split round-trip (writing output too)"
        } else {
            "verifying merge → split round-trip (no files written)"
        };
        println!("merge --check: {scope}");
    }

    let mut total = 0usize;
    let mut ok_count = 0usize;
    for (date, group) in by_date {
        let output = output_dir.map(|dir| dir.join(format!("{FILE_PREFIX}{date}.zst")));
        for (node, lines, ok) in merge_day(group, output.as_deref(), level, check)? {
            total += 1;
            if ok {
                ok_count += 1;
            }
            let status = if ok { "OK  " } else { "FAIL" };
            println!("  {date}  {node:<14} {status}  {lines} lines");
        }
    }

    if check {
        println!("check: {ok_count}/{total} files round-trip identical");
        if ok_count != total {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} file(s) failed the round-trip check", total - ok_count),
            ));
        }
    }
    Ok(())
}

/// Streaming k-way merge of one day's inputs. Writes the interleaved,
/// node-prefixed stream to `output` as zstd when `output` is `Some`, and/or —
/// when `check` is set — verifies the round-trip by comparing, per node, a hash
/// of the ingested content against a hash of what the real `split` parser
/// reconstructs from the merged stream. Both can run in the same pass.
///
/// Returns `(node, line_count, ok)` per input file (sorted by node) when
/// `check` is set, otherwise an empty vec.
fn merge_day(
    inputs: Vec<MergeInput>,
    output: Option<&Path>,
    level: i32,
    check: bool,
) -> io::Result<Vec<(String, u64, bool)>> {
    let mut readers: Vec<Reader> = inputs
        .into_iter()
        .map(|i| Reader::open(i, check))
        .collect::<io::Result<_>>()?;

    // Per-node hash of the split-reconstructed content, plus a line count.
    // Pre-seed every node so an empty input file (which emits nothing) still
    // compares as the hash of empty content rather than a missing entry.
    let mut roundtrip: HashMap<String, (DefaultHasher, u64)> = HashMap::new();
    if check {
        for r in &readers {
            roundtrip
                .entry(r.node.clone())
                .or_insert_with(|| (DefaultHasher::new(), 0));
        }
    }

    let mut heap: BinaryHeap<Reverse<HeapEntry>> = BinaryHeap::new();
    for (idx, r) in readers.iter_mut().enumerate() {
        if let Some((key, line)) = r.next_line()? {
            heap.push(Reverse(HeapEntry {
                key,
                reader_idx: idx,
                line,
            }));
        }
    }

    let mut encoder = match output {
        Some(path) => Some(zstd::Encoder::new(File::create(path)?, level)?),
        None => None,
    };

    let mut merged = String::new();
    let mut line_no = 0u64;
    while let Some(Reverse(entry)) = heap.pop() {
        line_no += 1;
        {
            let node = &readers[entry.reader_idx].node;
            if let Some(enc) = encoder.as_mut() {
                enc.write_all(node.as_bytes())?;
                enc.write_all(b" ")?;
                enc.write_all(entry.line.as_bytes())?;
            }
            if check {
                // Reconstruct exactly the bytes `merge` emits for this line.
                merged.clear();
                merged.push_str(node);
                merged.push(' ');
                merged.push_str(&entry.line);
            }
        }
        if check {
            // Route it back through the real `split` parser — the same code
            // path the `split` subcommand uses — and hash what it recovers.
            let (node, rest) = split::split_prefix(&merged, line_no)?;
            let (hasher, count) = roundtrip.entry(node.to_string()).or_default();
            hasher.write(rest.as_bytes());
            *count += 1;
        }
        if let Some((key, line)) = readers[entry.reader_idx].next_line()? {
            heap.push(Reverse(HeapEntry {
                key,
                reader_idx: entry.reader_idx,
                line,
            }));
        }
    }

    if let Some(enc) = encoder {
        enc.finish()?;
    }

    if !check {
        return Ok(Vec::new());
    }
    let mut results = Vec::with_capacity(readers.len());
    for r in &readers {
        let input_hash = r
            .input_hasher
            .as_ref()
            .expect("check mode enables input hashing")
            .finish();
        let (hasher, count) = roundtrip.get(&r.node).expect("node pre-seeded above");
        let ok = hasher.finish() == input_hash;
        results.push((r.node.clone(), *count, ok));
    }
    results.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(results)
}
