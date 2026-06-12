//! End-to-end tests for the IPC server and `ds status`: the `.dsync/`
//! socket and lock, the newline-delimited JSON protocol, and the rendered
//! status output.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::time::Duration;

mod common;
use common::{Harness, git};

/// Connect to the harness's IPC socket and exchange one raw protocol line.
fn raw_request(h: &Harness, line: &str) -> String {
    let stream = UnixStream::connect(h.socket_path()).expect("connect to dsync.sock");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let mut writer = stream.try_clone().unwrap();
    writeln!(writer, "{line}").unwrap();
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).unwrap();
    response.trim_end().to_string()
}

#[test]
fn status_reports_up_to_date_then_tracks_changes() {
    let mut h = Harness::new();
    h.write("a.txt", "one\n");
    h.wait_for_file("a.txt", "one\n");

    // The replica is up-to-date once the sync covering a.txt completes;
    // poll for it (no barrier until Phase 3).
    h.wait_until("ds status to report up-to-date", |h| {
        let out = h.ds(&["status"]);
        out.status.success() && String::from_utf8_lossy(&out.stdout).contains("up-to-date")
    });

    let out = h.ds(&["status"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let pid = h.child.id();
    let dest = h.dest.display().to_string();
    assert!(
        stdout.contains(&format!("default: pid {pid} -> {dest}:")),
        "status should report replica, pid, and target; got: {stdout}"
    );

    // More changes eventually converge back to up-to-date.
    h.write("b.txt", "two\n");
    h.wait_for_file("b.txt", "two\n");
    h.wait_until("ds status to report up-to-date again", |h| {
        let out = h.ds(&["status"]);
        out.status.success() && String::from_utf8_lossy(&out.stdout).contains("up-to-date")
    });
}

#[test]
fn status_aliases_work_against_a_live_server() {
    let mut h = Harness::new();
    h.wait_for_socket();
    for alias in ["stat", "s"] {
        let out = h.ds(&[alias]);
        assert!(
            out.status.success(),
            "`ds {alias}` should succeed; stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stdout).contains("default: pid"));
    }
}

#[test]
fn status_without_a_server_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);

    let out = Command::new(env!("CARGO_BIN_EXE_ds"))
        .args(["status"])
        .current_dir(&repo)
        .output()
        .expect("failed to run ds");
    assert!(!out.status.success(), "status without a server should fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no ds sync is running"),
        "stderr should explain that no server is running; got: {stderr}"
    );
}

#[test]
fn second_sync_in_the_same_repo_is_refused() {
    let mut h = Harness::new();
    // The lock is taken before the socket is bound, so once the socket
    // exists the lock is held.
    h.wait_for_socket();

    let other_dest = h.repo.parent().unwrap().join("dest2");
    std::fs::create_dir_all(&other_dest).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_ds"))
        .args(["sync"])
        .arg(&other_dest)
        .current_dir(&h.repo)
        .output()
        .expect("failed to run ds");
    assert!(!out.status.success(), "second ds sync should be refused");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("already running"),
        "stderr should report the running instance; got: {stderr}"
    );
}

#[test]
fn stale_socket_is_taken_over() {
    // A leftover socket (whose server died without cleaning up) must not
    // prevent a new ds sync from starting.
    let mut h = Harness::with_setup(|repo, _dest| {
        let dir = repo.join(".dsync");
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::net::UnixListener::bind(dir.join("dsync.sock")).unwrap();
        // (The listener is dropped immediately; the socket file remains.)
    });
    h.write("a.txt", "one\n");
    h.wait_for_file("a.txt", "one\n");
    h.wait_until("ds status to answer over the rebound socket", |h| {
        h.ds(&["status"]).status.success()
    });
}

#[test]
fn protocol_list_status_and_errors() {
    let mut h = Harness::new();
    h.write("a.txt", "one\n");
    h.wait_for_file("a.txt", "one\n");
    h.wait_for_socket();

    // list enumerates the live replicas.
    let resp = raw_request(&h, r#"{"version":1,"request":"list"}"#);
    assert_eq!(resp, r#"{"version":1,"ok":{"replicas":["default"]}}"#);

    // status (with the replica name defaulted) reports state, and no
    // watchman clock ever appears on the wire.
    let resp = raw_request(&h, r#"{"version":1,"request":"status"}"#);
    assert!(
        resp.contains(r#""ok":{"#) && resp.contains(r#""replica":"default""#),
        "unexpected status response: {resp}"
    );
    assert!(
        resp.contains(r#""pid":"#) && resp.contains(r#""target":"#),
        "status response should carry pid and target: {resp}"
    );
    assert!(
        !resp.to_lowercase().contains("clock"),
        "watchman clocks must never appear on the wire: {resp}"
    );

    // A version we do not speak is rejected.
    let resp = raw_request(&h, r#"{"version":2,"request":"list"}"#);
    assert!(
        resp.contains("unsupported protocol version 2"),
        "unexpected response: {resp}"
    );

    // Unknown replicas are an in-band error.
    let resp = raw_request(&h, r#"{"version":1,"request":"status","replica":"nope"}"#);
    assert!(
        resp.contains(r#"unknown replica \"nope\""#),
        "unexpected response: {resp}"
    );

    // Garbage is an in-band error, and the connection survives it.
    let stream = UnixStream::connect(h.socket_path()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    writeln!(writer, "this is not json").unwrap();
    let mut response = String::new();
    reader.read_line(&mut response).unwrap();
    assert!(
        response.contains("cannot parse request"),
        "unexpected response: {response}"
    );
    writeln!(writer, r#"{{"version":1,"request":"list"}}"#).unwrap();
    response.clear();
    reader.read_line(&mut response).unwrap();
    assert!(
        response.contains(r#""replicas":["default"]"#),
        "connection should survive a bad request; got: {response}"
    );
}
