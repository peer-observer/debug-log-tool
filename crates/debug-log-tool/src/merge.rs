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
use std::collections::{BTreeMap, BinaryHeap};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use flate2::read::MultiGzDecoder;

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
}

impl Reader {
    fn open(input: MergeInput) -> io::Result<Self> {
        let f = File::open(&input.path)?;
        Ok(Self {
            node: input.node,
            inner: BufReader::new(MultiGzDecoder::new(f)),
            last_key: TimestampKey::default(),
        })
    }

    fn next_line(&mut self) -> io::Result<Option<(TimestampKey, String)>> {
        let mut buf = String::new();
        if self.inner.read_line(&mut buf)? == 0 {
            return Ok(None);
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

pub fn run(input_dir: &Path, output_dir: &Path, level: i32) -> io::Result<()> {
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

    std::fs::create_dir_all(output_dir)?;

    // Group inputs by their filename `<date>` so each day merges into its own
    // output file. `BTreeMap` keeps the days — and thus which file we write
    // when — deterministic across runs.
    let mut by_date: BTreeMap<String, Vec<MergeInput>> = BTreeMap::new();
    for input in inputs {
        by_date.entry(input.date.clone()).or_default().push(input);
    }

    for (date, group) in by_date {
        let output = output_dir.join(format!("{FILE_PREFIX}{date}.zst"));
        merge_day(group, &output, level)?;
    }

    Ok(())
}

/// Streaming k-way merge of one day's inputs into a single zstd file.
fn merge_day(inputs: Vec<MergeInput>, output: &Path, level: i32) -> io::Result<()> {
    let mut readers: Vec<Reader> = inputs
        .into_iter()
        .map(Reader::open)
        .collect::<io::Result<_>>()?;

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

    let out = File::create(output)?;
    let mut enc = zstd::Encoder::new(out, level)?;

    while let Some(Reverse(entry)) = heap.pop() {
        let node = readers[entry.reader_idx].node.as_bytes();
        enc.write_all(node)?;
        enc.write_all(b" ")?;
        enc.write_all(entry.line.as_bytes())?;
        if let Some((key, line)) = readers[entry.reader_idx].next_line()? {
            heap.push(Reverse(HeapEntry {
                key,
                reader_idx: entry.reader_idx,
                line,
            }));
        }
    }

    enc.finish()?;
    Ok(())
}
