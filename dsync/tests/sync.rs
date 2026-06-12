//! End-to-end tests for `ds sync` against temp git repos with local-path
//! targets. These drive the real binary, the real watchman, and the real
//! rsync.
//!
//! `ds barrier` does not exist yet (Phase 3), so these tests wait by
//! polling the destination for an expected state, with a generous deadline.
//! Once the barrier lands, the harness should switch to it.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const DEADLINE: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(25);

/// A temp git repository plus a temp destination directory, with a running
/// `ds sync` child that is killed on drop.
struct Harness {
    _tmp: tempfile::TempDir,
    repo: PathBuf,
    dest: PathBuf,
    child: Child,
    stderr_path: PathBuf,
}

impl Harness {
    fn new() -> Harness {
        Self::with_setup(|_repo, _dest| {})
    }

    /// Create the repo and destination, run `setup` to pre-populate them,
    /// and only then start `ds sync` — for tests whose assertions depend on
    /// state existing before the very first sync (e.g. `.gitignore` rules).
    fn with_setup(setup: impl FnOnce(&Path, &Path)) -> Harness {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        let dest = tmp.path().join("dest");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&dest).unwrap();
        git(&repo, &["init", "-q"]);
        setup(&repo, &dest);

        let stderr_path = tmp.path().join("ds-sync.stderr");
        let stderr = std::fs::File::create(&stderr_path).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_ds"))
            .args(["sync"])
            .arg(&dest)
            .current_dir(&repo)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .env("RUST_LOG", "debug")
            // Isolate from the developer's global git config (e.g. a
            // personal core.excludesFile must not affect what syncs).
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("XDG_CONFIG_HOME", tmp.path().join("xdg"))
            .spawn()
            .expect("failed to spawn ds sync");

        Harness {
            _tmp: tmp,
            repo,
            dest,
            child,
            stderr_path,
        }
    }

    fn write(&self, rel: &str, contents: &str) {
        write_file(&self.repo.join(rel), contents);
    }

    fn dest_path(&self, rel: &str) -> PathBuf {
        self.dest.join(rel)
    }

    /// Poll until `pred` holds, or panic (with the child's stderr) at the
    /// deadline.
    fn wait_until(&mut self, what: &str, pred: impl Fn(&Harness) -> bool) {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!(
                    "ds sync exited early ({status}) while waiting for {what}\n--- ds sync stderr ---\n{}",
                    self.stderr()
                );
            }
            if pred(self) {
                return;
            }
            if start.elapsed() > DEADLINE {
                panic!(
                    "timed out waiting for {what}\n--- ds sync stderr ---\n{}",
                    self.stderr()
                );
            }
            std::thread::sleep(POLL);
        }
    }

    /// Wait until `rel` exists in the destination with exactly `contents`.
    fn wait_for_file(&mut self, rel: &str, contents: &str) {
        let path = self.dest_path(rel);
        self.wait_until(&format!("{rel} to sync"), |_| {
            std::fs::read_to_string(&path).is_ok_and(|got| got == contents)
        });
    }

    /// Wait until `rel` no longer exists in the destination.
    fn wait_for_gone(&mut self, rel: &str) {
        let path = self.dest_path(rel);
        self.wait_until(&format!("{rel} to be deleted"), |_| !path.exists());
    }

    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Best-effort cleanup of the watchman watch on the temp repo.
        let _ = Command::new("watchman")
            .arg("watch-del")
            .arg(&self.repo)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo)
        .status()
        .expect("failed to run git");
    assert!(
        status.success(),
        "git {args:?} failed in {}",
        repo.display()
    );
}

fn write_file(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

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
    h.wait_for_file("tracked.txt", "tracked\n");

    // Force one more full sync round so we know a sync that observed the
    // ignored files has completed before we assert on their absence.
    h.write("tracked2.txt", "more\n");
    h.wait_for_file("tracked2.txt", "more\n");

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

    h.write("kept2.txt", "kept2\n");
    h.wait_for_file("kept2.txt", "kept2\n");

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
