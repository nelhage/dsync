//! End-to-end tests for `ds barrier`: up-to-date-as-of-invocation
//! semantics, the timeout exit code, and the wire protocol.

use std::process::Command;

mod common;
use common::{Harness, git, raw_request};

/// The distinct exit code for a barrier that timed out (see
//  `barrier::TIMEOUT_EXIT_CODE`).
const TIMEOUT_EXIT_CODE: i32 = 3;

#[test]
fn barrier_covers_changes_made_before_it() {
    let mut h = Harness::new();
    // Repeatedly: change a file, barrier, and immediately assert the
    // destination — a barrier returns only once a completed sync covers
    // everything that changed before it was issued.
    for i in 0..5 {
        let contents = format!("revision {i}\n");
        h.write("a.txt", &contents);
        h.write(&format!("dir{i}/file.txt"), &contents);
        h.barrier();
        assert_eq!(
            std::fs::read_to_string(h.dest_path("a.txt")).unwrap(),
            contents,
            "iteration {i}: a.txt must be synced once the barrier returns"
        );
        assert_eq!(
            std::fs::read_to_string(h.dest_path(&format!("dir{i}/file.txt"))).unwrap(),
            contents,
            "iteration {i}: dir{i}/file.txt must be synced once the barrier returns"
        );
    }

    // Deletions are covered too.
    std::fs::remove_file(h.repo.join("a.txt")).unwrap();
    h.barrier();
    assert!(!h.dest_path("a.txt").exists());
}

#[test]
fn barrier_with_nothing_pending_returns_promptly_with_alias() {
    let mut h = Harness::new();
    h.write("a.txt", "one\n");
    h.barrier();
    // An already-covered barrier replies immediately; exercise the alias
    // and a timeout that must not be consumed.
    let out = h.ds(&["b", "--timeout", "30"]);
    assert!(
        out.status.success(),
        "`ds b` should succeed; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn barrier_without_a_server_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);

    let out = Command::new(env!("CARGO_BIN_EXE_ds"))
        .args(["barrier"])
        .current_dir(&repo)
        .output()
        .expect("failed to run ds");
    assert!(
        !out.status.success(),
        "barrier without a server should fail"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no ds sync is running"),
        "stderr should explain that no server is running; got: {stderr}"
    );
}

#[test]
fn barrier_timeout_has_a_distinct_exit_code() {
    // rsync is broken, so no sync can ever complete and the barrier can
    // only time out.
    let mut h = Harness::with_broken_rsync();
    h.write("a.txt", "one\n");
    h.wait_for_socket();

    let out = h.ds(&["barrier", "--timeout", "0.5"]);
    assert_eq!(
        out.status.code(),
        Some(TIMEOUT_EXIT_CODE),
        "a timed-out barrier should exit with the distinct code; stderr: {}\n--- ds sync stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr),
        h.stderr()
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("timed out"),
        "stderr should say the barrier timed out; got: {stderr}"
    );
}

#[test]
fn barrier_protocol_reports_state_without_clocks() {
    let mut h = Harness::new();
    h.write("a.txt", "one\n");
    h.barrier();

    // A bare request: replica defaults, no timeout. The reply reports
    // state — the target seq and the covering sync — never clocks and
    // never booleans.
    let resp = raw_request(&h, r#"{"version":1,"request":"barrier"}"#);
    assert!(
        resp.contains(r#""ok":{"#) && resp.contains(r#""replica":"default""#),
        "unexpected barrier response: {resp}"
    );
    assert!(
        resp.contains(r#""target_seq":"#),
        "barrier response should carry the target seq: {resp}"
    );
    assert!(
        !resp.to_lowercase().contains("clock"),
        "watchman clocks must never appear on the wire: {resp}"
    );

    // Unknown replicas are an in-band error.
    let resp = raw_request(&h, r#"{"version":1,"request":"barrier","replica":"nope"}"#);
    assert!(
        resp.contains(r#"unknown replica \"nope\""#),
        "unexpected response: {resp}"
    );

    // Invalid timeouts are an in-band error.
    let resp = raw_request(&h, r#"{"version":1,"request":"barrier","timeout":-1.0}"#);
    assert!(
        resp.contains("invalid barrier timeout"),
        "unexpected response: {resp}"
    );
}
