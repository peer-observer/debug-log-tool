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

/// JSONL state format version written by [`Analyzer::save`].
const STATE_VERSION: u64 = 1;

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
    /// line, in an order that depends only on their contents, so the same
    /// catalogue always writes the same bytes. Use
    /// [`save_atomic`](Self::save_atomic) to write a file safely (this
    /// method is for callers that already own a writer).
    pub fn save<W: Write>(&self, mut w: W) -> io::Result<()> {
        writeln!(
            w,
            r#"{{"_meta":{{"version":{},"depth":{},"threshold":{}}}}}"#,
            STATE_VERSION,
            self.depth,
            json_float(self.threshold),
        )?;
        let mut clusters: Vec<(Option<&str>, &Cluster)> = self.templates().collect();
        // Ids are unique per category, but a hand-edited state file can
        // repeat one; fall back to the template so the order stays total.
        clusters.sort_by(|(cat_a, a), (cat_b, b)| {
            cat_a
                .cmp(cat_b)
                .then(a.id.cmp(&b.id))
                .then_with(|| a.template.cmp(&b.template))
        });
        for (cat, c) in clusters {
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
    let version = p.parse_uint()?;
    if version > STATE_VERSION {
        return Err(invalid(format!(
            "state file version {version} is newer than {STATE_VERSION}"
        )));
    }
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

    /// Ids and counts are parsed as integers, not via [`parse_number`]: an
    /// `f64` silently rounds anything past 2^53, and a cluster id has to come
    /// back exactly as it was written.
    ///
    /// [`parse_number`]: Self::parse_number
    fn parse_uint(&mut self) -> io::Result<u64> {
        self.skip_ws();
        let bytes = self.src.as_bytes();
        let start = self.pos;
        let mut value: u64 = 0;
        while let Some(&b) = bytes.get(self.pos) {
            if !b.is_ascii_digit() {
                break;
            }
            value = value
                .checked_mul(10)
                .and_then(|v| v.checked_add((b - b'0') as u64))
                .ok_or_else(|| invalid("integer too large"))?;
            self.pos += 1;
        }
        if self.pos == start {
            return Err(invalid("expected a non-negative integer"));
        }
        Ok(value)
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

    /// Saved clusters come out sorted by (category, id), not in hash order.
    #[test]
    fn save_orders_clusters_by_category_and_id() {
        let mut a = Analyzer::new(4, 0.5);
        for n in 0..12 {
            a.ingest_line(&format!(
                "2026-06-06T12:00:00Z [net] alpha{n} one two three"
            ));
            a.ingest_line(&format!(
                "2026-06-06T12:00:00Z [bench] beta{n} one two three"
            ));
            a.ingest_line(&format!("2026-06-06T12:00:00Z [mempool] gamma{n} one two"));
            a.ingest_line(&format!("no timestamp here {n} one two"));
        }
        assert!(
            a.templates().count() > 12,
            "need enough clusters to shuffle"
        );

        let mut buf = Vec::new();
        a.save(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();

        let keys: Vec<(Option<String>, u64)> = text
            .lines()
            .skip(1)
            .map(|l| {
                let (cat, cluster) = parse_cluster_line(l).unwrap();
                (cat, cluster.id)
            })
            .collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted, "save output must be sorted by (category, id)");
    }

    const HEADER: &str = r#"{"_meta":{"version":1,"depth":4,"threshold":0.5}}"#;

    fn load_str(s: &str) -> io::Result<Analyzer> {
        Analyzer::load(io::Cursor::new(s.to_string()))
    }

    #[test]
    fn a_newer_state_version_is_refused() {
        let newer = r#"{"_meta":{"version":2,"depth":4,"threshold":0.5}}"#;
        let err = load_str(newer).unwrap_err();
        assert!(err.to_string().contains("version 2"), "{err}");
        assert!(load_str(HEADER).is_ok());
    }

    #[test]
    fn meta_round_trips_depth_and_threshold() {
        for (depth, threshold) in [(2usize, 0.0f64), (4, 0.5), (9, 1.0), (5, 0.125)] {
            let a = Analyzer::new(depth, threshold);
            let mut buf = Vec::new();
            a.save(&mut buf).unwrap();
            let b = Analyzer::load(io::Cursor::new(buf)).unwrap();
            assert_eq!(b.depth(), depth);
            assert_eq!(b.threshold(), threshold);
        }
    }

    #[test]
    fn malformed_state_is_an_error_not_a_panic() {
        for bad in [
            "",
            "not json",
            r#"{"_meta":{"version":1,"depth":4}}"#,
            r#"{"meta":{"version":1,"depth":4,"threshold":0.5}}"#,
            r#"{"_meta":{"version":1,"depth":-4,"threshold":0.5}}"#,
            r#"{"_meta":{"version":1,"depth":4,"threshold":x}}"#,
        ] {
            assert!(load_str(bad).is_err(), "accepted {bad:?}");
        }
        let fields = r#""id":1,"category":null,"count":1,"first_seen":null,"last_seen":null"#;
        for tail in [
            "",
            r#","template":[{"t":"Q"}]"#,
            r#","template":[{"t":"W","k":"NOPE"}]"#,
            r#","template":[{"t":"L","v":"unterminated"#,
            r#","template":[{"t":"L","v":"a"}"#,
            r#","template":[{"t":"L","v":"a"},]"#,
        ] {
            let s = format!("{HEADER}\n{{{fields}{tail}}}\n");
            assert!(load_str(&s).is_err(), "accepted {s}");
        }
    }

    #[test]
    fn blank_lines_in_state_are_skipped() {
        let cluster = r#"{"id":1,"category":"net","count":1,"first_seen":null,"last_seen":null,"template":[{"t":"S"}]}"#;
        let a = load_str(&format!("{HEADER}\n\n{cluster}\n\n")).unwrap();
        assert_eq!(a.templates().count(), 1);
    }

    #[test]
    fn every_slot_and_escape_survives_a_round_trip() {
        let mut a = Analyzer::new(4, 0.5);
        // One template with a literal, a typed and a starred slot, where the
        // literal needs escaping and carries a non-ASCII char.
        let bell = '\u{7}';
        a.ingest_line(&format!(
            "2026-06-06T12:00:00Z [net] \"quote\\back\"{bell}\u{b5} one ding peer=1"
        ));
        a.ingest_line(&format!(
            "2026-06-06T12:00:01Z [net] \"quote\\back\"{bell}\u{b5} one dong peer=2"
        ));
        assert_eq!(a.templates().count(), 1);

        let mut buf = Vec::new();
        a.save(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains(r#"{"t":"S"}"#), "expected a star: {text}");
        assert!(text.contains(r#"{"t":"W","k":"PEER"}"#), "{text}");
        assert!(text.contains("\\u0007"), "control char escaped: {text}");

        let b = Analyzer::load(io::Cursor::new(text)).unwrap();
        let (_, before) = a.templates().next().unwrap();
        let (_, after) = b.templates().next().unwrap();
        assert_eq!(before.template, after.template);
        assert_eq!(before.count, after.count);
        assert_eq!(before.first_seen, after.first_seen);
        assert_eq!(before.last_seen, after.last_seen);
    }

    #[test]
    fn every_json_string_escape_decodes() {
        let escaped = r#"q\"b\\s\/n\nr\rt\tb\bf\fAAeé"#;
        let cluster = format!(
            r#"{{"id":1,"category":"net","count":1,"first_seen":null,"last_seen":null,"template":[{{"t":"L","v":"{escaped}"}}]}}"#
        );
        let a = load_str(&format!("{HEADER}\n{cluster}\n")).unwrap();
        let (_, c) = a.templates().next().unwrap();
        let expected = "q\"b\\s/n\nr\rt\tb\u{8}f\u{c}AAe\u{e9}";
        match &c.template[0] {
            Slot::Text(s) => assert_eq!(s, expected),
            other => panic!("expected a literal slot, got {other:?}"),
        }
    }

    #[test]
    fn broken_escapes_are_rejected() {
        for broken in [r#"a\"#, r#"a\x"#, r#"a\u00"#, r#"a\uZZZZ"#, r#"a\ud800"#] {
            let cluster = format!(
                r#"{{"id":1,"category":"net","count":1,"first_seen":null,"last_seen":null,"template":[{{"t":"L","v":"{broken}"}}]}}"#
            );
            assert!(
                load_str(&format!("{HEADER}\n{cluster}\n")).is_err(),
                "accepted {broken}"
            );
        }
    }

    #[test]
    fn ids_and_counts_survive_exactly() {
        let fields = |id: &str| {
            format!(
                r#"{{"id":{id},"category":null,"count":{id},"first_seen":null,"last_seen":null,"template":[]}}"#
            )
        };
        // Past 2^53 an f64 would round these; both have to come back whole.
        for exact in ["9007199254740993", "18446744073709551615"] {
            let state = format!("{HEADER}\n{}\n", fields(exact));
            let loaded = load_str(&state).unwrap();
            let (_, cluster) = loaded.templates().next().unwrap();
            assert_eq!(cluster.id.to_string(), exact);
            assert_eq!(cluster.count.to_string(), exact);
            // And saving puts the same digits back.
            let mut buf = Vec::new();
            loaded.save(&mut buf).unwrap();
            assert!(String::from_utf8_lossy(&buf).contains(exact));
        }
        // One past u64 is refused rather than wrapped.
        let state = format!("{HEADER}\n{}\n", fields("18446744073709551616"));
        let err = load_str(&state).unwrap_err();
        assert!(err.to_string().contains("too large"), "{err}");
    }

    #[test]
    fn a_repeated_id_still_saves_deterministically() {
        // Ids are unique when we write them, but a hand-edited file can
        // repeat one, and the output still has to be reproducible.
        let cluster = |template: &str| {
            format!(
                r#"{{"id":1,"category":"net","count":1,"first_seen":null,"last_seen":null,"template":[{template}]}}"#
            )
        };
        let state = format!(
            "{HEADER}\n{}\n{}\n{}\n",
            cluster(r#"{"t":"L","v":"a"}"#),
            cluster(r#"{"t":"S"}"#),
            cluster(r#"{"t":"W","k":"PEER"}"#),
        );
        let first = {
            let mut buf = Vec::new();
            load_str(&state).unwrap().save(&mut buf).unwrap();
            buf
        };
        for _ in 0..8 {
            let mut buf = Vec::new();
            load_str(&state).unwrap().save(&mut buf).unwrap();
            assert_eq!(buf, first, "repeated ids saved in a different order");
        }
    }
}
