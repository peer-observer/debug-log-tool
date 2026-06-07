//! High-level API: feed raw bitcoind log lines in, get per-category
//! template clusters out. Includes JSONL save/load with atomic write.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::Path;

use crate::drain::{Cluster, Drain, Slot};
use crate::line;
use crate::tokenizer::{TokenKind, tokenize};

/// Top-level entry point. Maintains one [`Drain`] per category and
/// dispatches each ingested line accordingly.
#[derive(Debug, Clone)]
pub struct Analyzer {
    depth: usize,
    threshold: f64,
    by_category: HashMap<Option<String>, Drain>,
}

impl Analyzer {
    pub fn new(depth: usize, threshold: f64) -> Self {
        Self {
            depth,
            threshold,
            by_category: HashMap::new(),
        }
    }

    pub fn depth(&self) -> usize {
        self.depth
    }
    pub fn threshold(&self) -> f64 {
        self.threshold
    }

    /// Parse, tokenize, and cluster one raw log line. Lines without a
    /// recognised timestamp prefix are still ingested (they go into the
    /// `None` category as continuation/garbage).
    pub fn ingest_line(&mut self, raw: &str) {
        let parsed = line::parse(raw);
        let tokens = tokenize(parsed.content);
        let category = parsed.category.map(str::to_owned);
        let drain = self
            .by_category
            .entry(category)
            .or_insert_with(|| Drain::new(self.depth, self.threshold));
        drain.add_tokens(&tokens, parsed.timestamp);
    }

    /// Iterate `(category, cluster)` pairs across every per-category tree.
    pub fn templates(&self) -> impl Iterator<Item = (Option<&str>, &Cluster)> {
        self.by_category.iter().flat_map(|(cat, drain)| {
            let cat = cat.as_deref();
            drain.clusters().map(move |c| (cat, c))
        })
    }

    /// Serialize to JSONL — one `_meta` header line, then one cluster per
    /// line. Use [`save_atomic`](Self::save_atomic) to write a file safely
    /// (this method is for callers that already own a writer).
    pub fn save<W: Write>(&self, mut w: W) -> io::Result<()> {
        writeln!(
            w,
            r#"{{"_meta":{{"version":1,"depth":{},"threshold":{}}}}}"#,
            self.depth,
            json_float(self.threshold),
        )?;
        for (cat, c) in self.templates() {
            write_cluster_line(&mut w, cat, c)?;
        }
        Ok(())
    }

    /// Atomic save: writes to `path.tmp` and renames over `path` on
    /// success. Avoids leaving a half-written state file if the process
    /// dies mid-write.
    pub fn save_atomic(&self, path: &Path) -> io::Result<()> {
        let mut tmp_os = OsString::from(path);
        tmp_os.push(".tmp");
        let tmp = Path::new(&tmp_os);
        {
            let f = fs::File::create(tmp)?;
            let bw = io::BufWriter::new(f);
            self.save(bw)?;
        }
        fs::rename(tmp, path)
    }

    /// Deserialize JSONL written by [`save`](Self::save).
    pub fn load<R: BufRead>(r: R) -> io::Result<Self> {
        let mut lines = r.lines();
        let header = lines.next().ok_or_else(|| invalid("empty input"))??;
        let (depth, threshold) = parse_meta(&header)?;
        let mut a = Self::new(depth, threshold);
        for line in lines {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let (category, cluster) = parse_cluster_line(&line)?;
            let drain = a
                .by_category
                .entry(category)
                .or_insert_with(|| Drain::new(depth, threshold));
            drain.insert_cluster(cluster);
        }
        Ok(a)
    }
}

// ---------------------------------------------------------------------------
// Write side — hand-rolled JSON
// ---------------------------------------------------------------------------

fn write_cluster_line<W: Write>(w: &mut W, cat: Option<&str>, c: &Cluster) -> io::Result<()> {
    write!(w, "{{\"id\":{}", c.id)?;
    write!(w, ",\"category\":")?;
    write_optional_string(w, cat)?;
    write!(w, ",\"count\":{}", c.count)?;
    write!(w, ",\"first_seen\":")?;
    write_optional_string(w, c.first_seen.as_deref())?;
    write!(w, ",\"last_seen\":")?;
    write_optional_string(w, c.last_seen.as_deref())?;
    write!(w, ",\"template\":[")?;
    for (i, slot) in c.template.iter().enumerate() {
        if i > 0 {
            write!(w, ",")?;
        }
        write_slot(w, slot)?;
    }
    writeln!(w, "]}}")
}

fn write_slot<W: Write>(w: &mut W, slot: &Slot) -> io::Result<()> {
    match slot {
        Slot::Text(s) => {
            write!(w, "{{\"t\":\"L\",\"v\":")?;
            write_json_string(w, s)?;
            write!(w, "}}")
        }
        Slot::Typed(k) => write!(w, "{{\"t\":\"W\",\"k\":\"{}\"}}", k.label()),
        Slot::Star => write!(w, "{{\"t\":\"S\"}}"),
    }
}

fn write_optional_string<W: Write>(w: &mut W, s: Option<&str>) -> io::Result<()> {
    match s {
        Some(s) => write_json_string(w, s),
        None => write!(w, "null"),
    }
}

fn write_json_string<W: Write>(w: &mut W, s: &str) -> io::Result<()> {
    w.write_all(b"\"")?;
    for c in s.chars() {
        match c {
            '"' => w.write_all(b"\\\"")?,
            '\\' => w.write_all(b"\\\\")?,
            '\n' => w.write_all(b"\\n")?,
            '\r' => w.write_all(b"\\r")?,
            '\t' => w.write_all(b"\\t")?,
            '\u{08}' => w.write_all(b"\\b")?,
            '\u{0C}' => w.write_all(b"\\f")?,
            c if (c as u32) < 0x20 => write!(w, "\\u{:04x}", c as u32)?,
            c => {
                let mut buf = [0u8; 4];
                w.write_all(c.encode_utf8(&mut buf).as_bytes())?;
            }
        }
    }
    w.write_all(b"\"")
}

fn json_float(f: f64) -> String {
    if f.fract() == 0.0 && f.is_finite() {
        format!("{f:.1}")
    } else {
        format!("{f}")
    }
}

// ---------------------------------------------------------------------------
// Read side — strict-shape JSONL parser
// ---------------------------------------------------------------------------

fn parse_meta(line: &str) -> io::Result<(usize, f64)> {
    let mut p = JsonParser::new(line);
    p.expect(b'{')?;
    p.expect_key("_meta")?;
    p.expect(b'{')?;
    p.expect_key("version")?;
    let _version = p.parse_uint()?;
    p.expect(b',')?;
    p.expect_key("depth")?;
    let depth = p.parse_uint()?;
    p.expect(b',')?;
    p.expect_key("threshold")?;
    let threshold = p.parse_number()?;
    p.expect(b'}')?;
    p.expect(b'}')?;
    Ok((depth as usize, threshold))
}

fn parse_cluster_line(line: &str) -> io::Result<(Option<String>, Cluster)> {
    let mut p = JsonParser::new(line);
    p.expect(b'{')?;

    p.expect_key("id")?;
    let id = p.parse_uint()?;
    p.expect(b',')?;

    p.expect_key("category")?;
    let category = p.parse_optional_string()?;
    p.expect(b',')?;

    p.expect_key("count")?;
    let count = p.parse_uint()?;
    p.expect(b',')?;

    p.expect_key("first_seen")?;
    let first_seen = p.parse_optional_string()?;
    p.expect(b',')?;

    p.expect_key("last_seen")?;
    let last_seen = p.parse_optional_string()?;
    p.expect(b',')?;

    p.expect_key("template")?;
    let template = p.parse_template()?;

    p.expect(b'}')?;

    Ok((
        category,
        Cluster {
            id,
            template,
            count,
            first_seen,
            last_seen,
        },
    ))
}

struct JsonParser<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> JsonParser<'a> {
    fn new(src: &'a str) -> Self {
        Self { src, pos: 0 }
    }

    fn skip_ws(&mut self) {
        let bytes = self.src.as_bytes();
        while self.pos < bytes.len() && bytes[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.skip_ws();
        self.src.as_bytes().get(self.pos).copied()
    }

    fn expect(&mut self, c: u8) -> io::Result<()> {
        self.skip_ws();
        if self.src.as_bytes().get(self.pos).copied() == Some(c) {
            self.pos += 1;
            Ok(())
        } else {
            Err(invalid(format!(
                "expected '{}' at byte {}",
                c as char, self.pos
            )))
        }
    }

    fn expect_key(&mut self, key: &str) -> io::Result<()> {
        let s = self.parse_string()?;
        if s != key {
            return Err(invalid(format!("expected key {key:?}, got {s:?}")));
        }
        self.expect(b':')
    }

    fn parse_string(&mut self) -> io::Result<String> {
        self.skip_ws();
        self.expect(b'"')?;
        let bytes = self.src.as_bytes();
        let mut out = String::new();
        let mut chunk_start = self.pos;
        while self.pos < bytes.len() {
            match bytes[self.pos] {
                b'"' => {
                    out.push_str(&self.src[chunk_start..self.pos]);
                    self.pos += 1;
                    return Ok(out);
                }
                b'\\' => {
                    out.push_str(&self.src[chunk_start..self.pos]);
                    self.pos += 1;
                    if self.pos >= bytes.len() {
                        return Err(invalid("unterminated escape"));
                    }
                    let esc = bytes[self.pos];
                    self.pos += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{08}'),
                        b'f' => out.push('\u{0C}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            if self.pos + 4 > bytes.len() {
                                return Err(invalid("truncated \\u escape"));
                            }
                            let hex = std::str::from_utf8(&bytes[self.pos..self.pos + 4])
                                .map_err(|_| invalid("invalid \\u hex"))?;
                            let code = u32::from_str_radix(hex, 16)
                                .map_err(|_| invalid("invalid \\u hex"))?;
                            let c = char::from_u32(code)
                                .ok_or_else(|| invalid("invalid \\u codepoint"))?;
                            out.push(c);
                            self.pos += 4;
                        }
                        _ => return Err(invalid(format!("unknown escape \\{}", esc as char))),
                    }
                    chunk_start = self.pos;
                }
                _ => self.pos += 1,
            }
        }
        Err(invalid("unterminated string"))
    }

    fn parse_optional_string(&mut self) -> io::Result<Option<String>> {
        self.skip_ws();
        if self.src[self.pos..].starts_with("null") {
            self.pos += 4;
            Ok(None)
        } else {
            Ok(Some(self.parse_string()?))
        }
    }

    fn parse_number(&mut self) -> io::Result<f64> {
        self.skip_ws();
        let bytes = self.src.as_bytes();
        let start = self.pos;
        if self.pos < bytes.len() && bytes[self.pos] == b'-' {
            self.pos += 1;
        }
        while self.pos < bytes.len() && bytes[self.pos].is_ascii_digit() {
            self.pos += 1;
        }
        if self.pos < bytes.len() && bytes[self.pos] == b'.' {
            self.pos += 1;
            while self.pos < bytes.len() && bytes[self.pos].is_ascii_digit() {
                self.pos += 1;
            }
        }
        if self.pos < bytes.len() && (bytes[self.pos] == b'e' || bytes[self.pos] == b'E') {
            self.pos += 1;
            if self.pos < bytes.len() && (bytes[self.pos] == b'+' || bytes[self.pos] == b'-') {
                self.pos += 1;
            }
            while self.pos < bytes.len() && bytes[self.pos].is_ascii_digit() {
                self.pos += 1;
            }
        }
        if self.pos == start {
            return Err(invalid("expected number"));
        }
        self.src[start..self.pos]
            .parse::<f64>()
            .map_err(|_| invalid("invalid number"))
    }

    fn parse_uint(&mut self) -> io::Result<u64> {
        let f = self.parse_number()?;
        if !(f >= 0.0 && f.fract() == 0.0 && f.is_finite()) {
            return Err(invalid("expected non-negative integer"));
        }
        Ok(f as u64)
    }

    fn parse_template(&mut self) -> io::Result<Vec<Slot>> {
        self.expect(b'[')?;
        let mut out = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(out);
        }
        loop {
            out.push(self.parse_slot()?);
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    return Ok(out);
                }
                _ => return Err(invalid("expected ',' or ']' in template array")),
            }
        }
    }

    fn parse_slot(&mut self) -> io::Result<Slot> {
        self.expect(b'{')?;
        self.expect_key("t")?;
        let tag = self.parse_string()?;
        let s = match tag.as_str() {
            "L" => {
                self.expect(b',')?;
                self.expect_key("v")?;
                let v = self.parse_string()?;
                Slot::Text(v)
            }
            "W" => {
                self.expect(b',')?;
                self.expect_key("k")?;
                let k = self.parse_string()?;
                let kind = TokenKind::from_label(&k)
                    .ok_or_else(|| invalid(format!("unknown token kind {k:?}")))?;
                Slot::Typed(kind)
            }
            "S" => Slot::Star,
            other => return Err(invalid(format!("unknown slot tag {other:?}"))),
        };
        self.expect(b'}')?;
        Ok(s)
    }
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ingest_groups_by_category() {
        let mut a = Analyzer::new(4, 0.5);
        a.ingest_line("2026-06-06T12:00:00Z [net] received: ping peer=1");
        a.ingest_line("2026-06-06T12:00:01Z [net] received: ping peer=2");
        a.ingest_line("2026-06-06T12:00:02Z [validation] UpdateTip: x");
        a.ingest_line("2026-06-06T12:00:03Z [validation] UpdateTip: y");

        let mut by_cat: std::collections::BTreeMap<Option<String>, u64> =
            std::collections::BTreeMap::new();
        for (cat, c) in a.templates() {
            *by_cat.entry(cat.map(str::to_owned)).or_insert(0) += c.count;
        }
        assert_eq!(by_cat.get(&Some("net".into())), Some(&2));
        assert_eq!(by_cat.get(&Some("validation".into())), Some(&2));
    }

    #[test]
    fn jsonl_round_trip_preserves_clusters() {
        let mut a = Analyzer::new(4, 0.5);
        for n in 0..5 {
            a.ingest_line(&format!(
                "2026-06-06T12:00:0{n}Z [net] received: ping peer={n}"
            ));
        }
        a.ingest_line("2026-06-06T12:00:09Z [validation] UpdateTip: foo");

        let mut buf = Vec::new();
        a.save(&mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();

        let b = Analyzer::load(io::Cursor::new(s)).unwrap();
        assert_eq!(a.depth(), b.depth());
        assert_eq!(a.threshold(), b.threshold());

        let total_a: u64 = a.templates().map(|(_, c)| c.count).sum();
        let total_b: u64 = b.templates().map(|(_, c)| c.count).sum();
        assert_eq!(total_a, total_b);

        let cluster_count_a = a.templates().count();
        let cluster_count_b = b.templates().count();
        assert_eq!(cluster_count_a, cluster_count_b);
    }

    #[test]
    fn jsonl_escape_round_trip() {
        // Build a saved file with a literal token containing quote, backslash,
        // and a control char; load it; assert the slot survives intact.
        let saved = concat!(
            r#"{"_meta":{"version":1,"depth":4,"threshold":0.5}}"#,
            "\n",
            r#"{"id":1,"category":"net","count":1,"first_seen":null,"last_seen":null,"template":[{"t":"L","v":"hard\"slash\\tab\there"}]}"#,
            "\n",
        );
        let a = Analyzer::load(io::Cursor::new(saved)).unwrap();
        let (_, c) = a.templates().next().unwrap();
        assert_eq!(c.template.len(), 1);
        match &c.template[0] {
            Slot::Text(s) => assert_eq!(s, "hard\"slash\\tab\there"),
            other => panic!("expected Text slot, got {other:?}"),
        }
    }

    #[test]
    fn save_atomic_writes_file_and_loads_back() {
        let dir = std::env::temp_dir().join("drnlog-save-atomic-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.jsonl");

        let mut a = Analyzer::new(4, 0.5);
        a.ingest_line("2026-06-06T12:00:00Z [net] received: ping peer=1");
        a.save_atomic(&path).unwrap();
        assert!(path.exists());
        let tmp_path = {
            let mut p = OsString::from(&path);
            p.push(".tmp");
            std::path::PathBuf::from(p)
        };
        assert!(!tmp_path.exists(), "tmp should have been renamed away");

        let f = std::fs::File::open(&path).unwrap();
        let b = Analyzer::load(io::BufReader::new(f)).unwrap();
        assert_eq!(b.templates().count(), 1);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
