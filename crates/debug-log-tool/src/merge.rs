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
use std::ffi::OsString;
use std::fs::File;
use std::hash::Hasher;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use flate2::read::MultiGzDecoder;

use crate::split;
use crate::timestamp::{TimestampKey, parse_timestamp};

/// Name the file an IO error came from: a merge reads many at once.
fn at_path(path: &Path, e: io::Error) -> io::Error {
    io::Error::new(e.kind(), format!("{}: {e}", path.display()))
}

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
        // The merged stream separates node from line with a space, so a node
        // name holding one could never be split back off.
        if node.contains(' ') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("node name in `{fname}` contains a space"),
            ));
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
    path: PathBuf,
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
        let f = File::open(&input.path).map_err(|e| at_path(&input.path, e))?;
        Ok(Self {
            node: input.node,
            path: input.path,
            inner: BufReader::new(MultiGzDecoder::new(f)),
            last_key: TimestampKey::default(),
            input_hasher: check.then(DefaultHasher::new),
        })
    }

    fn next_line(&mut self) -> io::Result<Option<(TimestampKey, Vec<u8>)>> {
        // Bytes, not text: a debug.log can hold anything a peer sent, and a
        // truncated gzip can cut a character in half.
        let mut buf = Vec::new();
        let read = self
            .inner
            .read_until(b'\n', &mut buf)
            .map_err(|e| at_path(&self.path, e))?;
        if read == 0 {
            return Ok(None);
        }
        // Without a newline here, the next line written continues this one.
        if buf.last() != Some(&b'\n') {
            buf.push(b'\n');
        }
        if let Some(h) = self.input_hasher.as_mut() {
            h.write(&buf);
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
    line: Vec<u8>,
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
            "writing each day, then re-reading it and splitting it back"
        } else {
            "splitting the merged stream back (nothing written)"
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

/// Streaming k-way merge of one day's inputs, writing the interleaved,
/// node-prefixed stream to `output` as zstd when `output` is `Some`.
///
/// Returns `(node, line_count, ok)` per input file (sorted by node) when
/// `check` is set, otherwise an empty vec.
///
/// Writes to a temp file and renames on success, so a run that fails part-way
/// — or whose `--check` finds a mismatch — leaves neither a truncated merged
/// log nor a damaged earlier one. With `--check` the temp file is decompressed
/// and split again before the rename, so what gets published is the stream
/// that was verified, not just the one that was meant to be written.
fn merge_day(
    inputs: Vec<MergeInput>,
    output: Option<&Path>,
    level: i32,
    check: bool,
) -> io::Result<Vec<(String, u64, bool)>> {
    let tmp = output.map(|path| {
        let mut name = OsString::from(path);
        // Per-process name so two merges into one directory can't scribble
        // over each other's half-written file.
        name.push(format!(".{}.tmp", std::process::id()));
        PathBuf::from(name)
    });
    let result = merge_and_verify(inputs, tmp.as_deref(), output, level, check);
    if let (Err(_), Some(tmp)) = (&result, &tmp) {
        let _ = std::fs::remove_file(tmp);
    }
    result
}

fn merge_and_verify(
    inputs: Vec<MergeInput>,
    tmp: Option<&Path>,
    output: Option<&Path>,
    level: i32,
    check: bool,
) -> io::Result<Vec<(String, u64, bool)>> {
    let Some(checkable) = merge_into(inputs, tmp, level, check)? else {
        if let (Some(tmp), Some(output)) = (tmp, output) {
            std::fs::rename(tmp, output)?;
        }
        return Ok(Vec::new());
    };

    let written = match (tmp, checkable.stream) {
        // Re-read what was actually written.
        (Some(tmp), _) => read_back(tmp, &checkable.inputs)?,
        // Nothing was written, so judge the stream as it was assembled.
        (None, Some(stream)) => stream.finish(),
        (None, None) => unreachable!("check without an output file digests the stream"),
    };
    let results = compare(&checkable.inputs, &written);

    if let (Some(tmp), Some(output)) = (tmp, output) {
        if results.iter().all(|(_, _, ok)| *ok) {
            std::fs::rename(tmp, output)?;
        } else {
            println!(
                "  check failed; merged log left unpublished at {}",
                tmp.display()
            );
        }
    }
    Ok(results)
}

/// Decompress a merged log and digest it per node, the way `split` reads it.
fn read_back(path: &Path, inputs: &[(String, u64)]) -> io::Result<HashMap<Vec<u8>, (u64, u64)>> {
    let mut digests = Digests::default();
    for (node, _) in inputs {
        digests.seed(node);
    }
    let f = File::open(path).map_err(|e| at_path(path, e))?;
    let mut reader = BufReader::new(zstd::Decoder::new(f).map_err(|e| at_path(path, e))?);
    let mut line = Vec::new();
    let mut line_no = 0u64;
    loop {
        line.clear();
        if reader
            .read_until(b'\n', &mut line)
            .map_err(|e| at_path(path, e))?
            == 0
        {
            break;
        }
        line_no += 1;
        digests.feed(&line, line_no)?;
    }
    Ok(digests.finish())
}

/// Per input file: did the merged log give its content back unchanged?
fn compare(
    inputs: &[(String, u64)],
    written: &HashMap<Vec<u8>, (u64, u64)>,
) -> Vec<(String, u64, bool)> {
    let mut results: Vec<(String, u64, bool)> = inputs
        .iter()
        .map(|(node, input_hash)| {
            let (hash, lines) = written
                .get(node.as_bytes())
                .copied()
                .unwrap_or((DefaultHasher::new().finish(), 0));
            (node.clone(), lines, hash == *input_hash)
        })
        .collect();
    results.sort_by(|a, b| a.0.cmp(&b.0));
    results
}

/// Running per-node digest of a merged stream, fed one `<node> <line>` at a
/// time through the real `split` parser.
#[derive(Default)]
struct Digests(HashMap<Vec<u8>, (DefaultHasher, u64)>);

impl Digests {
    /// Start a node at the digest of empty content, so an input that
    /// contributes no lines compares as empty rather than as missing.
    fn seed(&mut self, node: &str) {
        self.0
            .entry(node.as_bytes().to_vec())
            .or_insert_with(|| (DefaultHasher::new(), 0));
    }

    fn feed(&mut self, line: &[u8], line_no: u64) -> io::Result<()> {
        let (node, rest) = split::split_prefix(line, line_no)?;
        let (hasher, count) = self.0.entry(node.to_vec()).or_default();
        hasher.write(rest);
        *count += 1;
        Ok(())
    }

    /// `(hash, line_count)` per node.
    fn finish(self) -> HashMap<Vec<u8>, (u64, u64)> {
        self.0
            .into_iter()
            .map(|(node, (hasher, count))| (node, (hasher.finish(), count)))
            .collect()
    }
}

/// What one merge pass produced for `--check` to judge: the digest of each
/// input's content, and — only when nothing was written and so nothing can be
/// re-read — the digest of the stream as it was assembled.
struct Checkable {
    inputs: Vec<(String, u64)>,
    stream: Option<Digests>,
}

fn merge_into(
    inputs: Vec<MergeInput>,
    output: Option<&Path>,
    level: i32,
    check: bool,
) -> io::Result<Option<Checkable>> {
    let mut readers: Vec<Reader> = inputs
        .into_iter()
        .map(|i| Reader::open(i, check))
        .collect::<io::Result<_>>()?;

    // With an output file the check re-reads it afterwards; with no file to
    // re-read, digest the stream as we build it.
    let mut stream = (check && output.is_none()).then(|| {
        let mut d = Digests::default();
        for r in &readers {
            d.seed(&r.node);
        }
        d
    });

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

    let mut merged: Vec<u8> = Vec::new();
    let mut line_no = 0u64;
    while let Some(Reverse(entry)) = heap.pop() {
        line_no += 1;
        {
            let node = &readers[entry.reader_idx].node;
            if let Some(enc) = encoder.as_mut() {
                enc.write_all(node.as_bytes())?;
                enc.write_all(b" ")?;
                enc.write_all(&entry.line)?;
            }
            if stream.is_some() {
                // Exactly the bytes `merge` would have written for this line.
                merged.clear();
                merged.extend_from_slice(node.as_bytes());
                merged.push(b' ');
                merged.extend_from_slice(&entry.line);
            }
        }
        if let Some(stream) = stream.as_mut() {
            stream.feed(&merged, line_no)?;
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
        return Ok(None);
    }
    let inputs = readers
        .iter()
        .map(|r| {
            let hash = r
                .input_hasher
                .as_ref()
                .expect("check mode enables input hashing")
                .finish();
            (r.node.clone(), hash)
        })
        .collect();
    Ok(Some(Checkable { inputs, stream }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("debug-log-tool-merge-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_zst(path: &Path, content: &[u8]) {
        let f = File::create(path).unwrap();
        let mut enc = zstd::Encoder::new(f, 1).unwrap();
        enc.write_all(content).unwrap();
        enc.finish().unwrap();
    }

    fn hash_of(content: &[u8]) -> u64 {
        let mut h = DefaultHasher::new();
        h.write(content);
        h.finish()
    }

    /// `read_back` has to recover, from the compressed log alone, exactly the
    /// content each input contributed.
    #[test]
    fn read_back_recovers_each_node() {
        let dir = test_dir("read-back");
        let merged = dir.join("debug.log-20260515.zst");
        write_zst(
            &merged,
            b"aa 2026-05-15T00:00:01 one\nbb 2026-05-15T00:00:02 two\naa 2026-05-15T00:00:03 three\n",
        );

        let inputs = vec![
            (
                "aa".to_string(),
                hash_of(b"2026-05-15T00:00:01 one\n2026-05-15T00:00:03 three\n"),
            ),
            ("bb".to_string(), hash_of(b"2026-05-15T00:00:02 two\n")),
        ];
        let written = read_back(&merged, &inputs).unwrap();
        assert_eq!(
            compare(&inputs, &written),
            vec![("aa".to_string(), 2, true), ("bb".to_string(), 1, true)],
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A log that doesn't give an input's bytes back has to come out as FAIL —
    /// one byte dropped from one line is enough.
    #[test]
    fn compare_catches_a_log_that_lost_a_byte() {
        let dir = test_dir("lost-byte");
        let merged = dir.join("debug.log-20260515.zst");
        write_zst(
            &merged,
            b"aa 2026-05-15T00:00:01 one\nbb 2026-05-15T00:00:02 tw\n",
        );

        let inputs = vec![
            ("aa".to_string(), hash_of(b"2026-05-15T00:00:01 one\n")),
            ("bb".to_string(), hash_of(b"2026-05-15T00:00:02 two\n")),
        ];
        let written = read_back(&merged, &inputs).unwrap();
        assert_eq!(
            compare(&inputs, &written),
            vec![("aa".to_string(), 1, true), ("bb".to_string(), 1, false)],
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A missing node reads as empty content: right for an input that had no
    /// lines, a failure for one that did.
    #[test]
    fn compare_treats_a_missing_node_as_empty() {
        let dir = test_dir("missing-node");
        let merged = dir.join("debug.log-20260515.zst");
        write_zst(&merged, b"aa 2026-05-15T00:00:01 one\n");

        let inputs = vec![
            ("aa".to_string(), hash_of(b"2026-05-15T00:00:01 one\n")),
            ("empty".to_string(), hash_of(b"")),
            (
                "dropped".to_string(),
                hash_of(b"2026-05-15T00:00:02 gone\n"),
            ),
        ];
        let written = read_back(&merged, &inputs).unwrap();
        assert_eq!(
            compare(&inputs, &written),
            vec![
                ("aa".to_string(), 1, true),
                ("dropped".to_string(), 0, false),
                ("empty".to_string(), 0, true),
            ],
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A line in the merged log with no `<node> ` prefix is a hard error, not
    /// a silently mis-attributed line.
    #[test]
    fn read_back_rejects_a_line_with_no_prefix() {
        let dir = test_dir("no-prefix");
        let merged = dir.join("debug.log-20260515.zst");
        write_zst(&merged, b"aa 2026-05-15T00:00:01 one\nno-prefix\n");

        let inputs = vec![("aa".to_string(), hash_of(b"2026-05-15T00:00:01 one\n"))];
        let err = read_back(&merged, &inputs).unwrap_err();
        assert!(err.to_string().contains("line 2"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
