//! The `lt-remote` CLI; see `lt_remote::cli`.

use std::process::ExitCode;
use std::sync::atomic::AtomicBool;

fn main() -> ExitCode {
    // `serve` runs until the process is interrupted.
    let stop = AtomicBool::new(false);
    match lt_remote::cli::run(
        std::env::args().skip(1).collect(),
        &mut std::io::stdout(),
        &stop,
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("lt-remote: {e}");
            ExitCode::FAILURE
        }
    }
}
