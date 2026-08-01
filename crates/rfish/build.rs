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

    // The ISA tier the binary was built for, which `compiler` reports the way upstream
    // reports its `ARCH=`. It arrives through RUSTFLAGS rather than through a cargo
    // variable, so it is parsed out of whichever form cargo passed it in.
    let flags = std::env::var("CARGO_ENCODED_RUSTFLAGS")
        .map(|f| f.replace('\x1f', " "))
        .or_else(|_| std::env::var("RUSTFLAGS"))
        .unwrap_or_default();
    let cpu = flags
        .split_whitespace()
        .find_map(|t| t.strip_prefix("target-cpu=").map(str::to_string))
        .unwrap_or_else(|| "generic".to_string());
    println!("cargo:rustc-env=RFISH_TARGET_CPU={cpu}");
    // Nothing else in the tree affects this value, so do not rerun on every file change.
    println!("cargo:rerun-if-env-changed=RUSTFLAGS");
    println!("cargo:rerun-if-env-changed=CARGO_ENCODED_RUSTFLAGS");
    println!("cargo:rerun-if-changed=build.rs");
}
