//! `templates` subcommand: drive `drnlog::Analyzer` over a debug.log
//! input file, then render a per-category template/count report.
//!
//! Compression of the input is detected by sniffing the first four bytes:
//! zstd (28 B5 2F FD), gzip (1F 8B), or plain. Whether the lines carry the
//! `<node> ` prefix `merge` adds is decided from the first timestamped line,
//! so a merged log and a per-node one both work in any of the three.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, Write};
use std::path::{Path, PathBuf};

use crate::timestamp::parse_timestamp;
use drnlog::{Analyzer, Cluster, Slot};
use flate2::read::MultiGzDecoder;

pub struct TemplatesOpts {
    pub depth: usize,
    pub threshold: f64,
    pub min_count: u64,
    pub categories: Vec<String>,
    pub top: Option<usize>,
    pub load_state: Option<PathBuf>,
    pub save_state: Option<PathBuf>,
    pub json: bool,
    /// Keep one analyzer per `<node>` (merged input only) and write one
    /// `<dir>/<node>.{json,txt}` report per node.
    pub per_node: Option<PathBuf>,
}

pub fn run(input: Option<&Path>, opts: TemplatesOpts) -> io::Result<()> {
    if let Some(dir) = &opts.per_node {
        return run_per_node(input, dir, &opts);
    }
    let mut analyzer = match &opts.load_state {
        Some(path) => {
            let f = File::open(path)?;
            Analyzer::load(BufReader::new(f))?
        }
        None => Analyzer::new(opts.depth, opts.threshold),
    };

    if let Some(path) = input {
        let mut reader = open_input(path)?;
        // `Some(true)` once a line shows the `<node> ` prefix, `Some(false)`
        // once one shows it absent.
        let mut prefixed: Option<bool> = None;
        let mut buf: Vec<u8> = Vec::new();
        loop {
            buf.clear();
            if reader.read_until(b'\n', &mut buf)? == 0 {
                break;
            }
            // Read as bytes and decode leniently: one stray byte in a log
            // shouldn't abort the whole run.
            let decoded = String::from_utf8_lossy(&buf);
            let line = decoded.trim_end_matches(['\r', '\n']);
            if prefixed.is_none() {
                prefixed = sniff_node_prefix(line);
            }
            let content = match prefixed {
                Some(true) => strip_node_prefix(line),
                _ => line,
            };
            analyzer.ingest_line(content);
        }
    }

    if let Some(path) = &opts.save_state {
        analyzer.save_atomic(path)?;
    }

    let stdout = io::stdout();
    let mut w = stdout.lock();
    render(&analyzer, &opts, &mut w)
}

/// Cluster each node of a merged log on its own, one report per node.
fn run_per_node(input: Option<&Path>, dir: &Path, opts: &TemplatesOpts) -> io::Result<()> {
    let bad = |m: String| io::Error::new(io::ErrorKind::InvalidInput, m);
    if opts.load_state.is_some() || opts.save_state.is_some() {
        return Err(bad(
            "--per-node can't be combined with --load-state/--save-state".into(),
        ));
    }
    let path = input.ok_or_else(|| bad("--per-node needs an INPUT".into()))?;
    let mut reader = open_input(path)?;

    // Every line of a merged log carries the `<node> ` prefix; refuse a
    // per-node log once its first timestamped line shows it has none.
    let mut prefixed: Option<bool> = None;
    let mut analyzers: BTreeMap<String, Analyzer> = BTreeMap::new();
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        let decoded = String::from_utf8_lossy(&buf);
        let line = decoded.trim_end_matches(['\r', '\n']);
        if prefixed.is_none() {
            prefixed = sniff_node_prefix(line);
            if prefixed == Some(false) {
                return Err(bad(
                    "--per-node needs a merged log (lines prefixed with <node>)".into(),
                ));
            }
        }
        let (node, content) = match line.split_once(' ') {
            // An empty node name still means the prefix was there.
            Some(("", rest)) => (UNKNOWN_NODE, rest),
            Some((node, rest)) => (node, rest),
            None => (UNKNOWN_NODE, line),
        };
        if !is_plain_filename(node) {
            return Err(bad(format!("node name is not a filename: {node}")));
        }
        // Allocating the key only for a node not seen yet keeps the hot path
        // free of a per-line allocation.
        if !analyzers.contains_key(node) {
            analyzers.insert(node.to_owned(), Analyzer::new(opts.depth, opts.threshold));
        }
        let a = analyzers.get_mut(node).expect("just inserted");
        a.ingest_line(content);
    }

    std::fs::create_dir_all(dir)?;
    let ext = if opts.json { "json" } else { "txt" };
    for (node, a) in &analyzers {
        let out = dir.join(format!("{node}.{ext}"));
        let tmp = dir.join(format!(".{node}.{ext}.tmp"));
        let mut w = io::BufWriter::new(File::create(&tmp)?);
        render(a, opts, &mut w)?;
        w.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        std::fs::rename(&tmp, &out)?;
    }
    Ok(())
}

/// Where a line with no `<node> ` prefix at all is counted.
const UNKNOWN_NODE: &str = "_unknown";

/// A node name becomes a filename, so it must not reach outside the
/// directory or collide with the temp files written there.
fn is_plain_filename(node: &str) -> bool {
    !node.starts_with('.') && !node.contains('/')
}

/// Open `path`, transparently decompressing zstd or gzip.
fn open_input(path: &Path) -> io::Result<Box<dyn BufRead>> {
    let mut f = File::open(path)?;
    let mut magic = [0u8; 4];
    let n = f.read(&mut magic)?;
    f.seek(io::SeekFrom::Start(0))?;
    let prefix = &magic[..n];

    if prefix.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]) {
        Ok(Box::new(BufReader::new(zstd::Decoder::new(f)?)))
    } else if prefix.starts_with(&[0x1F, 0x8B]) {
        Ok(Box::new(BufReader::new(MultiGzDecoder::new(f))))
    } else {
        Ok(Box::new(BufReader::new(f)))
    }
}

/// Does this line carry a `<node> ` prefix? `None` when the line has no
/// timestamp either way and so says nothing about the file.
fn sniff_node_prefix(line: &str) -> Option<bool> {
    if parse_timestamp(line.as_bytes()).is_some() {
        return Some(false);
    }
    let (node, rest) = line.split_once(' ')?;
    if node.is_empty() {
        return None;
    }
    parse_timestamp(rest.as_bytes()).is_some().then_some(true)
}

/// Strip the leading `<node> ` prefix produced by `merge`. If the line
/// has no space, return it as-is.
fn strip_node_prefix(line: &str) -> &str {
    match line.split_once(' ') {
        Some((_node, rest)) => rest,
        None => line,
    }
}

fn render<W: Write>(a: &Analyzer, opts: &TemplatesOpts, w: &mut W) -> io::Result<()> {
    let want_cat = |cat: Option<&str>| -> bool {
        if opts.categories.is_empty() {
            return true;
        }
        match cat {
            Some(c) => opts.categories.iter().any(|x| x == c),
            None => false,
        }
    };

    // BTreeMap keys give deterministic category ordering.
    let mut by_cat: BTreeMap<Option<String>, Vec<&Cluster>> = BTreeMap::new();
    for (cat, c) in a.templates() {
        if c.count < opts.min_count {
            continue;
        }
        if !want_cat(cat) {
            continue;
        }
        by_cat.entry(cat.map(str::to_owned)).or_default().push(c);
    }
    for v in by_cat.values_mut() {
        v.sort_by(|a, b| b.count.cmp(&a.count).then(a.id.cmp(&b.id)));
        if let Some(top) = opts.top {
            v.truncate(top);
        }
    }

    if opts.json {
        for (cat, clusters) in &by_cat {
            for c in clusters {
                write_json_cluster(w, cat.as_deref(), c)?;
            }
        }
    } else {
        let mut first = true;
        for (cat, clusters) in &by_cat {
            if !first {
                writeln!(w)?;
            }
            first = false;
            let label = cat.as_deref().unwrap_or("(no category)");
            writeln!(w, "=== {label} ===")?;
            for c in clusters {
                writeln!(w, "{:>6} {}", c.count, render_template(&c.template))?;
            }
        }
    }
    Ok(())
}

fn render_template(template: &[Slot]) -> String {
    template
        .iter()
        .map(|s| match s {
            Slot::Text(s) => s.clone(),
            Slot::Typed(k) => format!("<{}>", k.label()),
            Slot::Star => "<*>".to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn write_json_cluster<W: Write>(w: &mut W, cat: Option<&str>, c: &Cluster) -> io::Result<()> {
    write!(w, "{{\"category\":")?;
    match cat {
        Some(s) => json_string(w, s)?,
        None => write!(w, "null")?,
    }
    write!(w, ",\"count\":{}", c.count)?;
    write!(w, ",\"first_seen\":")?;
    match &c.first_seen {
        Some(s) => json_string(w, s)?,
        None => write!(w, "null")?,
    }
    write!(w, ",\"template\":")?;
    json_string(w, &render_template(&c.template))?;
    writeln!(w, "}}")
}

fn json_string<W: Write>(w: &mut W, s: &str) -> io::Result<()> {
    w.write_all(b"\"")?;
    for c in s.chars() {
        match c {
            '"' => w.write_all(b"\\\"")?,
            '\\' => w.write_all(b"\\\\")?,
            '\n' => w.write_all(b"\\n")?,
            '\r' => w.write_all(b"\\r")?,
            '\t' => w.write_all(b"\\t")?,
            c if (c as u32) < 0x20 => write!(w, "\\u{:04x}", c as u32)?,
            c => {
                let mut buf = [0u8; 4];
                w.write_all(c.encode_utf8(&mut buf).as_bytes())?;
            }
        }
    }
    w.write_all(b"\"")
}
