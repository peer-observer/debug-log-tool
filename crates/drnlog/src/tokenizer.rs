//! Tokenizer for Bitcoin Core `debug.log` content.
//!
//! Two passes:
//! 1. **Structural split** — break a line's content into atoms on whitespace
//!    plus the punctuation bitcoind format strings consistently use
//!    (`(`, `)`, `'`, trailing `:` / `,`). `=` is split too *except* on
//!    recognizer-protected prefixes (`peer=`, `height=`) where the
//!    classifier wants to fold the whole `peer=1234` slice. `[…]` brackets
//!    are split too except when they look like an IPv6-with-port wrapper
//!    so `[::1]:8333` stays whole.
//! 2. **Classification** — try a fixed slice of recognizer functions in
//!    priority order. Each recognizer takes the current atom plus a peek
//!    at the next atom (for two-atom folds like `42 bytes` → `<BYTES>`).
//!
//! Adding a new recognized value type touches three spots in this file:
//! 1. Add a variant to [`Token`] (and to [`TokenKind`]).
//! 2. Write `recognize_<thing>` with signature
//!    `for<'a> fn(&'a str, Option<&'a str>) -> Option<(Token<'a>, usize)>`.
//! 3. Insert it into [`RECOGNIZERS`] at the right priority.

use std::mem::discriminant;

/// A single token. All payload variants borrow from the source line; the
/// tokenizer is zero-copy on the hot path.
#[derive(Debug, Clone, Copy, Eq)]
pub enum Token<'a> {
    /// Literal text that no recognizer matched.
    Text(&'a str),
    /// `peer=<int>` collapsed into one token.
    PeerId(&'a str),
    /// `height=<int>` collapsed into one token.
    BlockHeight(&'a str),
    /// Decimal integer with optional sign.
    Int(&'a str),
    /// Decimal float with optional sign and exponent.
    Float(&'a str),
    /// Exactly 64 lowercase hex chars (block / tx hash).
    Hash(&'a str),
    /// `0x` + hex digits, or 8..=63 raw hex chars containing at least one
    /// non-decimal hex digit (`a..=f`).
    Hex(&'a str),
    /// `a.b.c.d`.
    Ipv4(&'a str),
    /// `a.b.c.d:<port>`.
    Ipv4Port(&'a str),
    /// Bare IPv6 (e.g. `::1`, `fe80::abcd`).
    Ipv6(&'a str),
    /// `[<ipv6>]:<port>`.
    Ipv6Port(&'a str),
    /// `<base32>.onion` (v2 or v3 length).
    Onion(&'a str),
    /// `<base32>.onion:<port>`.
    OnionPort(&'a str),
    /// `<base32>.b32.i2p`
    I2P(&'a str),
    /// `<base32>.b32.i2p:0`
    I2PPort(&'a str),
    /// Folded `<int> bytes` pair.
    ByteCount(&'a str),
    /// Number with a time-unit suffix (`ns`, `us`, `μs`, `ms`, `s`, `m`,
    /// `h`, `d`).
    Duration(&'a str),
    /// Bech32 address (`bc1…`, `tb1…`, `bcrt1…`).
    Bech32(&'a str),
    /// Base58 string (length / alphabet heuristic, useful for legacy
    /// addresses and txids serialised that way).
    Base58(&'a str),
}

/// `Text` compares by content; typed variants compare by *discriminant
/// only* — so `PeerId("3") == PeerId("4567")` is true. This is what makes
/// Drain collapse two log lines that differ only at typed positions into a
/// single template slot rather than going all the way to `<*>`.
impl PartialEq for Token<'_> {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Token::Text(a), Token::Text(b)) => a == b,
            (a, b) => discriminant(a) == discriminant(b),
        }
    }
}

impl<'a> Token<'a> {
    /// The kind tag, or `None` for [`Token::Text`].
    pub fn kind(&self) -> Option<TokenKind> {
        Some(match self {
            Token::Text(_) => return None,
            Token::PeerId(_) => TokenKind::PeerId,
            Token::BlockHeight(_) => TokenKind::BlockHeight,
            Token::Int(_) => TokenKind::Int,
            Token::Float(_) => TokenKind::Float,
            Token::Hash(_) => TokenKind::Hash,
            Token::Hex(_) => TokenKind::Hex,
            Token::Ipv4(_) => TokenKind::Ipv4,
            Token::Ipv4Port(_) => TokenKind::Ipv4Port,
            Token::Ipv6(_) => TokenKind::Ipv6,
            Token::Ipv6Port(_) => TokenKind::Ipv6Port,
            Token::Onion(_) => TokenKind::Onion,
            Token::OnionPort(_) => TokenKind::OnionPort,
            Token::I2P(_) => TokenKind::I2P,
            Token::I2PPort(_) => TokenKind::I2PPort,
            Token::ByteCount(_) => TokenKind::ByteCount,
            Token::Duration(_) => TokenKind::Duration,
            Token::Bech32(_) => TokenKind::Bech32,
            Token::Base58(_) => TokenKind::Base58,
        })
    }

    /// The raw text slice this token borrows from the source.
    pub fn text(&self) -> &'a str {
        match self {
            Token::Text(s)
            | Token::PeerId(s)
            | Token::BlockHeight(s)
            | Token::Int(s)
            | Token::Float(s)
            | Token::Hash(s)
            | Token::Hex(s)
            | Token::Ipv4(s)
            | Token::Ipv4Port(s)
            | Token::Ipv6(s)
            | Token::Ipv6Port(s)
            | Token::Onion(s)
            | Token::OnionPort(s)
            | Token::I2P(s)
            | Token::I2PPort(s)
            | Token::ByteCount(s)
            | Token::Duration(s)
            | Token::Bech32(s)
            | Token::Base58(s) => s,
        }
    }
}

/// Flat kind tag mirroring [`Token`] minus `Text`. Used in `Slot::Typed`,
/// route-key prefixes, and the JSONL persistence format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenKind {
    PeerId,
    BlockHeight,
    Int,
    Float,
    Hash,
    Hex,
    Ipv4,
    Ipv4Port,
    Ipv6,
    Ipv6Port,
    Onion,
    OnionPort,
    I2P,
    I2PPort,
    ByteCount,
    Duration,
    Bech32,
    Base58,
}

impl TokenKind {
    /// Canonical short label used in template rendering and JSONL.
    pub fn label(self) -> &'static str {
        match self {
            TokenKind::PeerId => "PEER",
            TokenKind::BlockHeight => "HEIGHT",
            TokenKind::Int => "INT",
            TokenKind::Float => "FLOAT",
            TokenKind::Hash => "HASH",
            TokenKind::Hex => "HEX",
            TokenKind::Ipv4 => "IPv4",
            TokenKind::Ipv4Port => "IPv4:PORT",
            TokenKind::Ipv6 => "IPv6",
            TokenKind::Ipv6Port => "IPv6:PORT",
            TokenKind::Onion => "ONION",
            TokenKind::OnionPort => "ONION:PORT",
            TokenKind::I2P => "I2P",
            TokenKind::I2PPort => "I2P:PORT",
            TokenKind::ByteCount => "BYTES",
            TokenKind::Duration => "DUR",
            TokenKind::Bech32 => "BECH32",
            TokenKind::Base58 => "BASE58",
        }
    }

    /// Inverse of [`label`](Self::label). `None` on unknown labels.
    pub fn from_label(s: &str) -> Option<Self> {
        Some(match s {
            "PEER" => Self::PeerId,
            "HEIGHT" => Self::BlockHeight,
            "INT" => Self::Int,
            "FLOAT" => Self::Float,
            "HASH" => Self::Hash,
            "HEX" => Self::Hex,
            "IPv4" => Self::Ipv4,
            "IPv4:PORT" => Self::Ipv4Port,
            "IPv6" => Self::Ipv6,
            "IPv6:PORT" => Self::Ipv6Port,
            "ONION" => Self::Onion,
            "ONION:PORT" => Self::OnionPort,
            "I2P" => Self::I2P,
            "I2P:PORT" => Self::I2PPort,
            "BYTES" => Self::ByteCount,
            "DUR" => Self::Duration,
            "BECH32" => Self::Bech32,
            "BASE58" => Self::Base58,
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------------------
// Pass 1: structural split
// ---------------------------------------------------------------------------

fn split_content(content: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for ws_atom in content.split_ascii_whitespace() {
        split_atom(ws_atom, &mut out);
    }
    out
}

fn split_atom<'a>(s: &'a str, out: &mut Vec<&'a str>) {
    // Peel a single trailing `:` or `,` so it never glues onto a
    // recognizer-matched token (e.g. `peer=9:` or `4d2a...hash,`).
    let tail = s.as_bytes().last();
    let (body, trailing_sep) = if s.len() > 1 && matches!(tail, Some(&b':') | Some(&b',')) {
        (&s[..s.len() - 1], Some(&s[s.len() - 1..]))
    } else {
        (s, None)
    };

    // Protected tokens recognizers want to see whole.
    if looks_like_ipv6_port(body) || body.starts_with("peer=") || body.starts_with("height=") {
        out.push(body);
        if let Some(t) = trailing_sep {
            out.push(t);
        }
        return;
    }

    let bytes = body.as_bytes();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if matches!(c, b'=' | b'(' | b')' | b'[' | b']' | b'\'' | b'/') {
            if i > start {
                out.push(&body[start..i]);
            }
            // Keep the punctuation as its own structural token so the
            // template visibly preserves it.
            out.push(&body[i..i + 1]);
            start = i + 1;
        }
        i += 1;
    }
    if start < bytes.len() {
        out.push(&body[start..]);
    }
    if let Some(t) = trailing_sep {
        out.push(t);
    }
}

fn looks_like_ipv6_port(s: &str) -> bool {
    s.starts_with('[') && s.contains("]:") && s.contains(':')
}

// ---------------------------------------------------------------------------
// Pass 2: classification
// ---------------------------------------------------------------------------

type Classifier = for<'a> fn(&'a str, Option<&'a str>) -> Option<(Token<'a>, usize)>;

const RECOGNIZERS: &[Classifier] = &[
    recognize_peer_id,
    recognize_block_height,
    recognize_byte_count,
    recognize_duration,
    recognize_hash,
    recognize_ipv6_port,
    recognize_ipv6,
    recognize_ipv4_port,
    recognize_ipv4,
    recognize_onion_port,
    recognize_onion,
    recognize_i2p_port,
    recognize_i2p,
    recognize_bech32,
    recognize_hex,
    recognize_base58,
    recognize_float,
    recognize_int,
];

/// Classify a single atom. The second value is the number of atoms
/// consumed (typically 1; recognizers like `ByteCount` consume 2).
pub fn classify<'a>(s: &'a str, peek: Option<&'a str>) -> (Token<'a>, usize) {
    for r in RECOGNIZERS {
        if let Some((t, n)) = r(s, peek) {
            return (t, n);
        }
    }
    (Token::Text(s), 1)
}

/// Tokenize bitcoind log *content* (i.e. after `line::parse` has stripped
/// timestamp and category). Borrows from `content`; no allocations beyond
/// the returned `Vec`.
pub fn tokenize(content: &str) -> Vec<Token<'_>> {
    let atoms = split_content(content);
    let mut out = Vec::with_capacity(atoms.len());
    let mut i = 0;
    while i < atoms.len() {
        let peek = atoms.get(i + 1).copied();
        let (tok, n) = classify(atoms[i], peek);
        out.push(tok);
        i += n;
    }
    out
}

// ---------------------------------------------------------------------------
// Recognizers
// ---------------------------------------------------------------------------

fn recognize_peer_id<'a>(s: &'a str, _peek: Option<&'a str>) -> Option<(Token<'a>, usize)> {
    let rest = s.strip_prefix("peer=")?;
    if is_signed_int(rest) {
        Some((Token::PeerId(s), 1))
    } else {
        None
    }
}

fn recognize_block_height<'a>(s: &'a str, _peek: Option<&'a str>) -> Option<(Token<'a>, usize)> {
    let rest = s.strip_prefix("height=")?;
    if is_unsigned_int(rest) {
        Some((Token::BlockHeight(s), 1))
    } else {
        None
    }
}

fn recognize_byte_count<'a>(s: &'a str, peek: Option<&'a str>) -> Option<(Token<'a>, usize)> {
    if !is_unsigned_int(s) {
        return None;
    }
    if peek? != "bytes" {
        return None;
    }
    Some((Token::ByteCount(s), 2))
}

fn recognize_duration<'a>(s: &'a str, _peek: Option<&'a str>) -> Option<(Token<'a>, usize)> {
    // Order matters: longer suffixes first so "ms" wins over "s".
    const UNITS: &[&str] = &["μs", "ns", "us", "ms", "s", "m", "h", "d"];
    for unit in UNITS {
        if let Some(num) = s.strip_suffix(unit)
            && (is_float(num) || is_unsigned_int(num) || is_signed_int(num))
        {
            return Some((Token::Duration(s), 1));
        }
    }
    None
}

fn recognize_hash<'a>(s: &'a str, _peek: Option<&'a str>) -> Option<(Token<'a>, usize)> {
    if s.len() != 64 {
        return None;
    }
    if s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        Some((Token::Hash(s), 1))
    } else {
        None
    }
}

fn recognize_hex<'a>(s: &'a str, _peek: Option<&'a str>) -> Option<(Token<'a>, usize)> {
    if let Some(rest) = s.strip_prefix("0x")
        && !rest.is_empty()
        && rest.bytes().all(is_ascii_hex)
    {
        return Some((Token::Hex(s), 1));
    }
    let len = s.len();
    if (8..=63).contains(&len)
        && s.bytes().all(is_ascii_hex)
        && s.bytes().any(|b| matches!(b, b'a'..=b'f' | b'A'..=b'F'))
    {
        return Some((Token::Hex(s), 1));
    }
    None
}

fn recognize_ipv4<'a>(s: &'a str, _peek: Option<&'a str>) -> Option<(Token<'a>, usize)> {
    if is_ipv4(s) {
        Some((Token::Ipv4(s), 1))
    } else {
        None
    }
}

fn recognize_ipv4_port<'a>(s: &'a str, _peek: Option<&'a str>) -> Option<(Token<'a>, usize)> {
    let (host, port) = s.rsplit_once(':')?;
    if !is_ipv4(host) || !is_port(port) {
        return None;
    }
    Some((Token::Ipv4Port(s), 1))
}

fn recognize_ipv6<'a>(s: &'a str, _peek: Option<&'a str>) -> Option<(Token<'a>, usize)> {
    if is_ipv6(s) {
        Some((Token::Ipv6(s), 1))
    } else {
        None
    }
}

fn recognize_ipv6_port<'a>(s: &'a str, _peek: Option<&'a str>) -> Option<(Token<'a>, usize)> {
    let inner = s.strip_prefix('[')?;
    let (host, rest) = inner.split_once(']')?;
    if !is_ipv6(host) {
        return None;
    }
    let port = rest.strip_prefix(':')?;
    if !is_port(port) {
        return None;
    }
    Some((Token::Ipv6Port(s), 1))
}

fn recognize_onion<'a>(s: &'a str, _peek: Option<&'a str>) -> Option<(Token<'a>, usize)> {
    let base = s.strip_suffix(".onion")?;
    if is_onion_base(base) {
        Some((Token::Onion(s), 1))
    } else {
        None
    }
}

fn recognize_onion_port<'a>(s: &'a str, _peek: Option<&'a str>) -> Option<(Token<'a>, usize)> {
    let (host, port) = s.rsplit_once(':')?;
    let base = host.strip_suffix(".onion")?;
    if !is_onion_base(base) || !is_port(port) {
        return None;
    }
    Some((Token::OnionPort(s), 1))
}

fn recognize_i2p<'a>(s: &'a str, _peek: Option<&'a str>) -> Option<(Token<'a>, usize)> {
    let base = s.strip_suffix(".b32.i2p")?;
    match is_i2p_base(base) {
        true => Some((Token::I2P(s), 1)),
        false => None,
    }
}

fn recognize_i2p_port<'a>(s: &'a str, _peek: Option<&'a str>) -> Option<(Token<'a>, usize)> {
    let (host, port) = s.rsplit_once(':')?;
    let base = host.strip_suffix(".b32.i2p")?;
    if !is_i2p_base(base) || !is_port(port) {
        return None;
    }
    Some((Token::I2PPort(s), 1))
}

fn recognize_bech32<'a>(s: &'a str, _peek: Option<&'a str>) -> Option<(Token<'a>, usize)> {
    let body = s
        .strip_prefix("bc1")
        .or_else(|| s.strip_prefix("tb1"))
        .or_else(|| s.strip_prefix("bcrt1"))?;
    const BECH32: &[u8] = b"qpzry9x8gf2tvdw0s3jn54khce6mua7l";
    if body.len() >= 6 && body.bytes().all(|b| BECH32.contains(&b)) {
        Some((Token::Bech32(s), 1))
    } else {
        None
    }
}

fn recognize_base58<'a>(s: &'a str, _peek: Option<&'a str>) -> Option<(Token<'a>, usize)> {
    const B58: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    let len = s.len();
    if !(26..=35).contains(&len) {
        return None;
    }
    if !s.bytes().all(|b| B58.contains(&b)) {
        return None;
    }
    if !s.bytes().any(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    Some((Token::Base58(s), 1))
}

fn recognize_float<'a>(s: &'a str, _peek: Option<&'a str>) -> Option<(Token<'a>, usize)> {
    if is_float(s) {
        Some((Token::Float(s), 1))
    } else {
        None
    }
}

fn recognize_int<'a>(s: &'a str, _peek: Option<&'a str>) -> Option<(Token<'a>, usize)> {
    if is_signed_int(s) {
        Some((Token::Int(s), 1))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Shared lexical predicates
// ---------------------------------------------------------------------------

fn is_ascii_hex(b: u8) -> bool {
    b.is_ascii_hexdigit()
}

fn is_unsigned_int(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit())
}

fn is_signed_int(s: &str) -> bool {
    let b = s.as_bytes();
    if b.is_empty() {
        return false;
    }
    let start = if b[0] == b'+' || b[0] == b'-' { 1 } else { 0 };
    if start == b.len() {
        return false;
    }
    b[start..].iter().all(|c| c.is_ascii_digit())
}

fn is_float(s: &str) -> bool {
    let b = s.as_bytes();
    if b.is_empty() {
        return false;
    }
    let mut i = 0;
    if b[i] == b'+' || b[i] == b'-' {
        i += 1;
    }
    let int_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == int_start {
        return false;
    }
    if i == b.len() || b[i] != b'.' {
        return false;
    }
    i += 1;
    let frac_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == frac_start {
        return false;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        let exp_start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == exp_start {
            return false;
        }
    }
    i == b.len()
}

fn is_port(s: &str) -> bool {
    if !is_unsigned_int(s) {
        return false;
    }
    s.parse::<u32>().is_ok_and(|n| n <= 65535)
}

fn is_ipv4(s: &str) -> bool {
    let mut parts = s.split('.');
    let mut count = 0;
    for p in parts.by_ref() {
        count += 1;
        if count > 4 {
            return false;
        }
        if p.is_empty() || p.len() > 3 || !p.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        if p.parse::<u8>().is_err() {
            return false;
        }
    }
    count == 4
}

fn is_ipv6(s: &str) -> bool {
    let colons = s.bytes().filter(|&b| b == b':').count();
    if colons < 2 {
        return false;
    }
    if s.matches("::").count() > 1 {
        return false;
    }
    s.split(':')
        .all(|p| p.is_empty() || (p.len() <= 4 && p.bytes().all(is_ascii_hex)))
}

fn is_onion_base(base: &str) -> bool {
    if base.len() != 16 && base.len() != 56 {
        return false;
    }
    is_base32(base)
}

fn is_i2p_base(base: &str) -> bool {
    if base.len() == 52 {
        return is_base32(base);
    }
    false
}

fn is_base32(base: &str) -> bool {
    !base.is_empty() && base.bytes().all(|b| matches!(b, b'a'..=b'z' | b'2'..=b'7'))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(line: &str) -> Vec<TokenKind> {
        tokenize(line)
            .into_iter()
            .filter_map(|t| t.kind())
            .collect()
    }

    fn render(line: &str) -> String {
        tokenize(line)
            .into_iter()
            .map(|t| match t.kind() {
                None => t.text().to_string(),
                Some(k) => format!("<{}>", k.label()),
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn token_equality_collapses_typed_values() {
        assert_eq!(Token::PeerId("3"), Token::PeerId("4567"));
        assert_eq!(Token::Int("1"), Token::Int("999"));
        assert_ne!(Token::Text("a"), Token::Text("b"));
        assert_ne!(Token::Int("1"), Token::PeerId("1"));
        assert_ne!(Token::Int("1"), Token::Text("1"));
    }

    #[test]
    fn label_round_trip() {
        for k in [
            TokenKind::PeerId,
            TokenKind::BlockHeight,
            TokenKind::Int,
            TokenKind::Float,
            TokenKind::Hash,
            TokenKind::Hex,
            TokenKind::Ipv4,
            TokenKind::Ipv4Port,
            TokenKind::Ipv6,
            TokenKind::Ipv6Port,
            TokenKind::Onion,
            TokenKind::OnionPort,
            TokenKind::I2P,
            TokenKind::I2PPort,
            TokenKind::ByteCount,
            TokenKind::Duration,
            TokenKind::Bech32,
            TokenKind::Base58,
        ] {
            assert_eq!(TokenKind::from_label(k.label()), Some(k));
        }
        assert!(TokenKind::from_label("NOPE").is_none());
    }

    #[test]
    fn recognizers_positive() {
        assert_eq!(kinds("peer=42"), vec![TokenKind::PeerId]);
        assert_eq!(kinds("height=800000"), vec![TokenKind::BlockHeight]);
        assert_eq!(kinds("37 bytes"), vec![TokenKind::ByteCount]);
        assert_eq!(kinds("12.34ms"), vec![TokenKind::Duration]);
        assert_eq!(kinds("500ns"), vec![TokenKind::Duration]);
        assert_eq!(kinds(&"a".repeat(64)), vec![TokenKind::Hash]);
        assert_eq!(kinds("0x20000000"), vec![TokenKind::Hex]);
        assert_eq!(kinds("deadbeef00"), vec![TokenKind::Hex]); // 10 hex incl. a-f
        assert_eq!(kinds("192.0.2.1"), vec![TokenKind::Ipv4]);
        assert_eq!(kinds("192.0.2.1:8333"), vec![TokenKind::Ipv4Port]);
        assert_eq!(kinds("fe80::1"), vec![TokenKind::Ipv6]);
        assert_eq!(kinds("[::1]:8333"), vec![TokenKind::Ipv6Port]);
        assert_eq!(kinds("abcdefghijklmnop.onion"), vec![TokenKind::Onion]);
        assert_eq!(
            kinds("5pcdrykqq4dewrtm4ngeduhivsqqmzs5rvt5icxnjmazn4c3yxta.b32.i2p"),
            vec![TokenKind::I2P]
        );
        assert_eq!(
            kinds("5pcdrykqq4dewrtm4ngeduhivsqqmzs5rvt5icxnjmazn4c3yxta.b32.i2p:0"),
            vec![TokenKind::I2PPort]
        );
        assert_eq!(
            kinds("abcdefghijklmnop.onion:8333"),
            vec![TokenKind::OnionPort]
        );
        assert_eq!(
            kinds("bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4"),
            vec![TokenKind::Bech32]
        );
        assert_eq!(
            kinds("1BvBMSEYstWetqTFn5Au4m4GFg7xJaNVN2"),
            vec![TokenKind::Base58]
        );
        assert_eq!(kinds("12.34"), vec![TokenKind::Float]);
        assert_eq!(kinds("-7"), vec![TokenKind::Int]);
    }

    #[test]
    fn recognizers_negative_examples_stay_text() {
        // Garbage inputs that *don't* match — confirm they fall through to Text.
        assert!(kinds("peer=abc").is_empty());
        assert!(kinds("height=-1").is_empty());
        assert!(kinds("12.34mss").is_empty()); // not a known unit
        assert!(kinds(&"a".repeat(65)).is_empty()); // hash must be exactly 64
        assert!(kinds("192.0.2").is_empty()); // 3-octet
        assert!(kinds("192.0.2.999").is_empty()); // octet out of range
        assert!(kinds("not.onion").is_empty()); // wrong base length
        assert!(kinds("bc1!").is_empty()); // invalid bech32 alphabet
        assert!(kinds("123").is_empty() || kinds("123") == vec![TokenKind::Int]); // pure digit = Int, not Base58
    }

    #[test]
    fn tokenize_full_lines() {
        assert_eq!(
            render("received: inv (8 bytes) peer=3"),
            "received : inv ( <BYTES> ) <PEER>"
        );
        assert_eq!(
            render("sending inv (2377 bytes) peer=8632"),
            "sending inv ( <BYTES> ) <PEER>"
        );
        assert_eq!(
            render(
                "got inv: wtx 6c83e32df10ff426274085a8dd329fe2525899f9b0616207769389c1a60e1351  have peer=799"
            ),
            "got inv : wtx <HASH> have <PEER>"
        );
        assert_eq!(
            render(
                "got inv: wtx 6c83e32df10ff426274085a8dd329fe2525899f9b0616207769389c1a60e1351  have peer=799"
            ),
            "got inv : wtx <HASH> have <PEER>"
        );
        assert_eq!(
            render(
                "TransactionAddedToMempool: txid=38925028e45fde071a75a7497eb215d3070f33298bda82f5a3b43cd24aa35673 wtxid=f09e953bbcd75f8531ee65ed2822ceb59ca691104cfcabaaddf98c9c57a5f928"
            ),
            "TransactionAddedToMempool : txid = <HASH> wtxid = <HASH>"
        );
        assert_eq!(
            render(
                "1 Selected 5pcdrykqq4dewrtm4ngeduhivszcmzs5rvt5icxnjmazn4c3yxta.b32.i2p:0 from tried"
            ),
            "<INT> Selected <I2P:PORT> from tried"
        );
        assert_eq!(
            render(
                "AcceptToMemoryPool: peer=9: accepted a9d6263d3e2320d1e8c742e9ce8c269f5f8b2ea4d7cefa4e2565215a14cd52d3 (wtxid=8fff3eb19a87650071a65f4dd14cb35ef41438c21dec9c068e8dc370e317b9ef) (poolsz 18829 txn, 87127 kB)"
            ),
            "AcceptToMemoryPool : <PEER> : accepted <HASH> ( wtxid = <HASH> ) ( poolsz <INT> txn , <INT> kB )"
        );
        assert_eq!(
            render(&format!(
                "UpdateTip: new best={} height=800000 progress=1.000000",
                "0".repeat(64)
            )),
            "UpdateTip : new best = <HASH> <HEIGHT> progress = <FLOAT>"
        );
        assert_eq!(
            render("- Connect 100 transactions: 12.34ms (0.123ms/tx)"),
            "- Connect <INT> transactions : <DUR> ( <DUR> / tx )"
        );
    }

    #[test]
    fn structural_punctuation_preserved_as_text() {
        // Parens split structurally; trailing commas are also split off so
        // typed values like hashes and integers are recognised.
        let toks = tokenize("(a, b)");
        let labels: Vec<&str> = toks.iter().map(|t| t.text()).collect();
        assert_eq!(labels, vec!["(", "a", ",", "b", ")"]);
    }

    #[test]
    fn trailing_comma_split_for_typed_tokens() {
        // A hash or integer immediately followed by a comma (as in bitcoind's
        // `wtxid = <hash>,` or `fees = 639,` patterns) must be recognised as
        // the typed token with a separate Text(",").
        let hash = "a".repeat(64);
        assert_eq!(render(&format!("{hash},")), "<HASH> ,");
        assert_eq!(render("fees = 639,"), "fees = <INT> ,");
        // Plain text words also get the trailing comma split off now.
        assert_eq!(render("txn,"), "txn ,");
    }

    #[test]
    fn ipv6_port_brackets_kept_whole() {
        // The atom is preserved through pass 1 so the recognizer sees it.
        assert_eq!(kinds("[2001:db8::1]:8333"), vec![TokenKind::Ipv6Port]);
    }
}
