use std::path::PathBuf;

use clap::{Parser, Subcommand};

mod merge;
mod split;
mod templates;
mod timestamp;

#[derive(Parser)]
#[command(version, about = "Tools for working with Bitcoin Core debug.log files")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Interleave gzipped per-node debug.log files into one timestamp-ordered
    /// log per day.
    ///
    /// Every file in the directory matching `debug.log-<date>-<node>.gz` is
    /// included. Files are grouped by their `<date>` segment, and each group is
    /// merged into `debug.log-<date>.zst` in the output directory. Each output
    /// line is prefixed with the `<node>` segment of its source filename.
    Merge {
        /// Directory containing the per-node gzipped logs.
        input_dir: PathBuf,

        /// Output directory for the per-day zstd-compressed merged logs.
        /// Required unless `--check` is given.
        #[arg(short, long = "output-dir")]
        output_dir: Option<PathBuf>,

        /// zstd compression level (1 = fastest, 22 = highest ratio).
        #[arg(short = 'l', long, default_value_t = 19)]
        level: i32,

        /// Number of zstd compression worker threads; 1 disables multithreaded
        /// compression. Input files are always decompressed and parsed on one
        /// thread each, regardless of this setting.
        #[arg(short = 'j', long, default_value_t = default_jobs())]
        jobs: u32,

        /// Verify the merge → split round-trip by comparing a content hash of
        /// each input file against a hash of what `split` reconstructs. Exits
        /// non-zero if any file fails to round-trip. Combine with --output-dir
        /// to write the merged output and verify it in the same pass; on its
        /// own it verifies without writing anything.
        #[arg(long)]
        check: bool,
    },

    /// Split a merged log back into per-node debug.log files.
    ///
    /// The `<date>` for the output filenames is taken from the input filename
    /// (`debug.log-<date>.zst`), so a `merge` → `split` round-trip reproduces
    /// each original `debug.log-<date>-<node>.gz` byte-for-byte.
    Split {
        /// Input merged log, named `debug.log-<date>.zst`.
        input: PathBuf,

        /// Output directory for per-node files.
        #[arg(short, long)]
        output_dir: PathBuf,
    },

    /// Extract templates from a debug.log via Drain-style clustering.
    ///
    /// Input is autodetected by magic bytes (zstd / gzip / plain). For a
    /// merged `.zst` log the per-line `<node> ` prefix is stripped before
    /// clustering. Group by category, sorted by count descending.
    Templates {
        /// Path to the log (omit when only printing `--load-state`).
        input: Option<PathBuf>,

        /// Drain tree depth.
        #[arg(long, default_value_t = 4)]
        depth: usize,

        /// Drain similarity threshold (0..=1).
        #[arg(long, default_value_t = 0.5)]
        threshold: f64,

        /// Drop templates with count below this.
        #[arg(long, default_value_t = 1)]
        min_count: u64,

        /// Restrict output to these categories (repeatable).
        #[arg(long = "category")]
        categories: Vec<String>,

        /// Only print the top N templates per category.
        #[arg(long)]
        top: Option<usize>,

        /// Resume from a previously saved JSONL state file.
        #[arg(long = "load-state")]
        load_state: Option<PathBuf>,

        /// Atomically write updated JSONL state after processing.
        #[arg(long = "save-state")]
        save_state: Option<PathBuf>,

        /// Emit one cluster per line as JSON instead of grouped text.
        #[arg(long)]
        json: bool,
    },
}

fn default_jobs() -> u32 {
    std::thread::available_parallelism().map_or(1, |n| n.get() as u32)
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Merge {
            input_dir,
            output_dir,
            level,
            jobs,
            check,
        } => merge::run(&input_dir, output_dir.as_deref(), level, jobs, check)
            .map_err(|e| eprintln!("merge: {e}")),
        Command::Split { input, output_dir } => {
            split::run(&input, &output_dir).map_err(|e| eprintln!("split: {e}"))
        }
        Command::Templates {
            input,
            depth,
            threshold,
            min_count,
            categories,
            top,
            load_state,
            save_state,
            json,
        } => templates::run(
            input.as_deref(),
            templates::TemplatesOpts {
                depth,
                threshold,
                min_count,
                categories,
                top,
                load_state,
                save_state,
                json,
            },
        )
        .map_err(|e| eprintln!("templates: {e}")),
    };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(()) => std::process::ExitCode::FAILURE,
    }
}
