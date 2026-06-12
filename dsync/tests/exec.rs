//! Integration tests for `ds exec`: target discovery via the socket, the
//! implicit barrier, CWD = the replica for local-path targets, exit-code
//! propagation, `--no-wait`, and `--timeout`.
//!
//! The ssh path needs a reachable host; it runs only when `DSYNC_TEST_SSH`
//! names one (e.g. `DSYNC_TEST_SSH=localhost`) with non-interactive auth,
//! so CI without ssh skips it.

mod common;

use std::process::Command;

use common::Harness;

/// `ds barrier`'s (and `ds exec`'s) distinct timed-out exit code.
const TIMEOUT_EXIT_CODE: i32 = 3;

#[test]
fn exec_runs_in_the_replica_cwd() {
    let mut h = Harness::new();
    h.wait_for_socket();
    let out = h.ds(&["exec", "sh", "-c", "pwd"]);
    assert!(
        out.status.success(),
        "ds exec failed: {}\n--- ds sync stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr),
        h.stderr()
    );
    let got = String::from_utf8_lossy(&out.stdout);
    let want = h.dest.canonicalize().unwrap();
    assert_eq!(got.trim_end(), want.to_str().unwrap());
}

#[test]
fn exec_barriers_before_running() {
    let mut h = Harness::new();
    h.wait_for_socket();
    // Written immediately before exec: only the implicit barrier
    // guarantees the replica already has it when the command runs.
    h.write("greeting.txt", "hello from the repo\n");
    let out = h.ds(&["exec", "cat", "greeting.txt"]);
    assert!(
        out.status.success(),
        "ds exec failed: {}\n--- ds sync stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr),
        h.stderr()
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "hello from the repo\n"
    );
}

#[test]
fn exec_propagates_the_exit_code() {
    let mut h = Harness::new();
    h.wait_for_socket();
    let out = h.ds(&["exec", "sh", "-c", "exit 7"]);
    assert_eq!(out.status.code(), Some(7), "exit code should propagate");

    // A missing command exits 127, like a shell.
    let out = h.ds(&["exec", "definitely-no-such-command-dsync"]);
    assert_eq!(out.status.code(), Some(127));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cannot execute"),
        "stderr should explain the exec failure; got: {stderr}"
    );
}

#[test]
fn exec_passes_argv_verbatim() {
    let mut h = Harness::new();
    h.wait_for_socket();
    let tricky = "it's $HOME `a b`;*";
    let out = h.ds(&["exec", "printf", "%s", tricky]);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), tricky);
}

#[test]
fn exec_alias_works() {
    let mut h = Harness::new();
    h.wait_for_socket();
    let out = h.ds(&["x", "true"]);
    assert!(
        out.status.success(),
        "ds x failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn exec_no_wait_skips_the_barrier() {
    // rsync is broken: no sync can ever complete, so only --no-wait can
    // let the command run.
    let mut h = Harness::with_broken_rsync();
    h.wait_for_socket();
    let out = h.ds(&["exec", "--no-wait", "sh", "-c", "echo ran"]);
    assert!(
        out.status.success(),
        "ds exec --no-wait failed: {}\n--- ds sync stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr),
        h.stderr()
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ran\n");
}

#[test]
fn exec_timeout_exits_3_without_running() {
    let mut h = Harness::with_broken_rsync();
    h.wait_for_socket();
    let out = h.ds(&["exec", "--timeout", "0.2", "sh", "-c", "echo ran"]);
    assert_eq!(
        out.status.code(),
        Some(TIMEOUT_EXIT_CODE),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "",
        "the command must not run when the barrier times out"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("timed out waiting for sync"),
        "stderr should say the wait timed out; got: {stderr}"
    );
}

#[test]
fn exec_without_a_server_fails_clearly() {
    let tmp = tempfile::tempdir().unwrap();
    common::git(tmp.path(), &["init", "-q"]);
    let out = Command::new(env!("CARGO_BIN_EXE_ds"))
        .args(["exec", "true"])
        .current_dir(tmp.path())
        .output()
        .expect("failed to run ds");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no ds sync is running"),
        "stderr should say no sync is running; got: {stderr}"
    );
}

/// The remote (ssh) path, end-to-end: discovery reports `HOST:PATH`, the
/// command runs on HOST in PATH via `cd`-and-`exec`, quoting survives the
/// remote shell, and the remote exit code propagates. Gated on
/// `DSYNC_TEST_SSH` naming a host (with non-interactive auth) whose
/// filesystem is this machine's — e.g. `localhost`.
#[test]
fn exec_over_ssh() {
    let Ok(host) = std::env::var("DSYNC_TEST_SSH") else {
        eprintln!("skipping: set DSYNC_TEST_SSH=localhost (or another host) to run");
        return;
    };
    let mut h = Harness::with_ssh_host(&host);
    h.write("greeting.txt", "hello over ssh\n");
    h.wait_for_file("greeting.txt", "hello over ssh\n");

    // CWD is the replica, reached via the remote `cd`.
    let out = h.ds(&["exec", "cat", "greeting.txt"]);
    assert!(
        out.status.success(),
        "ds exec over ssh failed: {}\n--- ds sync stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr),
        h.stderr()
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hello over ssh\n");

    // Quoting survives the remote shell.
    let tricky = "it's $HOME `a b`;*";
    let out = h.ds(&["exec", "printf", "%s", tricky]);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), tricky);

    // The remote exit code propagates.
    let out = h.ds(&["exec", "sh", "-c", "exit 9"]);
    assert_eq!(out.status.code(), Some(9));
}
