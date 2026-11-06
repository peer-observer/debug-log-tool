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
- `-j, --jobs <N>` — compression worker threads; 0 uses every core. Default 1.
- `-D, --days <N>` — days merged side by side; 0 uses every core. Default 1.
- `--before <DATE>` — only merge days dated before `DATE` (e.g. `20251116`).
  A daily job passes today so the day still being written is left alone, and
  exits successfully when nothing is old enough yet.

### Making it fast

Compression is almost all of a merge's wall time at the default level —
everything else (gzip inflate, the k-way merge, writing) is 0.6s of a 66s run
on 2.1M lines. So the question is only how much compression parallelism you
can get.

`--jobs` is the first answer, on 2.1M real lines (257 MB, three nodes, 24
cores available):

| | `-j 1` | `-j 4` | `-j 8` | `-j 24` |
| --- | --- | --- | --- | --- |
| time | 66.2s | 22.1s | 12.4s | 12.5s |
| output | 13.690 MB | 13.698 MB | 13.698 MB | 13.698 MB |

It does not scale linearly and flattens out: zstd's workers have to emit their
jobs in input order, and each job is around twice the window size (8 MiB at
level 19), so a 257 MB day only has a dozen or so jobs to spread over. A
bigger day has more: at 1 GB, `-j 8` takes 44.7s, `-j 24` 28.8s.

`--days` scales where `--jobs` flattens, because days share nothing — separate
inputs, separate output files. Three 257 MB days:

| | `-D 1` | `-D 3` |
| --- | --- | --- |
| time | 36.8s | 18.3s |

Peak memory is the real constraint past that, not cores: on a 1 GB day it is
1.0 GB at `-j 8` and 2.9 GB at `-j 24`, and `--days` multiplies it.

Level is the knob not to reach for: at `-j 1` the same input takes 0.6s at
level 1, 1.7s at 9, 6.4s at 15, 30.2s at 18 and 66.2s at 19. Level 19 costs
twice level 18 for 10% smaller output, but `--jobs` buys that back without
giving up any ratio — the merged content is identical at any job or day count,
and only the compressed framing differs.

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

## Benchmarks

`bench/` holds criterion benchmarks for the per-line work — timestamp parsing,
line parsing, tokenizing, and clustering — on real bitcoind log lines:

```
cd bench
cargo bench                        # all groups
cargo bench -- --save-baseline before
cargo bench -- --baseline before   # compare after a change
```

Like `fuzz/`, it is its own workspace root so criterion stays out of the main
lockfile and CI's `--all-targets` build.
