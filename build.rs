fn main() {
    // If we are building the `cargo-loom` binary, enable `tokio-unstable` so
    // that we can use `JoinSet`.
    println!("cargo:rustc-cfg=tokio_unstable");
}
