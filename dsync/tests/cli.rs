//! Integration tests for the `ds` CLI skeleton: every subcommand exists
//! (including its aliases), and the unimplemented ones fail loudly.

use std::process::{Command, Output};

fn ds(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ds"))
        .args(args)
        .output()
        .expect("failed to run ds")
}

fn assert_not_implemented(args: &[&str], canonical: &str) {
    let out = ds(args);
    assert!(
        !out.status.success(),
        "`ds {}` should exit non-zero",
        args.join(" ")
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&format!("`ds {canonical}` is not implemented yet")),
        "`ds {}` stderr should report `ds {canonical}` unimplemented; got: {stderr}",
        args.join(" ")
    );
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
fn sync_is_stubbed() {
    assert_not_implemented(&["sync", "host:/tmp/replica"], "sync");
    assert_not_implemented(&["sync", "/tmp/replica"], "sync");
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
fn status_is_stubbed_with_aliases() {
    assert_not_implemented(&["status"], "status");
    assert_not_implemented(&["stat"], "status");
    assert_not_implemented(&["s"], "status");
}

#[test]
fn barrier_is_stubbed_with_alias() {
    assert_not_implemented(&["barrier"], "barrier");
    assert_not_implemented(&["b"], "barrier");
    assert_not_implemented(&["barrier", "--timeout", "1.5"], "barrier");
}

#[test]
fn exec_is_stubbed_with_alias() {
    assert_not_implemented(&["exec", "true"], "exec");
    assert_not_implemented(&["x", "true"], "exec");
    assert_not_implemented(&["exec", "--no-wait", "make", "-j4"], "exec");
}

#[test]
fn exec_requires_a_command() {
    let out = ds(&["exec"]);
    assert!(
        !out.status.success(),
        "`ds exec` without a command should fail"
    );
}
