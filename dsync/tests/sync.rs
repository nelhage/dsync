//! End-to-end tests for `ds sync` against temp git repos with local-path
//! targets. See `common/mod.rs` for the harness.

use std::process::Command;

mod common;
use common::{Harness, git, write_file};

#[test]
fn initial_sync_copies_the_tree() {
    let mut h = {
        let h = Harness::new();
        h.write("hello.txt", "hello\n");
        h.write("src/lib.rs", "pub fn x() {}\n");
        h
    };
    h.wait_for_file("hello.txt", "hello\n");
    h.wait_for_file("src/lib.rs", "pub fn x() {}\n");

    // .git is never synced.
    assert!(
        !h.dest_path(".git").exists(),
        ".git must not be synced to the destination"
    );
    // Neither is .dsync (the server creates it in the repo root).
    assert!(
        !h.dest_path(".dsync").exists(),
        ".dsync must not be synced to the destination"
    );
}

#[test]
fn incremental_changes_propagate() {
    let mut h = Harness::new();
    h.write("a.txt", "one\n");
    h.wait_for_file("a.txt", "one\n");

    // Modification.
    h.write("a.txt", "two\n");
    h.wait_for_file("a.txt", "two\n");

    // New file in a new directory.
    h.write("deep/nested/b.txt", "b\n");
    h.wait_for_file("deep/nested/b.txt", "b\n");

    // Deletion propagates (--delete).
    std::fs::remove_file(h.repo.join("a.txt")).unwrap();
    h.wait_for_gone("a.txt");
}

#[test]
fn gitignored_paths_are_skipped_and_remote_artifacts_survive() {
    // Pre-populate the destination with things that only exist remotely:
    // a build artifact under a gitignored directory, and a .git dir (as if
    // the user deliberately created a repo there). Neither may be deleted.
    // Everything is in place before `ds sync` starts, so even the very
    // first sync must honor the ignore rules.
    let mut h = Harness::with_setup(|repo, dest| {
        write_file(&dest.join("target/artifact.o"), "precious\n");
        write_file(&dest.join(".git/config"), "[core]\n");
        write_file(&repo.join(".gitignore"), "/target/\n*.log\n");
        write_file(&repo.join("tracked.txt"), "tracked\n");
        write_file(&repo.join("target/build.out"), "local build\n");
        write_file(&repo.join("debug.log"), "noise\n");
    });
    // The barrier inside wait_for_file guarantees a completed sync covers
    // everything written above — including the ignored files, which that
    // sync must have observed and skipped.
    h.wait_for_file("tracked.txt", "tracked\n");

    assert!(
        !h.dest_path("target/build.out").exists(),
        "gitignored target/build.out must not be synced"
    );
    assert!(
        !h.dest_path("debug.log").exists(),
        "gitignored *.log must not be synced"
    );
    // No --delete-excluded: remote-only ignored paths survive.
    assert_eq!(
        std::fs::read_to_string(h.dest_path("target/artifact.o")).unwrap(),
        "precious\n",
        "remote-only build artifact must survive syncs"
    );
    assert_eq!(
        std::fs::read_to_string(h.dest_path(".git/config")).unwrap(),
        "[core]\n",
        "a deliberately-created destination .git must survive syncs"
    );
}

#[test]
fn git_info_exclude_is_respected() {
    let mut h = Harness::with_setup(|repo, _dest| {
        write_file(&repo.join(".git/info/exclude"), "*.scratch\n");
        write_file(&repo.join("notes.scratch"), "private\n");
    });

    h.write("kept.txt", "kept\n");
    h.wait_for_file("kept.txt", "kept\n");

    assert!(
        !h.dest_path("notes.scratch").exists(),
        ".git/info/exclude patterns must not be synced"
    );
}

#[test]
fn sync_target_inside_repo_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);

    let out = Command::new(env!("CARGO_BIN_EXE_ds"))
        .args(["sync", "subdir"])
        .current_dir(&repo)
        .output()
        .expect("failed to run ds");
    assert!(!out.status.success(), "in-repo target should be refused");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("inside the repository"),
        "stderr should explain the refusal; got: {stderr}"
    );
}
