# debug-log-tool

Tools for working with Bitcoin Core `debug.log` files — typically the
gzip-rotated per-node logs produced by
[peer-observer](https://github.com/0xb10c/peer-observer) when running several
nodes side by side.

Per-node logs are named `debug.log-<date>-<node>.gz` (e.g.
`debug.log-20251115-alice.gz`). The `<node>` name and `<date>` are taken from
the filename.

## Build

```
cargo build --release
```

## Subcommands

### `merge`

Scan a directory for `debug.log-<date>-<node>.gz` files, group them by their
`<date>`, and write one zstd-compressed `debug.log-<date>.zst` per day into the
output directory, with each day's lines interleaved by timestamp. Every output
line is prefixed with its `<node>` name. Files that don't match the naming
pattern are skipped.

```
debug-log-tool merge -o out/ /path/to/logs/
```

- `-o, --output-dir <DIR>` — where the per-day `.zst` files are written.
- `-l, --level <N>` — zstd level, 1 (fastest) to 22 (smallest). Default 19.

#### `--check` — verify the round-trip

`--check` verifies that every input file round-trips through `merge` → `split`
byte-for-byte. It hashes each input file's decompressed content as it is read,
then — with `--output-dir` — decompresses the merged log it just wrote, runs it
back through the real `split` parser, and compares what that recovers per node.
The log is only renamed into place once it passes, so a mismatch publishes
nothing. It reports one line per input file plus a summary, and exits non-zero
if anything fails to round-trip.

With no `--output-dir` there is no file to read back, so the check digests the
merged stream as it is assembled instead. That still covers the merge and split
logic, but not the writing itself.

Verify only, writing nothing (`--output-dir` is optional here):

```
debug-log-tool merge --check /path/to/logs/
```

Compress **and** verify in the same pass — the common case when archiving a
large batch of logs and wanting assurance before deleting the originals:

```
debug-log-tool merge --check -o out/ /path/to/logs/
```

Reading the output back costs a decompression pass, which disappears next to
the compression a merge is already doing: on 600k real lines at level 19,
18.8s with `--check` against 18.6s without. Example output:

```
merge --check: writing each day, then re-reading it and splitting it back
  20251115  alice          OK    39090878 lines
  20251115  bob            OK    33594449 lines
  ...
check: 30/30 files round-trip identical
```

### `split`

Reverse of `merge`: read a `debug.log-<date>.zst` combined log and write one
`debug.log-<date>-<node>.gz` per node. The `<date>` for the output filenames
comes from the **input filename**, not the line timestamps, so a
`merge` → `split` round-trip reproduces each original per-node file
byte-for-byte.

```
debug-log-tool split -o restored/ out/debug.log-20251115.zst
```

### `templates`

Drain-style template extraction: reduce each log line to its underlying
template (recovering Bitcoin Core's compile-time format strings empirically, no
regexes) and count occurrences per category. Input compression is auto-detected
by magic bytes (zstd / gzip / plain).

```
debug-log-tool templates out/debug.log-20251115.zst
```

- `--json` — emit one cluster per line as NDJSON.
- `--load-state` / `--save-state <FILE>` — accumulate a catalogue across runs
  (atomic JSONL persistence).
- `--category <CAT>` (repeatable), `--top <N>`, `--min-count <N>` — filter and
  trim the output.

## Streaming

Both `merge` and `split` stream: `merge` is a k-way merge holding at most one
pending line per input, and `split` dispatches line by line. Neither loads a
whole log into memory, so they work on logs far larger than RAM.

## Fuzzing

`fuzz/` holds libFuzzer targets for the parsers that read bytes nobody
controls — log lines and state files — plus a corpus test that runs them on the
pinned toolchain. See [fuzz/README.md](fuzz/README.md).
