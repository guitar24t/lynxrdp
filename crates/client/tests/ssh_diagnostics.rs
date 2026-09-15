//! Exercise the CLI/GUI stderr selection in the actual client executable.
#![cfg(unix)]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

#[test]
fn gui_connection_error_carries_ssh_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("ssh");
    std::fs::write(
        &script,
        "#!/bin/sh\nprintf 'No route to host\\n' >&2\nexit 255\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_lynxrdp"))
        .args([
            "--ssh",
            script.to_str().unwrap(),
            "--tunnel-timeout",
            "2",
            "test",
        ])
        .env("LYNXRDP_GUI_ASKPASS", "1")
        .env_remove("LYNXRDP_ASKPASS")
        .env("RUST_LOG", "off")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    // The UI displays this error, not the child process's inherited stderr.
    assert!(
        stderr.lines().any(|line| line.starts_with("lynxrdp:")
            && line.contains("255")
            && line.contains("No route to host")),
        "{stderr}"
    );
}

#[test]
fn terminal_connection_keeps_stdin_and_stderr_inherited() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("ssh");
    std::fs::write(
        &script,
        "#!/bin/sh\nprintf 'Password: ' >&2\nIFS= read -r answer\n[ \"$answer\" = 'test-input' ] || exit 42\nprintf 'Authentication refused\\n' >&2\nexit 255\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_lynxrdp"))
        .args([
            "--ssh",
            script.to_str().unwrap(),
            "--tunnel-timeout",
            "2",
            "test",
        ])
        .env_remove("LYNXRDP_GUI_ASKPASS")
        .env_remove("LYNXRDP_ASKPASS")
        .env("RUST_LOG", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"test-input\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.starts_with("Password: Authentication refused\n"),
        "{stderr}"
    );
    assert!(stderr.contains("255"), "{stderr}");
}
