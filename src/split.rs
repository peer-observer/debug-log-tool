//! Reverse of `merge`: read a zstd-compressed merged log and write one
//! gzipped `debug.log-<date>-<node>.gz` per `(node, date)` pair encountered.
//!
//! The date for each line comes from the line's own timestamp. Continuation
//! lines without a timestamp inherit the previous date seen for the same
//! node, so multi-line log entries stay glued together in the right output.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;

use flate2::Compression;
use flate2::write::GzEncoder;

use crate::timestamp::parse_timestamp;

pub fn run(input: &Path, output_dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(output_dir)?;
    let f = File::open(input)?;
    let dec = zstd::Decoder::new(f)?;
    let mut reader = BufReader::new(dec);

    let mut writers: HashMap<(String, String), GzEncoder<File>> = HashMap::new();
    let mut last_date: HashMap<String, String> = HashMap::new();

    let mut buf = String::new();
    let mut line_no: u64 = 0;
    loop {
        buf.clear();
        if reader.read_line(&mut buf)? == 0 {
            break;
        }
        line_no += 1;

        let Some(sp) = buf.find(' ') else {
            return Err(invalid(format!("line {line_no}: missing `<node> ` prefix")));
        };
        let node = &buf[..sp];
        let rest = &buf[sp + 1..];

        let date = match parse_timestamp(rest) {
            Some(k) => format!("{:04}{:02}{:02}", k.year, k.month, k.day),
            None => last_date.get(node).cloned().ok_or_else(|| {
                invalid(format!(
                    "line {line_no}: no timestamp and no prior line for node `{node}`"
                ))
            })?,
        };
        last_date.insert(node.to_string(), date.clone());

        let key = (node.to_string(), date.clone());
        if !writers.contains_key(&key) {
            let path = output_dir.join(format!("debug.log-{date}-{node}.gz"));
            let file = File::create(&path)?;
            writers.insert(key.clone(), GzEncoder::new(file, Compression::default()));
        }
        writers
            .get_mut(&key)
            .expect("just inserted")
            .write_all(rest.as_bytes())?;
    }

    for (_, w) in writers.drain() {
        w.finish()?;
    }
    Ok(())
}

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}
