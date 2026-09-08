//! Embeds build-identity metadata the Issue #65 capture probe prints on
//! startup. No external crates; values come from what Cargo/rustc already
//! expose plus a wall-clock stamp. (Same idiom as the #61/#63 probe build
//! scripts; this is a separate self-contained crate.)

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=I65_BUILD_EPOCH_SECS={epoch}");

    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let rustc_version = Command::new(rustc)
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=I65_RUSTC_VERSION={rustc_version}");

    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".into());
    println!("cargo:rustc-env=I65_TARGET_TRIPLE={target}");

    let git = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "nogit".into());
    println!("cargo:rustc-env=I65_GIT_SHORT={git}");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/main.rs");
}
