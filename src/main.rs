//! `lane` binary entry point — thin: parse args, dispatch, map errors to exit codes.
#![forbid(unsafe_code)]

use clap::Parser;
use lane::cli::{Cli, Command};

fn main() {
    if let Err(err) = run() {
        eprintln!("lane: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Board(args) => lane::board::run_board(&args),
    }
}
