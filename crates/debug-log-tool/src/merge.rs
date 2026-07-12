//! Interleave gzipped per-node `debug.log` files into one timestamp-ordered
//! zstd log *per day*, with each line prefixed by the node name parsed from
//! its source filename. Inputs are grouped by the `<date>` segment of their
//! filename, so a directory holding several days' logs yields one
//! `debug.log-<date>.zst` per day.
//!
//! Each day's merge is a streaming k-way merge. Every input file gets its own
//! reader thread that decompresses, splits into lines, parses timestamps, and
//! (in `--check` mode) hashes, feeding batches of lines over a bounded channel
//! to the merge thread. The bounded channels keep memory use proportional to
//! the number of inputs rather than the total log size, and the heap merge
//! itself stays on one thread so the output order is identical to a
//! single-threaded merge.

use std::cmp::Reverse;
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BinaryHeap, HashMap};
use std::fs::File;
use std::hash::Hasher;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;

use flate2::read::MultiGzDecoder;

use crate::split;
use crate::timestamp::{TimestampKey, parse_timestamp};

const FILE_PREFIX: &str = "debug.log-";
const FILE_SUFFIX: &str = ".gz";

/// Reader threads ship lines to the merge thread in batches to amortize
/// channel overhead; a batch is flushed at whichever limit is hit first.
const BATCH_LINES: usize = 1024;
const BATCH_BYTES: usize = 256 * 1024;
/// Bound on in-flight batches per reader. Backpressure from this bound is what
/// keeps merge streaming: memory stays O(inputs × batch × bound) no matter how
/// large the logs are.
const CHANNEL_BATCHES: usize = 4;

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

type Batch = Vec<(TimestampKey, String)>;

/// Body of one input's reader thread: decompress, split into lines, parse
/// timestamps, and — in `--check` mode — hash the file's raw decompressed
/// content as read (compared later against the hash of what `split` would
/// reconstruct for this node to prove the round-trip). Line batches go out
/// over the bounded channel; the content hash comes back via the join handle,
/// `None` when not checking so a normal merge pays no hashing cost.
///
/// A clean EOF is signalled by dropping the sender; an I/O error is sent over
/// the channel itself so the merge thread can propagate it.
fn read_lines(path: PathBuf, check: bool, tx: mpsc::SyncSender<io::Result<Batch>>) -> Option<u64> {
    let mut hasher = check.then(DefaultHasher::new);
    if let Err(e) = read_lines_inner(&path, &mut hasher, &tx) {
        // If the merge thread already hung up, nobody is left to care.
        let _ = tx.send(Err(e));
    }
    hasher.map(|h| h.finish())
}

fn read_lines_inner(
    path: &Path,
    hasher: &mut Option<DefaultHasher>,
    tx: &mpsc::SyncSender<io::Result<Batch>>,
) -> io::Result<()> {
    let mut reader = BufReader::new(MultiGzDecoder::new(File::open(path)?));
    // Timestamp of the most recent line we successfully parsed. Lines without
    // their own timestamp (continuation lines, multi-line errors) inherit it
    // so they stay glued to the entry they belong to.
    let mut last_key = TimestampKey::default();
    let mut batch: Batch = Vec::new();
    let mut batch_bytes = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        if let Some(h) = hasher.as_mut() {
            h.write(line.as_bytes());
        }
        let key = parse_timestamp(&line).unwrap_or(last_key);
        last_key = key;
        batch_bytes += line.len();
        batch.push((key, line));
        if batch.len() >= BATCH_LINES || batch_bytes >= BATCH_BYTES {
            if tx.send(Ok(std::mem::take(&mut batch))).is_err() {
                // Merge thread hung up (it hit an error); stop quietly.
                return Ok(());
            }
            batch_bytes = 0;
        }
    }
    if !batch.is_empty() {
        let _ = tx.send(Ok(batch));
    }
    Ok(())
}

/// Merge-thread handle to one input: pulls pre-parsed lines out of the reader
/// thread's channel one batch at a time.
struct ThreadedReader<'scope> {
    node: String,
    rx: mpsc::Receiver<io::Result<Batch>>,
    current: std::vec::IntoIter<(TimestampKey, String)>,
    handle: thread::ScopedJoinHandle<'scope, Option<u64>>,
}

impl ThreadedReader<'_> {
    fn next_line(&mut self) -> io::Result<Option<(TimestampKey, String)>> {
        loop {
            if let Some(entry) = self.current.next() {
                return Ok(Some(entry));
            }
            match self.rx.recv() {
                Ok(Ok(batch)) => self.current = batch.into_iter(),
                Ok(Err(e)) => return Err(e),
                // Sender dropped without sending an error: clean EOF.
                Err(mpsc::RecvError) => return Ok(None),
            }
        }
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

pub fn run(
    input_dir: &Path,
    output_dir: Option<&Path>,
    level: i32,
    jobs: u32,
    check: bool,
) -> io::Result<()> {
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
        for (node, lines, ok) in merge_day(group, output.as_deref(), level, jobs, check)? {
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
/// Each input is decompressed, parsed, and hashed on its own thread; `jobs`
/// sets the zstd compression worker count (1 = single-threaded compression).
///
/// Returns `(node, line_count, ok)` per input file (sorted by node) when
/// `check` is set, otherwise an empty vec.
fn merge_day(
    inputs: Vec<MergeInput>,
    output: Option<&Path>,
    level: i32,
    jobs: u32,
    check: bool,
) -> io::Result<Vec<(String, u64, bool)>> {
    thread::scope(|s| {
        // If we bail out early with `?`, the readers — and with them the channel
        // receivers — are dropped, which makes the reader threads' `send` fail so
        // they exit instead of blocking; the scope then joins them.
        let mut readers: Vec<ThreadedReader<'_>> = Vec::with_capacity(inputs.len());
        for input in inputs {
            let (tx, rx) = mpsc::sync_channel(CHANNEL_BATCHES);
            let handle = s.spawn(move || read_lines(input.path, check, tx));
            readers.push(ThreadedReader {
                node: input.node,
                rx,
                current: Vec::new().into_iter(),
                handle,
            });
        }

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
            Some(path) => {
                let mut enc = zstd::Encoder::new(File::create(path)?, level)?;
                if jobs > 1 {
                    enc.multithread(jobs)?;
                }
                Some(enc)
            }
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
                // Every node is pre-seeded above; a lookup miss means `split`
                // recovered a node name `merge` never wrote, which no hash
                // comparison could make good, so it fails the check outright.
                let (node, rest) = split::split_prefix(&merged, line_no)?;
                let Some((hasher, count)) = roundtrip.get_mut(node) else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("line {line_no}: split recovered unknown node `{node}`"),
                    ));
                };
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
        // The heap drained, so every reader thread has hit EOF and dropped its
        // sender; joining here doesn't block.
        let mut results = Vec::with_capacity(readers.len());
        for r in readers {
            let input_hash = r
                .handle
                .join()
                .map_err(|_| io::Error::other(format!("reader thread for `{}` panicked", r.node)))?
                .expect("check mode enables input hashing");
            let (hasher, count) = roundtrip.get(&r.node).expect("node pre-seeded above");
            let ok = hasher.finish() == input_hash;
            results.push((r.node, *count, ok));
        }
        results.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(results)
    })
}
