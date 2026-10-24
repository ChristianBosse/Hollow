//! P2 as a test: every sandbox must block every escape the self-test tries.

use std::process::Command;

#[test]
fn renderer_sandbox_blocks_every_escape() {
    // `cargo test` doesn't rebuild sibling binaries that have no tests of
    // their own, and a stale child speaks an older protocol, so build them.
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut build = Command::new(cargo);
    build.args(["build", "-p", "hollow-fortress", "-p", "hollow-net"]);
    if !cfg!(debug_assertions) {
        build.arg("--release");
    }
    assert!(build.status().expect("running cargo build").success(), "building the child processes failed");

    let out = Command::new(env!("CARGO_BIN_EXE_hollow")).arg("selftest").output().expect("running hollow selftest");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "self-test failed\n--- stdout\n{stdout}\n--- stderr\n{stderr}");
    assert!(!stdout.contains("[FAIL]"), "{stdout}");
}
