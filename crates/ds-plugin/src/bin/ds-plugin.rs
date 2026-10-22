//! `ds-plugin`: see `ds_plugin::cli`.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match ds_plugin::cli::run(&args) {
        Ok(out) => print!("{out}"),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}
