use std::path::PathBuf;

use clap::{Parser, Subcommand};

mod merge;
mod split;
mod timestamp;

#[derive(Parser)]
#[command(version, about = "Tools for working with Bitcoin Core debug.log files")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Interleave gzipped per-node debug.log files into one timestamp-ordered log.
    ///
    /// Every file in the directory matching `debug.log-<date>-<node>.gz` is
    /// included. Each output line is prefixed with the `<node>` segment of its
    /// source filename.
    Merge {
        /// Directory containing the per-node gzipped logs.
        input_dir: PathBuf,

        /// Output path for the zstd-compressed merged log.
        #[arg(short, long)]
        output: PathBuf,

        /// zstd compression level (1 = fastest, 22 = highest ratio).
        #[arg(short = 'l', long, default_value_t = 19)]
        level: i32,
    },

    /// Split a merged log back into per-node debug.log files.
    Split {
        /// Input zstd-compressed merged log.
        input: PathBuf,

        /// Output directory for per-node files.
        #[arg(short, long)]
        output_dir: PathBuf,
    },
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Merge {
            input_dir,
            output,
            level,
        } => merge::run(&input_dir, &output, level).map_err(|e| eprintln!("merge: {e}")),
        Command::Split { input, output_dir } => {
            split::run(&input, &output_dir).map_err(|e| eprintln!("split: {e}"))
        }
    };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(()) => std::process::ExitCode::FAILURE,
    }
}
