//! Build script: expose a human-friendly build version string.
//!
//! The crate version is the literal `version` in `Cargo.toml` (bumped per
//! release with `cargo-release`). For development builds that have not been
//! tagged yet, we decorate the version with the git commit so the binary
//! reports something more useful than `0.1.1` even when run from a checkout
//! that is ahead of the last tag.
//!
//! The result is surfaced as the `BUILD_VERSION` compile-time env var, which
//! clap uses for `geotop --version`.

use std::process::Command;

fn main() {
    // Re-run if the git HEAD or Cargo.toml changes.
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/tags");

    let pkg_version = env!("CARGO_PKG_VERSION");

    // Prefer `git describe --tags --always` so tagged builds report the tag
    // (e.g. `v0.1.1`) and untagged builds report the short commit hash.
    let git = Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string());

    let build_version = match git {
        Some(g) if !g.is_empty() => g,
        _ => pkg_version.to_string(),
    };

    println!("cargo:rustc-env=BUILD_VERSION={build_version}");
}