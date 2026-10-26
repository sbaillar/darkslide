//! Local `uniffi-bindgen` so the bindings generator version always matches the
//! `uniffi` crate the library was built with (there is no global install).
//! Usage: `cargo run -p lt-ffi --features bindgen --bin uniffi-bindgen -- generate ...`

fn main() {
    uniffi::uniffi_bindgen_main();
}
