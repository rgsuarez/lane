//! `lane` binary entry point — thin: parse args, dispatch, map errors to exit codes.
//!
//! The locking-core verbs render their own versioned JSON envelope (or human line) and
//! return a process exit code directly. `board` keeps its Slice-1 output; its read errors
//! are typed `LaneError`s surfaced through `anyhow`, so the exit-code contract still holds
//! (downcast below). Clap usage errors are handled by Clap (human-only, stderr, exit 2)
//! before this runs.
#![forbid(unsafe_code)]

use clap::Parser;

use lane::cli::{Cli, Command};
use lane::error::LaneError;

fn main() {
    let cli = Cli::parse();
    let code = match cli.command {
        Command::Board(args) => match lane::board::run_board(&args) {
            Ok(()) => 0,
            Err(err) => {
                eprintln!("lane: {err:#}");
                err.downcast_ref::<LaneError>()
                    .map(LaneError::exit_code)
                    .unwrap_or(1)
            }
        },
        Command::Claim(args) => lane::lock::run_claim(&args),
        Command::Renew(args) => lane::lock::run_renew(&args),
        Command::Handoff(args) => lane::lock::run_handoff(&args),
        Command::Release(args) => lane::lock::run_release(&args),
        Command::Status(args) => lane::lock::run_status(&args),
        Command::List(args) => lane::lock::run_list(&args),
        Command::Start(args) => lane::lifecycle::run_start(&args),
        Command::Close(args) => lane::lifecycle::run_close(&args),
    };
    std::process::exit(code);
}
