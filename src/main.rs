//! FS SpaceMaker command-line entry point.
//! Usage: fs-spacemaker scan --source <dir> --destination <dir> [--mode content|same-path|relocated]
//!        fs-spacemaker apply --plan <plan.json> --confirm <code> [--dry-run]

use std::process::ExitCode;

use fs_spacemaker::cli;

/// Exit code used when the command could not run at all.
const EXIT_ERROR: u8 = 1;

fn main() -> ExitCode {
    match cli::run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("ERROR: {error:#}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}
