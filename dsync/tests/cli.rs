//! Integration tests for the `ds` CLI surface: every subcommand exists
//! (including its aliases), and bad invocations fail fast. End-to-end
//! behavior is covered in `sync.rs`/`status.rs`/`barrier.rs`/`exec.rs`.

use std::process::{Command, Output};

fn ds(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ds"))
        .args(args)
        .output()
        .expect("failed to run ds")
}

#[test]
fn help_succeeds_and_lists_subcommands() {
    let out = ds(&["--help"]);
    assert!(out.status.success(), "`ds --help` should succeed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    for sub in ["sync", "status", "barrier", "exec"] {
        assert!(stdout.contains(sub), "--help should mention `{sub}`");
    }
}

#[test]
fn no_subcommand_is_a_usage_error() {
    let out = ds(&[]);
    assert!(!out.status.success(), "bare `ds` should exit non-zero");
}

#[test]
fn sync_requires_a_target() {
    let out = ds(&["sync"]);
    assert!(
        !out.status.success(),
        "`ds sync` without TARGET should fail"
    );
}

#[test]
fn sync_outside_a_repo_fails_fast() {
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_ds"))
        .args(["sync", "host:/tmp/replica"])
        .current_dir(tmp.path())
        .output()
        .expect("failed to run ds");
    assert!(!out.status.success(), "sync outside a repo should fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not inside a git repository"),
        "stderr should explain the git requirement; got: {stderr}"
    );
}

#[test]
fn status_outside_a_repo_fails_fast() {
    let tmp = tempfile::tempdir().unwrap();
    for args in [["status"], ["stat"], ["st"]] {
        let out = Command::new(env!("CARGO_BIN_EXE_ds"))
            .args(args)
            .current_dir(tmp.path())
            .output()
            .expect("failed to run ds");
        assert!(!out.status.success(), "status outside a repo should fail");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("not inside a git repository"),
            "stderr should explain the git requirement; got: {stderr}"
        );
    }
}

#[test]
fn barrier_outside_a_repo_fails_fast() {
    let tmp = tempfile::tempdir().unwrap();
    for args in [
        vec!["barrier"],
        vec!["b"],
        vec!["barrier", "--timeout", "1.5"],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_ds"))
            .args(&args)
            .current_dir(tmp.path())
            .output()
            .expect("failed to run ds");
        assert!(!out.status.success(), "barrier outside a repo should fail");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("not inside a git repository"),
            "stderr should explain the git requirement; got: {stderr}"
        );
    }
}

#[test]
fn barrier_rejects_a_negative_timeout() {
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_ds"))
        .args(["barrier", "--timeout=-1"])
        .current_dir(tmp.path())
        .output()
        .expect("failed to run ds");
    assert!(!out.status.success(), "negative timeout should be rejected");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("non-negative"),
        "stderr should explain the timeout requirement; got: {stderr}"
    );
}

#[test]
fn exec_outside_a_repo_fails_fast() {
    let tmp = tempfile::tempdir().unwrap();
    for args in [
        vec!["exec", "true"],
        vec!["x", "true"],
        vec!["exec", "--no-wait", "make", "-j4"],
        vec!["exec", "--timeout", "1.5", "true"],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_ds"))
            .args(&args)
            .current_dir(tmp.path())
            .output()
            .expect("failed to run ds");
        assert!(!out.status.success(), "exec outside a repo should fail");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("not inside a git repository"),
            "stderr should explain the git requirement; got: {stderr}"
        );
    }
}

#[test]
fn exec_requires_a_command() {
    let out = ds(&["exec"]);
    assert!(
        !out.status.success(),
        "`ds exec` without a command should fail"
    );
}

#[test]
fn exec_rejects_timeout_with_no_wait() {
    let out = ds(&["exec", "--no-wait", "--timeout", "1", "true"]);
    assert!(
        !out.status.success(),
        "`--timeout` with `--no-wait` should be a usage error"
    );
    assert_eq!(
        out.status.code(),
        Some(2),
        "conflicting flags are a clap usage error"
    );
}

#[test]
fn exec_rejects_a_negative_timeout() {
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_ds"))
        .args(["exec", "--timeout=-1", "true"])
        .current_dir(tmp.path())
        .output()
        .expect("failed to run ds");
    assert!(!out.status.success(), "negative timeout should be rejected");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("non-negative"),
        "stderr should explain the timeout requirement; got: {stderr}"
    );
}
