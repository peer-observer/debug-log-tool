//! Runs every fuzz target over its corpus on the pinned stable toolchain,
//! which is what CI and a plain `nix develop` can do — `cargo fuzz` needs a
//! nightly one. Every artifact the fuzzer finds, dropped into
//! `corpus/<target>/`, becomes a permanent regression check here.
//!
//!     cargo test --release                      # corpus only
//!     FUZZ_ROUNDS=1000000 cargo test --release  # plus random mutations
//!     FUZZ_TARGETS=tokenize cargo test --release
//!
//! There is no coverage feedback, so `FUZZ_ROUNDS` is a blunt instrument
//! next to `cargo fuzz run`; it is here so that a machine without a nightly
//! toolchain can still shake the parsers.

use std::path::{Path, PathBuf};

const MAX_LEN: usize = 4096;

#[test]
fn corpus_and_mutations() {
    let rounds: u64 = env("FUZZ_ROUNDS").map_or(0, |v| number(&v));
    let seed: u64 = env("FUZZ_SEED").map_or(1, |v| number(&v));
    let wanted = env("FUZZ_TARGETS");
    let only: Vec<&str> = wanted.iter().flat_map(|w| w.split(',')).collect();

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut failures = Vec::new();
    for (name, target) in drnlog_fuzz::TARGETS {
        if !only.is_empty() && !only.contains(name) {
            continue;
        }
        let corpus = read_corpus(&root.join("corpus").join(name));
        assert!(!corpus.is_empty(), "no corpus in corpus/{name}/");

        let mut runs = 0u64;
        let mut failed = None;
        for input in &corpus {
            runs += 1;
            if let Err(input) = run(*target, input) {
                failed = Some(input);
                break;
            }
        }
        if failed.is_none() {
            let mut rng = Rng(seed ^ hash(name.as_bytes()));
            let mut buf = Vec::with_capacity(MAX_LEN);
            for _ in 0..rounds {
                mutate(&mut rng, &corpus, &mut buf);
                runs += 1;
                if let Err(input) = run(*target, &buf) {
                    failed = Some(input);
                    break;
                }
            }
        }

        match failed {
            None => println!("{name:<18} ok      {runs} inputs, {} seeds", corpus.len()),
            Some(input) => {
                let path = save_artifact(root, name, &input);
                println!("{name:<18} FAILED  after {runs} inputs");
                failures.push(format!("{name}: saved {}", path.display()));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "failing targets:\n  {}",
        failures.join("\n  ")
    );
}

/// Runs one input, handing it back on panic so the caller can save it.
fn run(target: fn(&[u8]), input: &[u8]) -> Result<(), Vec<u8>> {
    let owned = input.to_vec();
    let hushed = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let outcome = std::panic::catch_unwind(|| target(&owned));
    std::panic::set_hook(hushed);
    match outcome {
        Ok(()) => Ok(()),
        Err(payload) => {
            let message = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("panicked");
            println!("  panic: {message}");
            println!("  input: {}", String::from_utf8_lossy(input).escape_debug());
            Err(input.to_vec())
        }
    }
}

fn read_corpus(dir: &Path) -> Vec<Vec<u8>> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    paths.sort();
    paths.iter().filter_map(|p| std::fs::read(p).ok()).collect()
}

fn save_artifact(root: &Path, target: &str, input: &[u8]) -> PathBuf {
    let dir = root.join("artifacts").join(target);
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("replay-{:016x}", hash(input)));
    let _ = std::fs::write(&path, input);
    path
}

// ---------------------------------------------------------------------------
// Mutation
// ---------------------------------------------------------------------------

/// Byte sequences worth splicing in: the shapes the parsers branch on.
const INTERESTING: &[&[u8]] = &[
    b"\n",
    b"\r\n",
    b" ",
    b"\t",
    b":",
    b"=",
    b",",
    b"[",
    b"]",
    b"\\",
    b"\"",
    b"\0",
    b"\xff",
    b"peer=",
    b"height=",
    b" bytes",
    b".onion",
    b".b32.i2p",
    b"[::1]:8333",
    b"0x",
    b"2026-05-15T00:06:31.149660Z",
    b"2026-05-15T00:06:31",
    b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    br#"{"_meta":{"version":1,"depth":4,"threshold":0.5}}"#,
    br#"{"id":1,"category":"net","count":1,"first_seen":null,"last_seen":null,"template":[]}"#,
    br#"{"t":"L","v":""#,
    br#"{"t":"W","k":"PEER"}"#,
    br#"{"t":"S"}"#,
    b"\\u0041",
    b"null",
    b"18446744073709551615",
];

fn mutate(rng: &mut Rng, corpus: &[Vec<u8>], buf: &mut Vec<u8>) {
    buf.clear();
    buf.extend_from_slice(&corpus[rng.below(corpus.len() as u64) as usize]);

    for _ in 0..1 + rng.below(4) {
        match rng.below(8) {
            // Flip a bit.
            0 if !buf.is_empty() => {
                let at = rng.below(buf.len() as u64) as usize;
                buf[at] ^= 1 << rng.below(8);
            }
            // Overwrite a byte.
            1 if !buf.is_empty() => {
                let at = rng.below(buf.len() as u64) as usize;
                buf[at] = rng.below(256) as u8;
            }
            // Splice in an interesting sequence.
            2 => {
                let piece = INTERESTING[rng.below(INTERESTING.len() as u64) as usize];
                let at = rng.below(buf.len() as u64 + 1) as usize;
                buf.splice(at..at, piece.iter().copied());
            }
            // Delete a span.
            3 if !buf.is_empty() => {
                let at = rng.below(buf.len() as u64) as usize;
                let len = 1 + rng.below((buf.len() - at) as u64) as usize;
                buf.drain(at..at + len);
            }
            // Duplicate a span.
            4 if !buf.is_empty() => {
                let at = rng.below(buf.len() as u64) as usize;
                let len = 1 + rng.below((buf.len() - at) as u64) as usize;
                let span = buf[at..at + len].to_vec();
                buf.splice(at..at, span);
            }
            // Graft on part of another corpus entry.
            5 => {
                let other = &corpus[rng.below(corpus.len() as u64) as usize];
                let at = rng.below(buf.len() as u64 + 1) as usize;
                let cut = rng.below(other.len() as u64 + 1) as usize;
                buf.splice(at..at, other[..cut].iter().copied());
            }
            // Truncate.
            6 if !buf.is_empty() => {
                let keep = rng.below(buf.len() as u64) as usize;
                buf.truncate(keep);
            }
            _ => {
                let at = rng.below(buf.len() as u64 + 1) as usize;
                buf.insert(at, rng.below(256) as u8);
            }
        }
        buf.truncate(MAX_LEN);
    }
}

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
        if n == 0 { 0 } else { (self.0 >> 33) % n }
    }
}

fn hash(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Accepts `1e6` as well as `1000000`.
fn number(value: &str) -> u64 {
    match value.split_once('e') {
        Some((mantissa, exponent)) => {
            let mantissa: u64 = mantissa.parse().expect("bad number");
            let exponent: u32 = exponent.parse().expect("bad exponent");
            mantissa * 10u64.pow(exponent)
        }
        None => value.parse().expect("bad number"),
    }
}
