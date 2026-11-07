//! Embeds the version as `git describe --always --dirty` (BABY_MCP_VERSION),
//! or the package version outside a git checkout.
use std::process::Command;

fn main() {
    let v = Command::new("git")
        .args(["describe", "--always", "--dirty"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("v{}", env!("CARGO_PKG_VERSION")));
    println!("cargo:rustc-env=BABY_MCP_VERSION={v}");
    // Again after a commit, a checkout or a change to a tracked file.
    for p in [".git/HEAD", ".git/index", "src", "static", "Cargo.toml"] {
        println!("cargo:rerun-if-changed={p}");
    }
}
