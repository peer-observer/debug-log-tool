//! Reverse of `merge`: read a `debug.log-<date>.zst` merged log and write one
//! gzipped `debug.log-<date>-<node>.gz` per node encountered.
//!
//! The `<date>` comes from the input filename, not from the timestamps inside
//! the file. Since `merge` groups a single day into each `debug.log-<date>.zst`
//! and prefixes every line — including timestamp-less continuation lines — with
//! its `<node> `, splitting purely on the node prefix reconstructs each
//! original `debug.log-<date>-<node>.gz` byte-for-byte.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;

use flate2::Compression;
use flate2::write::GzEncoder;

const FILE_PREFIX: &str = "debug.log-";
const INPUT_SUFFIX: &str = ".zst";

pub fn run(input: &Path, output_dir: &Path) -> io::Result<()> {
    let date = date_from_filename(input)?;
    std::fs::create_dir_all(output_dir)?;
    let f = File::open(input)?;
    let dec = zstd::Decoder::new(f)?;
    let mut reader = BufReader::new(dec);

    let mut writers: HashMap<String, GzEncoder<File>> = HashMap::new();

    let mut buf = String::new();
    let mut line_no: u64 = 0;
    loop {
        buf.clear();
        if reader.read_line(&mut buf)? == 0 {
            break;
        }
        line_no += 1;

        let (node, rest) = split_prefix(&buf, line_no)?;

        if !writers.contains_key(node) {
            let path = output_dir.join(format!("{FILE_PREFIX}{date}-{node}.gz"));
            let file = File::create(&path)?;
            writers.insert(
                node.to_string(),
                GzEncoder::new(file, Compression::default()),
            );
        }
        writers
            .get_mut(node)
            .expect("just inserted")
            .write_all(rest.as_bytes())?;
    }

    for (_, w) in writers.drain() {
        w.finish()?;
    }
    Ok(())
}

/// Split a merged line `<node> <rest>` into its node name and remainder,
/// where `<rest>` is the original per-node log line. This is the exact inverse
/// of the `<node> ` prefix `merge` prepends, and is shared with `merge --check`
/// so the round-trip self-check exercises the same parsing the real split does.
pub(crate) fn split_prefix(line: &str, line_no: u64) -> io::Result<(&str, &str)> {
    let Some(sp) = line.find(' ') else {
        return Err(invalid(format!("line {line_no}: missing `<node> ` prefix")));
    };
    Ok((&line[..sp], &line[sp + 1..]))
}

/// Recover the `<date>` from a `debug.log-<date>.zst` input filename.
fn date_from_filename(input: &Path) -> io::Result<String> {
    let name = input
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| invalid(format!("input path {} has no filename", input.display())))?;
    let date = name
        .strip_prefix(FILE_PREFIX)
        .and_then(|s| s.strip_suffix(INPUT_SUFFIX))
        .filter(|d| !d.is_empty())
        .ok_or_else(|| {
            invalid(format!(
                "input filename `{name}` is not of the form {FILE_PREFIX}<date>{INPUT_SUFFIX}"
            ))
        })?;
    Ok(date.to_string())
}

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}
