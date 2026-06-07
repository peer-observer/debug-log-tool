//! Template extraction over Bitcoin Core `debug.log`.
//!
//! Reduces each log line to its underlying template (Drain-style fixed-depth
//! tree + leaf-level similarity merge) and counts occurrences, with a
//! bitcoind-tuned tokenizer that recognises peer ids, block heights,
//! addresses, hashes, byte counts, durations, and other domain-specific
//! patterns.
//!
//! Top-level API:
//! - [`Analyzer`] — high-level entry point. Feeds raw bitcoind log lines
//!   through the pipeline and exposes templates + JSONL persistence.
//! - [`Drain`], [`Cluster`] — lower-level building blocks for callers that
//!   want to drive clustering directly over pre-tokenized input.
//! - [`tokenize`] / [`classify`] / [`Token`] — the tokenizer.
//! - [`line::parse`] / [`line::LogLine`] — bitcoind line-shape parser.

pub mod analyzer;
pub mod drain;
pub mod line;
pub mod tokenizer;

pub use analyzer::Analyzer;
pub use drain::{Cluster, Drain, Slot};
pub use line::LogLine;
pub use tokenizer::{Token, TokenKind, classify, tokenize};
