//! Stamps build provenance into the binary so `/live` can report exactly which
//! commit is running. This resolves the "release executable predates Git HEAD"
//! ambiguity surfaced during the transport audit.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let git_sha = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|sha| sha.trim().to_owned())
        .filter(|sha| !sha.is_empty())
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo:rustc-env=CODEWEAVE_GIT_SHA={git_sha}");

    let build_unix_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_owned());
    println!("cargo:rustc-env=CODEWEAVE_BUILD_TIME={build_unix_time}");

    // Re-run when HEAD moves so the stamped sha stays current.
    println!("cargo:rerun-if-changed=.git/HEAD");
}
