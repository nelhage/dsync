//! The core sync loop: watch the repository with watchman and propagate
//! changes to the target with full-tree rsync runs.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use tokio::sync::watch;
use tracing::{debug, error, info, trace, warn};
use watchman_client::SubscriptionData;
use watchman_client::prelude::*;

use crate::fastpath::{self, Compression};
use crate::ignore::Rules;
use crate::protocol::DEFAULT_REPLICA;
use crate::server::{self, Watchman};
use crate::state::{ServerState, SyncedClock, SyncingClock};
use crate::target::Target;

/// How long to let the filesystem settle after a change notification before
/// starting a sync, coalescing bursts of events into one rsync run.
const SETTLE_WINDOW: Duration = Duration::from_millis(75);

/// How long to wait before retrying after a failed sync.
const RETRY_DELAY: Duration = Duration::from_secs(2);

/// How often to force a full rsync as a self-healing measure, repairing any
/// drift the fast path might have left, even while changes keep flowing.
const HEAL_INTERVAL: Duration = Duration::from_secs(300);

/// The self-heal interval, overridable via `DSYNC_HEAL_INTERVAL_MS` (an
/// internal knob, used by the integration tests to avoid a 5-minute wait).
fn heal_interval() -> Duration {
    match std::env::var("DSYNC_HEAL_INTERVAL_MS") {
        Ok(ms) => ms
            .parse()
            .map(Duration::from_millis)
            .unwrap_or(HEAL_INTERVAL),
        Err(_) => HEAL_INTERVAL,
    }
}

/// A change notification from watchman, tagged with its receipt-order
/// sequence number.
#[derive(Debug, Clone)]
struct ChangeEvent {
    seq: u64,
    clock: Clock,
    files: Option<usize>,
    /// watchman reported a fresh instance with this notification: the whole
    /// tree must be resynced, so the fast path is not eligible.
    fresh_instance: bool,
}

/// A coarse subscription filter that drops notifications for the
/// always-excluded internal paths (`.git`/`.dsync` and their contents).
///
/// `.git/` churns constantly (index locks, refs, logs) yet is *never*
/// synced, so without this filter every git operation wakes the sync loop
/// only for the fast path to find nothing to do. These two paths are
/// excluded unconditionally (the invariant), independent of the dynamic
/// ignore rules, so filtering them at the subscription is always safe.
///
/// This is only a pre-filter to cut spurious wakeups; the authoritative,
/// property-tested filtering still happens in the fast path's since-query
/// (`wquery::not_ignored` over the live rules). It mirrors
/// `ignore::builtin_ignored_expr` in the typed `Expr` form that
/// `SubscribeRequest` requires (the query side uses raw JSON; see `wquery`).
fn subscription_expr() -> Expr {
    let dir_or_self = |name: &str| {
        Expr::Any(vec![
            Expr::DirName(DirNameTerm {
                path: PathBuf::from(name),
                depth: None,
            }),
            Expr::Name(NameTerm {
                paths: vec![PathBuf::from(name)],
                wholename: true,
            }),
        ])
    };
    Expr::Not(Box::new(Expr::Any(vec![
        dir_or_self(".git"),
        dir_or_self(".dsync"),
    ])))
}

/// Run `ds sync`: subscribe to watchman and rsync the repository to
/// `target` on every (settled) change, forever, while serving IPC requests
/// on `.dsync/dsync.sock`.
pub async fn run(repo_root: PathBuf, target: Target) -> Result<()> {
    // Take the instance lock and bind the socket first, so a second
    // `ds sync` in the same repo fails fast with "already running".
    let control = server::ControlDir::acquire(&repo_root)?;
    let listener = control.bind_socket()?;

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
        .subscribe::<NameOnly>(
            &resolved,
            SubscribeRequest {
                expression: Some(subscription_expr()),
                ..SubscribeRequest::default()
            },
        )
        .await
        .context("watchman subscribe failed")?;
    info!("watching {} -> {}", repo_root.display(), target);

    let state = Arc::new(ServerState::new());
    state.add_replica(DEFAULT_REPLICA, target.clone());
    let watchman = Arc::new(Watchman {
        client,
        root: resolved,
    });
    // The repository's ignore rules, shared with the IPC server (which
    // filters its since-queries against them) and reloaded by the runner
    // whenever an ignore file changes.
    let (rules_tx, rules_rx) = watch::channel(Arc::new(Rules::load(&repo_root)));
    // Sequence-number grants for IPC-read clocks (e.g. `barrier`) are
    // funneled through the reader task below so that sequence order always
    // matches clock receipt order; see `SeqAssigner`.
    let (seq_assigner, mut seq_requests) = server::seq_assigner();
    let server = server::run(
        listener,
        Arc::clone(&state),
        Arc::clone(&watchman),
        rules_rx,
        seq_assigner,
    );

    // Latest-value channel from the watchman reader to the sync runner: the
    // runner always syncs the newest observed event, so any number of
    // changes arriving mid-sync coalesce into at most one pending follow-up.
    let (tx, mut rx) = watch::channel::<Option<ChangeEvent>>(None);

    let reader_state = Arc::clone(&state);
    let reader = async move {
        loop {
            // `biased` polls top-to-bottom, so every already-delivered
            // subscription notification is sequenced before any pending
            // sequence-number grant: grants are therefore ordered after
            // every clock that was received before them, which is what
            // makes a granted seq a sound barrier target.
            tokio::select! {
                biased;
                data = subscription.next() => match data? {
                    SubscriptionData::FilesChanged(result) => {
                        // Receipt order is clock order: tag each clock with
                        // the next sequence number as it arrives.
                        let seq = reader_state.next_seq();
                        // Dump the raw watchman event at the highest verbosity
                        // (`-vvv`); see `init_tracing` for the gating.
                        tracing::trace!(
                            target: "watchman_events",
                            seq,
                            event = ?result,
                            "watchman event"
                        );
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
                            fresh_instance: result.is_fresh_instance,
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
                },
                Some(grant) = seq_requests.recv() => {
                    // The requester may have given up (e.g. a dropped IPC
                    // connection); a dead oneshot is fine.
                    let _ = grant.send(reader_state.next_seq());
                }
            }
        }
    };

    // The fast path queries and streams over the same watchman connection
    // and target; probe for zstd on both ends once, here at startup.
    let runner_watchman = Arc::clone(&watchman);
    let compression = fastpath::detect_compression(&target).await;

    let runner = async move {
        let ctx = SyncCtx {
            repo_root: &repo_root,
            target: &target,
            watchman: &runner_watchman,
            compression,
        };
        let mut retrying = false;
        let mut force_full = false;
        // Fire the first self-heal one interval from now, not immediately.
        let interval = heal_interval();
        let mut heal = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
        heal.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            if retrying {
                tokio::time::sleep(RETRY_DELAY).await;
            } else {
                tokio::select! {
                    changed = rx.changed() => {
                        if changed.is_err() {
                            bail!("watchman event stream ended");
                        }
                        tokio::time::sleep(SETTLE_WINDOW).await;
                    }
                    _ = heal.tick() => {
                        // Periodic self-heal: re-sync the latest known state
                        // with a full rsync. Skip if nothing has synced yet
                        // (no state to repair).
                        if rx.borrow().is_none() {
                            continue;
                        }
                        force_full = true;
                        debug!("periodic self-heal: forcing a full rsync");
                    }
                }
            }
            // Take the *latest* event (newer notifications may have arrived
            // during the settle window, the previous sync, or a retry delay).
            // On a self-heal tick this is the last event, re-synced in full.
            let Some(event) = rx.borrow_and_update().clone() else {
                continue;
            };
            // Reload the ignore rules so edits to `.gitignore` /
            // `.dsyncexclude` take effect, and publish them to the IPC
            // server (whose since-queries filter against them).
            let rules = Arc::new(Rules::load(&repo_root));
            let _ = rules_tx.send(Arc::clone(&rules));
            state.with_replica(DEFAULT_REPLICA, |r| {
                r.syncing = Some(SyncingClock {
                    seq: event.seq,
                    started_at: SystemTime::now(),
                });
            });
            // The clock of the last completed sync is the lower bound for a
            // fast-path since-query. With none yet (the startup sync), the
            // fast path is ineligible and we full-rsync.
            let prev_clock = state
                .replica(DEFAULT_REPLICA)
                .and_then(|r| r.synced.map(|s| s.clock));
            let started = Instant::now();
            match sync_once(&ctx, &rules, prev_clock.as_ref(), &event, force_full).await {
                Ok(outcome) => {
                    retrying = false;
                    force_full = false;
                    match outcome {
                        SyncOutcome::Synced(mode) => info!(
                            seq = event.seq,
                            "{mode} sync finished in {:.2?}",
                            started.elapsed()
                        ),
                        // Only ignored files changed since the last sync, so
                        // nothing was propagated. This can fire on every
                        // touched build artifact, so stay quiet at INFO — but
                        // still advance the synced clock below so barriers
                        // waiting on this event are released.
                        SyncOutcome::NoChanges => trace!(
                            seq = event.seq,
                            "no syncable changes ({:.2?})",
                            started.elapsed()
                        ),
                    }
                    debug!(
                        seq = event.seq,
                        clock = ?event.clock,
                        "recorded synced clock"
                    );
                    state.record_synced(
                        DEFAULT_REPLICA,
                        SyncedClock {
                            seq: event.seq,
                            clock: event.clock,
                            completed_at: SystemTime::now(),
                        },
                    );
                }
                Err(err) => {
                    retrying = true;
                    state.with_replica(DEFAULT_REPLICA, |r| r.syncing = None);
                    error!(
                        seq = event.seq,
                        "sync failed after {:.2?}: {err:#}; retrying in {RETRY_DELAY:?}",
                        started.elapsed()
                    );
                }
            }
        }
    };

    // All three futures run forever; whichever errors first ends the
    // process.
    tokio::select! {
        result = reader => result,
        result = runner => result,
        result = server => result,
    }
}

/// The result of one sync attempt, for logging by the caller.
enum SyncOutcome {
    /// Real work was propagated; the label is the sync mode (`"fast"` /
    /// `"full"`).
    Synced(&'static str),
    /// The fast path found nothing syncable (only ignored files changed since
    /// the last sync). The synced clock still advances, but there is nothing
    /// to report.
    NoChanges,
}

/// Run one sync of the latest `event`, taking the small-change fast path
/// when it is eligible and falling back to a full rsync otherwise (or when
/// the fast path declines or errors). Returns the outcome for logging.
/// The loop-invariant context for a sync: where we sync from and to, over
/// which watchman connection, and how the fast path compresses.
struct SyncCtx<'a> {
    repo_root: &'a Path,
    target: &'a Target,
    watchman: &'a Watchman,
    compression: Compression,
}

async fn sync_once(
    ctx: &SyncCtx<'_>,
    rules: &Rules,
    prev_clock: Option<&Clock>,
    event: &ChangeEvent,
    force_full: bool,
) -> Result<SyncOutcome> {
    // The fast path is eligible only when the work can be bounded precisely:
    // not a forced self-heal, a prior completed sync to query since, no fresh
    // instance, a rule set watchman can express, and a notification small
    // enough to be worth it.
    if !force_full
        && let Some(since) = prev_clock
        && !event.fresh_instance
        && let Some(ignored) = rules.ignored_expr()
        && event.files.is_none_or(|n| n <= fastpath::MAX_FILES)
    {
        match fastpath::try_fast_path(
            ctx.repo_root,
            ctx.target,
            ctx.watchman,
            ignored,
            since,
            ctx.compression,
        )
        .await
        {
            Ok(fastpath::Outcome::Applied) => return Ok(SyncOutcome::Synced("fast")),
            Ok(fastpath::Outcome::NoChanges) => return Ok(SyncOutcome::NoChanges),
            // The correctness valve: any uncertainty falls back to a full
            // rsync, which is always correct on its own.
            Ok(fastpath::Outcome::Fallback(reason)) => {
                debug!(
                    seq = event.seq,
                    "fast path declined ({reason}); running full rsync"
                );
            }
            Err(err) => {
                warn!(
                    seq = event.seq,
                    "fast path failed ({err:#}); running full rsync"
                );
            }
        }
    }
    // "sync started" is logged only here, for the full rsync: it is the slow
    // path where the up-front notice is worth it. A fast-path sync is near
    // instantaneous, so its single "fast sync finished" line suffices.
    match event.files {
        Some(n) => info!(seq = event.seq, "full sync started ({n} files changed)"),
        None => info!(seq = event.seq, "full sync started"),
    }
    run_rsync(ctx.repo_root, ctx.target, rules).await?;
    Ok(SyncOutcome::Synced("full"))
}

/// Run one full-tree rsync of `repo_root` to `target`, honoring `rules`.
async fn run_rsync(repo_root: &Path, target: &Target, rules: &Rules) -> Result<()> {
    // Prefer the exact `dsync-ignore` translation; on the rare untranslatable
    // rule set (a pathological `**` blow-up) fall back to the interim
    // per-directory dir-merge filters, which are approximately right.
    let filters = rules
        .rsync_filter_args()
        .unwrap_or_else(|| interim_rsync_filter_args(repo_root));
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
        .args(filters)
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

/// Build the interim rsync filter arguments — Phase 1's approximate ignore
/// handling, now used only as the fallback when `dsync-ignore` cannot
/// translate the rules exactly (a pathological `**` blow-up):
///
/// - `.git/` and `.dsync/` are always excluded (and, since we never pass
///   `--delete-excluded`, never deleted from the destination);
/// - per-directory `.gitignore` files apply via rsync's dir-merge filter;
/// - `.git/info/exclude` and `core.excludesFile` apply as lower-precedence
///   merge files (rsync filter rules are first-match-wins, so later args
///   have lower precedence, matching git's ordering).
fn interim_rsync_filter_args(repo_root: &Path) -> Vec<OsString> {
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
        let args = interim_rsync_filter_args(tmp.path());
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

        let args = interim_rsync_filter_args(&repo);
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
