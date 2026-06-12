//! Shared integration-test harness: a temp git repository plus a temp
//! destination directory, with a running `ds sync` child that is killed on
//! drop. Drives the real binary, the real watchman, and the real rsync.
//!
//! `ds barrier` does not exist yet (Phase 3), so tests wait by polling for
//! an expected state, with a generous deadline. Once the barrier lands, the
//! harness should switch to it.

// Each test crate compiles its own copy of this module and uses a subset
// of it.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

pub const DEADLINE: Duration = Duration::from_secs(30);
pub const POLL: Duration = Duration::from_millis(25);

pub struct Harness {
    _tmp: tempfile::TempDir,
    pub repo: PathBuf,
    pub dest: PathBuf,
    pub child: Child,
    stderr_path: PathBuf,
}

impl Harness {
    pub fn new() -> Harness {
        Self::with_setup(|_repo, _dest| {})
    }

    /// Create the repo and destination, run `setup` to pre-populate them,
    /// and only then start `ds sync` — for tests whose assertions depend on
    /// state existing before the very first sync (e.g. `.gitignore` rules).
    pub fn with_setup(setup: impl FnOnce(&Path, &Path)) -> Harness {
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

    pub fn write(&self, rel: &str, contents: &str) {
        write_file(&self.repo.join(rel), contents);
    }

    pub fn dest_path(&self, rel: &str) -> PathBuf {
        self.dest.join(rel)
    }

    /// The IPC socket of the running `ds sync`.
    pub fn socket_path(&self) -> PathBuf {
        self.repo.join(".dsync/dsync.sock")
    }

    /// Run a `ds` subcommand with the repo as its working directory.
    pub fn ds(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_ds"))
            .args(args)
            .current_dir(&self.repo)
            .output()
            .expect("failed to run ds")
    }

    /// Poll until `pred` holds, or panic (with the child's stderr) at the
    /// deadline.
    pub fn wait_until(&mut self, what: &str, pred: impl Fn(&Harness) -> bool) {
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
    pub fn wait_for_file(&mut self, rel: &str, contents: &str) {
        let path = self.dest_path(rel);
        self.wait_until(&format!("{rel} to sync"), |_| {
            std::fs::read_to_string(&path).is_ok_and(|got| got == contents)
        });
    }

    /// Wait until `rel` no longer exists in the destination.
    pub fn wait_for_gone(&mut self, rel: &str) {
        let path = self.dest_path(rel);
        self.wait_until(&format!("{rel} to be deleted"), |_| !path.exists());
    }

    /// Wait until the IPC socket exists (the server holds the lock and has
    /// bound the socket).
    pub fn wait_for_socket(&mut self) {
        let path = self.socket_path();
        self.wait_until("the IPC socket to appear", |_| path.exists());
    }

    pub fn stderr(&self) -> String {
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

pub fn git(repo: &Path, args: &[&str]) {
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

pub fn write_file(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}
