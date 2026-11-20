//! Isolated process regressions: no Docker daemon, GPU, or production storage.
#![cfg(unix)]
use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};
fn executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}
#[test]
fn cli_failures_return_nonzero() {
    let root = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ferryman"))
        .args(["--no-cache", "--input"])
        .arg(root.path().join("missing.txt"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("1 failed"));
}
#[test]
fn unhealthy_cli_container_is_removed() {
    let root = tempfile::tempdir().unwrap();
    executable(&root.path().join("docker"), "#!/bin/sh\ncase \"$1\" in\nrun) touch \"$FIXTURE_ROOT/running\";;\nrm) rm -f \"$FIXTURE_ROOT/running\";;\ninspect) echo true;;\nesac\n");
    fs::write(root.path().join("source.txt"), "Hello").unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_ferryman"))
        .env(
            "PATH",
            format!(
                "{}:{}",
                root.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("FIXTURE_ROOT", root.path())
        .args([
            "--serve",
            "--host-port",
            "0",
            "--health-timeout",
            "0",
            "--input",
        ])
        .arg(root.path().join("source.txt"))
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(!root.path().join("running").exists());
}
