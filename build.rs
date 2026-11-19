//! Records which commit Fizz was built from and where its source lives, so
//! the app can tell when a newer version has been pushed and update itself.

use std::process::Command;

fn main() {
    let source = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let commit = Command::new("git")
        .args(["-C", &source, "rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default();
    println!("cargo:rustc-env=FIZZ_COMMIT={commit}");
    println!("cargo:rustc-env=FIZZ_SOURCE={source}");
    // Rebuild when a commit is made or pulled, so the recorded commit is right.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads");
    println!("cargo:rerun-if-changed=.git/packed-refs");
}
