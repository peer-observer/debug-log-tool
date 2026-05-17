use std::path::PathBuf;

use clap::{Parser, Subcommand};

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
    /// Each input filename is expected to look like `debug.log-<date>-<node>.gz`;
    /// the `<node>` segment is used to prefix every line from that file.
    Merge {
        /// Input gzipped debug.log files (one per node, same day).
        #[arg(required = true)]
        inputs: Vec<PathBuf>,

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
        Command::Merge { .. } => {
            eprintln!("merge: not yet implemented");
            Err(())
        }
        Command::Split { .. } => {
            eprintln!("split: not yet implemented");
            Err(())
        }
    };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(()) => std::process::ExitCode::FAILURE,
    }
}
