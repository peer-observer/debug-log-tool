//! Parse a single `debug.log` line into its structural pieces.
//!
//! Bitcoin Core lines look like:
//!
//! ```text
//! 2026-06-06T12:34:56Z [net] received: ping (8 bytes) peer=3
//! 2026-06-06T12:34:56Z [thread] [net] connect: 192.0.2.1:8333
//! 2026-06-06T12:34:56Z UpdateTip: …                     ← no [category]
//!   continuation line without leading timestamp           ← inherits ctx
//! ```
//!
//! Up to two consecutive `[…]` groups are recognised. Lines that don't
//! start with a timestamp fall through with `timestamp = None` and the
//! whole line in `content` — callers can carry forward the previous
//! line's category if they want.
//!
//! Never fails: garbage lines pass through with `content` set so the
//! analyzer clusters them as their own templates.

/// One parsed bitcoind log line. All fields borrow from the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogLine<'a> {
    pub timestamp: Option<&'a str>,
    pub thread: Option<&'a str>,
    pub category: Option<&'a str>,
    pub content: &'a str,
}

/// Parse one line. Trailing `\r` / `\n` is stripped from `content`.
pub fn parse(line: &str) -> LogLine<'_> {
    let line = line.trim_end_matches(['\r', '\n']);

    // Continuation line: no timestamp prefix — treat the whole line as
    // content, leading whitespace preserved (so multi-line stack traces
    // keep their indentation when re-emitted).
    if !looks_like_timestamp_start(line) {
        return LogLine {
            timestamp: None,
            thread: None,
            category: None,
            content: line,
        };
    }

    // Timestamp = up to the first whitespace.
    let (ts, rest) = match line.find(|c: char| c.is_ascii_whitespace()) {
        Some(idx) => (&line[..idx], line[idx..].trim_start()),
        None => (line, ""),
    };

    // Up to two `[…]` groups.
    let (first, after_first) = take_bracket_group(rest);
    let (second, after_second) = match first {
        Some(_) => take_bracket_group(after_first),
        None => (None, after_first),
    };

    let (thread, category, content) = match (first, second) {
        (Some(a), Some(b)) => (Some(a), Some(b), after_second),
        (Some(a), None) => (None, Some(a), after_first),
        (None, _) => (None, None, rest),
    };

    LogLine {
        timestamp: Some(ts),
        thread,
        category,
        content,
    }
}

fn looks_like_timestamp_start(line: &str) -> bool {
    // Cheap heuristic: starts with 4 digits then '-'. Enough to
    // distinguish "2026-06-06T…" from continuation/garbage lines without
    // pulling in the full timestamp grammar (which lives in the CLI crate).
    let b = line.as_bytes();
    b.len() >= 5 && b[..4].iter().all(|c| c.is_ascii_digit()) && b[4] == b'-'
}

/// If `s` starts with `[xxx]` (no embedded `]`), return `(Some(xxx),
/// remainder-with-leading-whitespace-stripped)`. Otherwise `(None, s)`.
fn take_bracket_group(s: &str) -> (Option<&str>, &str) {
    let s = s.trim_start();
    let rest = match s.strip_prefix('[') {
        Some(r) => r,
        None => return (None, s),
    };
    let end = match rest.find(']') {
        Some(idx) => idx,
        None => return (None, s),
    };
    let inner = &rest[..end];
    // Guard against `[…]` accidentally swallowing IPv6-port atoms in the
    // content. Reject brackets that contain whitespace OR colons — neither
    // appears in a bitcoind `[category]` tag.
    if inner.contains(|c: char| c.is_ascii_whitespace() || c == ':') {
        return (None, s);
    }
    let after = rest[end + 1..].trim_start();
    (Some(inner), after)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ts_plus_category_plus_content() {
        let l = parse("2026-06-06T12:34:56Z [net] received: ping (8 bytes) peer=3");
        assert_eq!(l.timestamp, Some("2026-06-06T12:34:56Z"));
        assert_eq!(l.thread, None);
        assert_eq!(l.category, Some("net"));
        assert_eq!(l.content, "received: ping (8 bytes) peer=3");
    }

    #[test]
    fn ts_plus_thread_plus_category() {
        let l = parse("2026-06-06T12:34:56Z [msghand] [net] processing block");
        assert_eq!(l.thread, Some("msghand"));
        assert_eq!(l.category, Some("net"));
        assert_eq!(l.content, "processing block");
    }

    #[test]
    fn ts_only_no_category() {
        let l = parse("2026-06-06T12:34:56Z UpdateTip: new best=0");
        assert_eq!(l.timestamp, Some("2026-06-06T12:34:56Z"));
        assert_eq!(l.category, None);
        assert_eq!(l.content, "UpdateTip: new best=0");
    }

    #[test]
    fn fractional_timestamp() {
        let l = parse("2026-06-06T12:34:56.123456Z [net] hi");
        assert_eq!(l.timestamp, Some("2026-06-06T12:34:56.123456Z"));
        assert_eq!(l.category, Some("net"));
        assert_eq!(l.content, "hi");
    }

    #[test]
    fn continuation_line_no_timestamp() {
        let l = parse("\tat method (file.cpp:123)");
        assert_eq!(l.timestamp, None);
        assert_eq!(l.thread, None);
        assert_eq!(l.category, None);
        assert_eq!(l.content, "\tat method (file.cpp:123)");
    }

    #[test]
    fn garbage_line_passes_through() {
        let l = parse("???nonsense???");
        assert_eq!(l.timestamp, None);
        assert_eq!(l.content, "???nonsense???");
    }

    #[test]
    fn trailing_newline_stripped() {
        let l = parse("2026-06-06T12:34:56Z [net] hi\r\n");
        assert_eq!(l.content, "hi");
    }

    #[test]
    fn ipv6_in_content_not_consumed_as_bracket_group() {
        // The recognizer guard means an IPv6-port atom further into the
        // line stays in `content` instead of being misparsed as a category.
        let l = parse("2026-06-06T12:34:56Z connect: [2001:db8::1]:8333");
        assert_eq!(l.category, None);
        assert_eq!(l.content, "connect: [2001:db8::1]:8333");
    }
}
