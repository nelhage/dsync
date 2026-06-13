//! Shared integration-test harness: a temp git repository plus a temp
//! destination directory, with a running `ds sync` child that is killed on
//! drop. Drives the real binary, the real watchman, and the real rsync.
//!
//! Tests wait for synchronization via `ds barrier` (dogfooding the barrier
//! mechanism — never via sleeps or sync-state polling): a barrier issued
//! after a filesystem change returns only once a completed sync covers
//! that change, so post-barrier assertions can be immediate. The only
//! polling left is for server *startup* (waiting for the IPC socket to
//! exist), which no barrier can cover.

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
        Self::build(setup, false, None)
    }

    /// A harness whose `ds sync` child sees a broken `rsync` (a stub that
    /// always fails), so no sync can ever complete — for testing timeout
    /// behavior.
    pub fn with_broken_rsync() -> Harness {
        Self::build(|_repo, _dest| {}, true, None)
    }

    /// A harness that syncs over ssh: the target is `HOST:DEST` where DEST
    /// is still the harness's local temp directory, so assertions on the
    /// destination keep working when HOST is the local machine (e.g.
    /// `localhost`). Requires non-interactive ssh to HOST.
    pub fn with_ssh_host(host: &str) -> Harness {
        Self::build(|_repo, _dest| {}, false, Some(host.to_string()))
    }

    fn build(
        setup: impl FnOnce(&Path, &Path),
        break_rsync: bool,
        ssh_host: Option<String>,
    ) -> Harness {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        let dest = tmp.path().join("dest");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&dest).unwrap();
        git(&repo, &["init", "-q"]);
        setup(&repo, &dest);

        let stderr_path = tmp.path().join("ds-sync.stderr");
        let stderr = std::fs::File::create(&stderr_path).unwrap();
        let target: std::ffi::OsString = match &ssh_host {
            Some(host) => format!("{host}:{}", dest.display()).into(),
            None => dest.clone().into_os_string(),
        };
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ds"));
        cmd.args(["sync"])
            .arg(&target)
            .current_dir(&repo)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .env("RUST_LOG", "debug")
            // Isolate from the developer's global git config (e.g. a
            // personal core.excludesFile must not affect what syncs).
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("XDG_CONFIG_HOME", tmp.path().join("xdg"));
        if break_rsync {
            // Shadow rsync (and only rsync) with an always-failing stub;
            // everything else (git, watchman) still resolves via PATH.
            let bin = tmp.path().join("broken-bin");
            std::fs::create_dir_all(&bin).unwrap();
            let stub = bin.join("rsync");
            std::fs::write(
                &stub,
                "#!/bin/sh\necho 'rsync: broken by test' >&2\nexit 1\n",
            )
            .unwrap();
            let mut perms = std::fs::metadata(&stub).unwrap().permissions();
            std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
            std::fs::set_permissions(&stub, perms).unwrap();
            let path = std::env::var_os("PATH").unwrap_or_default();
            let mut paths = vec![bin];
            paths.extend(std::env::split_paths(&path));
            cmd.env("PATH", std::env::join_paths(paths).unwrap());
        }
        let child = cmd.spawn().expect("failed to spawn ds sync");

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
    /// deadline. Only used for server *startup* (no barrier can cover it);
    /// waiting for synchronization goes through [`Harness::barrier`].
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

    /// Run `ds barrier`: returns only once a completed sync covers every
    /// change made before this call. Panics if the barrier fails or times
    /// out. "No server is running" is retried within the deadline: it is
    /// a startup condition (the child may not have bound — or, with a
    /// stale leftover socket, rebound — the socket yet), not a sync state.
    pub fn barrier(&mut self) {
        self.wait_for_socket();
        let start = Instant::now();
        loop {
            let out = self.ds(&["barrier", "--timeout", "30"]);
            if out.status.success() {
                return;
            }
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            assert!(
                stderr.contains("no ds sync is running") && start.elapsed() < DEADLINE,
                "ds barrier failed ({}): {}\n--- ds sync stderr ---\n{}",
                out.status,
                stderr,
                self.stderr()
            );
            std::thread::sleep(POLL);
        }
    }

    /// Barrier, then assert `rel` exists in the destination with exactly
    /// `contents`.
    pub fn wait_for_file(&mut self, rel: &str, contents: &str) {
        self.barrier();
        let path = self.dest_path(rel);
        match std::fs::read_to_string(&path) {
            Ok(got) if got == contents => {}
            got => panic!(
                "after a barrier, {rel} should contain {contents:?}, got {got:?}\n--- ds sync stderr ---\n{}",
                self.stderr()
            ),
        }
    }

    /// Barrier, then assert `rel` no longer exists in the destination.
    pub fn wait_for_gone(&mut self, rel: &str) {
        self.barrier();
        let path = self.dest_path(rel);
        assert!(
            !path.exists(),
            "after a barrier, {rel} should be deleted from the destination\n--- ds sync stderr ---\n{}",
            self.stderr()
        );
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

    /// How many times `needle` appears in the child's stderr so far. Used to
    /// distinguish fast-path from full syncs via their "<mode> sync finished"
    /// log lines.
    pub fn count_stderr(&self, needle: &str) -> usize {
        self.stderr().matches(needle).count()
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

/// Connect to a harness's IPC socket and exchange one raw protocol line.
pub fn raw_request(h: &Harness, line: &str) -> String {
    use std::io::{BufRead, BufReader, Write};
    let stream =
        std::os::unix::net::UnixStream::connect(h.socket_path()).expect("connect to dsync.sock");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let mut writer = stream.try_clone().unwrap();
    writeln!(writer, "{line}").unwrap();
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).unwrap();
    response.trim_end().to_string()
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
