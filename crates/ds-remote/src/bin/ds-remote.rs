//! The `ds-remote` CLI; see `ds_remote::cli`.

use std::process::ExitCode;
use std::sync::atomic::AtomicBool;

fn main() -> ExitCode {
    // `serve` runs until the process is interrupted.
    let stop = AtomicBool::new(false);
    match ds_remote::cli::run(
        std::env::args().skip(1).collect(),
        &mut std::io::stdout(),
        &stop,
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ds-remote: {e}");
            ExitCode::FAILURE
        }
    }
}
