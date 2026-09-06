//! Embeds build-identity metadata the probe prints on startup (Checkpoint 2:
//! "imprime build/version"). No external crates; values come from the
//! environment Cargo/rustc already expose plus a wall-clock stamp.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let build_epoch_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=PROBE_BUILD_EPOCH_SECS={build_epoch_secs}");

    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let rustc_version = Command::new(rustc)
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=PROBE_RUSTC_VERSION={rustc_version}");

    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".into());
    println!("cargo:rustc-env=PROBE_TARGET_TRIPLE={target}");

    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "unknown".into());
    println!("cargo:rustc-env=PROBE_PROFILE={profile}");

    // A short local source marker: git short hash if available, else "nogit".
    let git = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "nogit".into());
    println!("cargo:rustc-env=PROBE_GIT_SHORT={git}");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/main.rs");
}
