//! End-to-end tests for the small-change fast path (Phase 6). The harness
//! drives the real binary against a local-path target; the `ds sync` child
//! logs `"<mode> sync finished"` at debug level, so we can tell a fast-path
//! sync from a full rsync by scraping its stderr.
//!
//! The startup sync is always a full rsync (a watchman fresh instance), so
//! each test first drives one sync to completion, then asserts the *next*
//! sync's mode.

mod common;
use common::{Harness, write_file};

/// A barrier-covered sync settles before we read the mode counters.
fn fast_count(h: &Harness) -> usize {
    h.count_stderr("fast sync finished")
}
fn full_count(h: &Harness) -> usize {
    h.count_stderr("full sync finished")
}

#[test]
fn small_change_uses_the_fast_path() {
    let mut h = Harness::new();
    h.write("a.txt", "one\n");
    h.wait_for_file("a.txt", "one\n"); // startup: full rsync

    let fast_before = fast_count(&h);
    h.write("a.txt", "two\n");
    h.wait_for_file("a.txt", "two\n");
    assert!(
        fast_count(&h) > fast_before,
        "a small modification should use the fast path\n--- stderr ---\n{}",
        h.stderr()
    );
}

#[test]
fn fast_path_creates_modifies_and_deletes() {
    let mut h = Harness::new();
    h.write("seed.txt", "seed\n");
    h.wait_for_file("seed.txt", "seed\n"); // startup: full rsync

    let fast_before = fast_count(&h);

    // Create a file in a new directory.
    h.write("deep/nested/b.txt", "b\n");
    h.wait_for_file("deep/nested/b.txt", "b\n");

    // Modify it.
    h.write("deep/nested/b.txt", "bb\n");
    h.wait_for_file("deep/nested/b.txt", "bb\n");

    // Delete it: the fast path streams a deletion list, not a tarball.
    std::fs::remove_file(h.repo.join("deep/nested/b.txt")).unwrap();
    h.wait_for_gone("deep/nested/b.txt");

    // All three were handled without a full rsync.
    assert_eq!(
        full_count(&h),
        1,
        "only the startup sync should have been a full rsync\n--- stderr ---\n{}",
        h.stderr()
    );
    assert!(
        fast_count(&h) >= fast_before + 3,
        "create/modify/delete should each use the fast path\n--- stderr ---\n{}",
        h.stderr()
    );
}

#[test]
fn fast_path_skips_ignored_files_and_keeps_remote_artifacts() {
    let mut h = Harness::with_setup(|repo, dest| {
        // A remote-only build artifact under a gitignored directory.
        write_file(&dest.join("target/artifact.o"), "precious\n");
        write_file(&repo.join(".gitignore"), "/target/\n*.log\n");
        write_file(&repo.join("tracked.txt"), "v1\n");
    });
    h.wait_for_file("tracked.txt", "v1\n"); // startup: full rsync

    let full_before = full_count(&h);

    // Change an ignored file and a tracked file together; barrier on the
    // tracked one.
    h.write("target/build.out", "local build\n");
    h.write("debug.log", "noise\n");
    h.write("tracked.txt", "v2\n");
    h.wait_for_file("tracked.txt", "v2\n");

    // The tracked change went over the fast path (no new full rsync)...
    assert_eq!(
        full_count(&h),
        full_before,
        "an ignored-file change must not force a full rsync\n--- stderr ---\n{}",
        h.stderr()
    );
    // ...and the ignored files were never sent.
    assert!(
        !h.dest_path("target/build.out").exists(),
        "gitignored target/build.out must not be fast-synced"
    );
    assert!(
        !h.dest_path("debug.log").exists(),
        "gitignored *.log must not be fast-synced"
    );
    // The remote-only artifact survives: the fast path never deletes ignored
    // paths.
    assert_eq!(
        std::fs::read_to_string(h.dest_path("target/artifact.o")).unwrap(),
        "precious\n",
        "remote-only build artifact must survive a fast-path sync"
    );
}

#[test]
fn oversized_change_falls_back_to_full_rsync() {
    let mut h = Harness::new();
    h.write("seed.txt", "seed\n");
    h.wait_for_file("seed.txt", "seed\n"); // startup: full rsync

    let full_before = full_count(&h);

    // A single file past the fast-path byte budget (8 MiB): the change is
    // one file (so it passes the file-count gate and the fast path is
    // attempted), but the post-query byte budget trips the correctness valve
    // and we fall back to a full rsync.
    let big = "x".repeat(9 * 1024 * 1024);
    h.write("big.bin", &big);
    h.wait_for_file("big.bin", &big);

    assert!(
        full_count(&h) > full_before,
        "an oversized change should fall back to a full rsync\n--- stderr ---\n{}",
        h.stderr()
    );
}
