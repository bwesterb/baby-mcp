//! Embeds the version as `git describe --always` (BABY_MCP_VERSION), or the
//! package version outside a git checkout.
//!
//! No `--dirty`: it rewrites .git/index while checking, which made cargo
//! compile everything a second time on the next run.
use std::path::Path;
use std::process::Command;

fn main() {
    let v = Command::new("git")
        .args(["describe", "--always"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("v{}", env!("CARGO_PKG_VERSION")));
    println!("cargo:rustc-env=BABY_MCP_VERSION={v}");
    // Again after a commit, a checkout or a new tag. (Cargo counts a missing
    // file as changed, hence only those that exist.)
    for p in [".git/HEAD", ".git/refs", ".git/packed-refs"] {
        if Path::new(p).exists() {
            println!("cargo:rerun-if-changed={p}");
        }
    }
}
