// This example requires FUSE 7.40 or later. Run with:
//
//   cargo run --example passthrough /tmp/foobar

#[cfg(not(target_os = "macos"))]
mod common;

#[cfg(not(target_os = "macos"))]
#[path = "passthrough/fd_backed.rs"]
mod fd_backed;

#[cfg(not(target_os = "macos"))]
fn main() {
    fd_backed::run();
}

#[cfg(target_os = "macos")]
fn main() {
    eprintln!("passthrough requires FUSE 7.40 and is unavailable on macOS");
    std::process::exit(1);
}
