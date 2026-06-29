//! Stamps build provenance into the binary so `/live` can report exactly which
//! commit is running. This resolves the "release executable predates Git HEAD"
//! ambiguity surfaced during the transport audit.

use std::fs;
use std::path::PathBuf;
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

    // Re-run when HEAD moves so detached-HEAD changes are noticed.
    println!("cargo:rerun-if-changed=.git/HEAD");
    // On a branch, HEAD itself usually stays unchanged while the referenced ref
    // advances. Watch that resolved ref as well so the stamped sha cannot go stale.
    if let Ok(head) = fs::read_to_string(".git/HEAD") {
        if let Some(reference) = head.trim().strip_prefix("ref: ") {
            let ref_path = Command::new("git")
                .args(["rev-parse", "--git-path", reference])
                .output()
                .ok()
                .filter(|output| output.status.success())
                .and_then(|output| String::from_utf8(output.stdout).ok())
                .map(|path| PathBuf::from(path.trim()))
                .filter(|path| !path.as_os_str().is_empty())
                .unwrap_or_else(|| PathBuf::from(".git").join(reference));
            println!("cargo:rerun-if-changed={}", ref_path.display());
        }
    }
}
