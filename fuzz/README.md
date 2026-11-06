# Fuzzing

Fuzz targets for the parsers that read bytes nobody controls: log lines a
peer's data ends up in, and state files that can be corrupted on disk.

This crate is its own workspace root, so `libfuzzer-sys` stays out of the main
`Cargo.lock` and `drnlog` keeps its "no external dependencies" promise.

## Targets

Each one is a `fn(&[u8])` in `src/lib.rs` that asserts invariants, not just the
absence of a crash.

| target | what it feeds | what it asserts |
| --- | --- | --- |
| `timestamp` | bytes to `parse_timestamp` | `nanos` in range; trailing bytes don't change the key; the whole-seconds head is the same instant without the fraction |
| `line_parse` | a log line | every piece borrows from the input and `content` is the line's tail; a thread only ever comes with a category; parsing is deterministic |
| `tokenize` | line content | tokens are non-empty, in order, non-overlapping slices of the input; an atom classifies the same alone as in context |
| `ingest_line` | a log line | one line is exactly one cluster, with no `<*>` in it, and the same line always finds that cluster again |
| `state_load` | a JSONL state file | whatever loads can be saved, loaded again, and saved to the same bytes |
| `state_round_trip` | log lines to cluster, then save | the catalogue survives save → load unchanged, and saving is byte-stable |

## Running it

`cargo fuzz` needs a nightly toolchain, which the flake does not pin:

```
nix shell nixpkgs#cargo-fuzz          # or have cargo-fuzz on PATH
rustup toolchain install nightly
cargo fuzz run tokenize local/tokenize corpus/tokenize -- -max_total_time=60
```

Naming `local/<target>` first keeps the inputs the fuzzer discovers out of the
curated seed corpus; `local/` is gitignored. Drop a crash artifact into
`corpus/<target>/` to turn it into a regression check.

Inside the dev shell, the libFuzzer link step needs rustup on PATH as well,
for the linker shim the nightly toolchain was installed with.

## Without a nightly toolchain

`tests/corpus.rs` runs the same targets over the corpus on the pinned stable
toolchain, which is what CI can do:

```
cargo test --release                      # the corpus
FUZZ_ROUNDS=1000000 cargo test --release  # plus random mutations
FUZZ_TARGETS=tokenize,state_load cargo test --release
```

There is no coverage feedback, so `FUZZ_ROUNDS` is blunt next to `cargo fuzz
run` — it is there so every machine can shake the parsers, and so that each
artifact in `corpus/` is checked on every run.

## Found so far

- `Drain::insert_cluster` overflowed `next_id` on a cluster id of `u64::MAX`:
  a panic with overflow checks on, a wrap to colliding ids without.
- The JSONL reader parsed ids and counts through an `f64`, silently rounding
  anything past 2^53.
