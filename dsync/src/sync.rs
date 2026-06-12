//! The core sync loop: watch the repository with watchman and propagate
//! changes to the target with full-tree rsync runs.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use tokio::sync::watch;
use tracing::{debug, error, info, warn};
use watchman_client::SubscriptionData;
use watchman_client::prelude::*;

use crate::target::Target;

/// How long to let the filesystem settle after a change notification before
/// starting a sync, coalescing bursts of events into one rsync run.
const SETTLE_WINDOW: Duration = Duration::from_millis(75);

/// How long to wait before retrying after a failed sync.
const RETRY_DELAY: Duration = Duration::from_secs(2);

/// The record of a completed sync: the watchman clock it covers (opaque;
/// ordered only by `seq`, the receipt-order sequence number assigned when
/// the clock arrived over our watchman connection) and when it finished.
#[derive(Debug, Clone)]
pub struct SyncedClock {
    pub seq: u64,
    #[allow(dead_code)] // Consumed by `ds status`/`ds barrier` in later phases.
    pub clock: Clock,
    #[allow(dead_code)]
    pub completed_at: SystemTime,
}

/// A change notification from watchman, tagged with its receipt-order
/// sequence number.
#[derive(Debug, Clone)]
struct ChangeEvent {
    seq: u64,
    clock: Clock,
    files: Option<usize>,
}

/// Run `ds sync`: subscribe to watchman and rsync the repository to
/// `target` on every (settled) change, forever.
pub async fn run(repo_root: PathBuf, target: Target) -> Result<()> {
    let client = Connector::new()
        .connect()
        .await
        .context("cannot connect to watchman (watchman is required for ds sync)")?;
    let resolved = client
        .resolve_root(CanonicalPath::canonicalize(&repo_root)?)
        .await
        .with_context(|| format!("watchman cannot watch {}", repo_root.display()))?;
    // The default subscription immediately delivers a fresh-instance
    // notification covering the whole tree, which drives the startup sync.
    let (mut subscription, _response) = client
        .subscribe::<NameOnly>(&resolved, SubscribeRequest::default())
        .await
        .context("watchman subscribe failed")?;
    info!("watching {} -> {}", repo_root.display(), target);

    // Latest-value channel from the watchman reader to the sync runner: the
    // runner always syncs the newest observed event, so any number of
    // changes arriving mid-sync coalesce into at most one pending follow-up.
    let (tx, mut rx) = watch::channel::<Option<ChangeEvent>>(None);

    let reader = async move {
        let mut seq: u64 = 0;
        loop {
            match subscription.next().await? {
                SubscriptionData::FilesChanged(result) => {
                    seq += 1;
                    if result.is_fresh_instance {
                        info!(
                            seq,
                            "watchman reports a fresh instance; scheduling full sync"
                        );
                    }
                    let event = ChangeEvent {
                        seq,
                        clock: result.clock,
                        files: result.files.as_ref().map(|f| f.len()),
                    };
                    debug!(seq, files = ?event.files, "change notification");
                    tracing::trace!(
                        seq,
                        files = ?result.files.as_ref().map(|fs| {
                            fs.iter().map(|f| f.name.display().to_string()).collect::<Vec<_>>()
                        }),
                        "changed files"
                    );
                    if tx.send(Some(event)).is_err() {
                        // The runner is gone; we are being torn down.
                        return Ok(());
                    }
                }
                SubscriptionData::Canceled => {
                    bail!("watchman canceled our subscription (was the watch deleted?)");
                }
                SubscriptionData::StateEnter { .. } | SubscriptionData::StateLeave { .. } => {}
            }
        }
    };

    let runner = async move {
        let mut last_synced: Option<SyncedClock> = None;
        let mut retrying = false;
        loop {
            if retrying {
                tokio::time::sleep(RETRY_DELAY).await;
            } else {
                if rx.changed().await.is_err() {
                    bail!("watchman event stream ended");
                }
                tokio::time::sleep(SETTLE_WINDOW).await;
            }
            // Take the *latest* event (newer notifications may have arrived
            // during the settle window, the previous sync, or a retry delay).
            let Some(event) = rx.borrow_and_update().clone() else {
                continue;
            };
            match event.files {
                Some(n) => info!(seq = event.seq, "sync started ({n} files changed)"),
                None => info!(seq = event.seq, "sync started"),
            }
            let started = Instant::now();
            match run_rsync(&repo_root, &target).await {
                Ok(()) => {
                    retrying = false;
                    info!(
                        seq = event.seq,
                        "sync finished in {:.2?}",
                        started.elapsed()
                    );
                    debug!(
                        prev_seq = last_synced.as_ref().map(|s| s.seq),
                        new_seq = event.seq,
                        clock = ?event.clock,
                        "recorded synced clock"
                    );
                    last_synced = Some(SyncedClock {
                        seq: event.seq,
                        clock: event.clock,
                        completed_at: SystemTime::now(),
                    });
                }
                Err(err) => {
                    retrying = true;
                    error!(
                        seq = event.seq,
                        "sync failed after {:.2?}: {err:#}; retrying in {RETRY_DELAY:?}",
                        started.elapsed()
                    );
                }
            }
        }
    };

    // Both futures run forever; whichever errors first ends the process.
    tokio::select! {
        result = reader => result,
        result = runner => result,
    }
}

/// Run one full-tree rsync of `repo_root` to `target`.
async fn run_rsync(repo_root: &Path, target: &Target) -> Result<()> {
    let mut source = repo_root.as_os_str().to_owned();
    source.push("/");
    let mut cmd = tokio::process::Command::new("rsync");
    cmd.arg("-a")
        // --delete-after, not plain --delete: per-directory merge rules
        // (.gitignore) only protect receiver files from deletion if the
        // receiver has the merge files when it deletes, which is only
        // guaranteed after the transfer (see "PER-DIRECTORY RULES AND
        // DELETE" in rsync(1)). Never --delete-excluded.
        .arg("--delete-after")
        // Compare mtimes at nanosecond granularity (needs receiver >=
        // 3.1.3). Without this, a file rewritten with same-size contents
        // within one second of a synced version is skipped by rsync's
        // quick-check *forever* — its size and integer mtime never change
        // again.
        .arg("--modify-window=-1")
        .args(rsync_filter_args(repo_root))
        .arg(source)
        .arg(target.rsync_dest());
    debug!(?cmd, "running rsync");
    let status = cmd
        .status()
        .await
        .context("failed to run rsync (is it installed?)")?;
    match status.code() {
        Some(0) => Ok(()),
        // 24: "partial transfer due to vanished source files" — expected
        // under churn; the change that removed them triggers a new sync.
        Some(24) => {
            warn!("rsync reported vanished source files (exit 24); continuing");
            Ok(())
        }
        Some(code) => bail!("rsync exited with status {code}"),
        None => bail!("rsync was killed by a signal"),
    }
}

/// Build the rsync filter arguments implementing Phase 1's approximate
/// ignore handling:
///
/// - `.git/` and `.dsync/` are always excluded (and, since we never pass
///   `--delete-excluded`, never deleted from the destination);
/// - per-directory `.gitignore` files apply via rsync's dir-merge filter;
/// - `.git/info/exclude` and `core.excludesFile` apply as lower-precedence
///   merge files (rsync filter rules are first-match-wins, so later args
///   have lower precedence, matching git's ordering).
fn rsync_filter_args(repo_root: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "--exclude=.git".into(),
        "--exclude=.dsync".into(),
        "--filter=:- .gitignore".into(),
    ];
    for file in global_exclude_files(repo_root) {
        let mut arg = OsString::from("--filter=.- ");
        arg.push(&file);
        args.push(arg);
    }
    args
}

/// The repo-global gitignore files, in decreasing precedence order:
/// `$GIT_DIR/info/exclude`, then `core.excludesFile` (defaulting to
/// `$XDG_CONFIG_HOME/git/ignore` or `~/.config/git/ignore`). Only files
/// that exist are returned. Failures here are logged, not fatal: ignore
/// handling is best-effort in Phase 1.
fn global_exclude_files(repo_root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();

    match git_output(repo_root, &["rev-parse", "--git-path", "info/exclude"]) {
        Ok(Some(path)) => {
            // --git-path output is relative to the cwd we ran git in.
            let path = repo_root.join(path);
            if path.is_file() {
                files.push(path);
            }
        }
        Ok(None) => {}
        Err(err) => warn!("cannot locate git info/exclude: {err:#}"),
    }

    match git_output(
        repo_root,
        &["config", "--path", "--get", "core.excludesFile"],
    ) {
        Ok(Some(path)) => {
            let path = PathBuf::from(path);
            if path.is_file() {
                files.push(path);
            }
        }
        // Unset: fall back to git's documented default location.
        Ok(None) => {
            if let Some(path) = default_excludes_file()
                && path.is_file()
            {
                files.push(path);
            }
        }
        Err(err) => warn!("cannot read core.excludesFile: {err:#}"),
    }

    files
}

fn default_excludes_file() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("git/ignore"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/git/ignore"))
}

/// Run git in `repo_root` and return its trimmed stdout. `Ok(None)` means
/// git exited non-zero with no output (e.g. `config --get` on an unset
/// key); other failures are errors.
fn git_output(repo_root: &Path, args: &[&str]) -> Result<Option<String>> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .output()
        .context("failed to run git")?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() {
        if stdout.is_empty() {
            return Ok(None);
        }
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(if stdout.is_empty() {
        None
    } else {
        Some(stdout)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_args_always_exclude_git_and_dsync() {
        let tmp = tempfile::tempdir().unwrap();
        let args = rsync_filter_args(tmp.path());
        assert!(args.contains(&OsString::from("--exclude=.git")));
        assert!(args.contains(&OsString::from("--exclude=.dsync")));
        assert!(args.contains(&OsString::from("--filter=:- .gitignore")));
        // No --delete-excluded, ever.
        assert!(
            !args
                .iter()
                .any(|a| a.to_string_lossy().contains("delete-excluded"))
        );
    }

    #[test]
    fn filter_args_pick_up_info_exclude_and_excludes_file() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            let st = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .status()
                .unwrap();
            assert!(st.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);

        std::fs::write(repo.join(".git/info/exclude"), "*.tmp\n").unwrap();
        let global = tmp.path().join("global-ignore");
        std::fs::write(&global, "*.bak\n").unwrap();
        git(&["config", "core.excludesFile", global.to_str().unwrap()]);

        let args = rsync_filter_args(&repo);
        let rendered: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            rendered
                .iter()
                .any(|a| a.starts_with("--filter=.- ") && a.ends_with("info/exclude")),
            "missing info/exclude merge in {rendered:?}"
        );
        assert!(
            rendered
                .iter()
                .any(|a| a.starts_with("--filter=.- ") && a.ends_with("global-ignore")),
            "missing core.excludesFile merge in {rendered:?}"
        );
        // info/exclude must come before (higher precedence than) the
        // global excludes file.
        let info = rendered.iter().position(|a| a.ends_with("info/exclude"));
        let glob = rendered.iter().position(|a| a.ends_with("global-ignore"));
        assert!(info < glob);
    }
}
