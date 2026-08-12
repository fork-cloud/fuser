// This example requires fuse 7.40 or later. Run with:
//
//   cargo run --example passthrough_fork /tmp/foobar

#[cfg(not(target_os = "macos"))]
#[path = "passthrough_fork/fd_backed.rs"]
mod fd_backed;

#[cfg(not(target_os = "macos"))]
fn main() {
    fd_backed::run();
}

#[cfg(target_os = "macos")]
fn main() {
    eprintln!("passthrough_fork requires an fd-backed FUSE channel and is unavailable on macOS");
    std::process::exit(1);
}
