use std::env;

fn main() {
    // If we are building the `cargo-loom` binary, enable `tokio-unstable` so
    // that we can use `JoinSet`.
    let bin_name = env::var("CARGO_BIN_NAME").ok();
    if bin_name.as_deref() == Some("cargo-loom") {
        println!("cargo:rustc-cfg=tokio_unstable");
    }
}
