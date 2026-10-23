//! `darkslide-rawler`: answers Darkslide's requests with the LGPL `rawler` decoder, so that
//! Darkslide itself doesn't link it. The protocol is `ds_raw::helper`'s; this program only
//! moves bytes: the command line and stdin in, the answer out.

use std::io::Write;
use std::os::fd::AsFd;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(
        args.first().map(String::as_str),
        None | Some("-h" | "--help")
    ) {
        eprintln!(
            "darkslide-rawler: the rawler decoder for Darkslide (not run by hand).\n\
             Darkslide runs it with a request on the command line and the input on stdin."
        );
        return ExitCode::from(2);
    }
    let input = match std::io::stdin()
        .as_fd()
        .try_clone_to_owned()
        .map(std::fs::File::from)
        .and_then(ds_raw::helper::read_input)
    {
        Ok(v) => v,
        Err(e) => {
            eprintln!("darkslide-rawler: can't read stdin: {e}");
            return ExitCode::FAILURE;
        }
    };
    let out = ds_raw::helper::serve(&args, &input);
    match std::io::stdout().lock().write_all(&out) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}
