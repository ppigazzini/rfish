//! Record the compiler version, so `compiler` can report it.
//!
//! The UCI `compiler` command exists because a perf number is a property of the toolchain
//! as much as of the source: two builds of identical code by different compilers are not
//! comparable, and a bug report that does not say which one built the binary is missing
//! half its evidence.

use std::process::Command;

fn main() {
    let version = Command::new(std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into()))
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map_or_else(|| "unknown".to_string(), |s| s.trim().to_string());
    println!("cargo:rustc-env=RFISH_RUSTC={version}");
    // Nothing else in the tree affects this value, so do not rerun on every file change.
    println!("cargo:rerun-if-changed=build.rs");
}
