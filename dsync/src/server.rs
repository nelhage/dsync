//! The IPC server side: the `.dsync/` control directory, the instance lock,
//! and the request loop on the single UNIX socket (`.dsync/dsync.sock`).
//!
//! One server process per repo. Liveness is determined by an `flock` on
//! `.dsync/lock`, held for the process lifetime: if the lock is held,
//! another `ds sync` is running; if it is free, any leftover socket is
//! stale and may be unlinked.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};
use watchman_client::prelude::*;

use crate::protocol::{
    self, BarrierResponse, ListResponse, PendingStatus, Request, RequestOp, Response, RpcResult,
    StatusResponse, SyncedStatus, SyncingStatus,
};
use crate::state::{ReplicaState, ServerState};

/// The shared watchman connection: the sync loop's subscription and the IPC
/// server's queries all flow over this single connection, which is what
/// makes receipt-order sequence numbers a valid clock order.
pub struct Watchman {
    pub client: Client,
    pub root: ResolvedRoot,
}

/// A handle for assigning receipt-order sequence numbers to clocks the IPC
/// server reads (e.g. for `barrier`).
///
/// Receipt order is clock order only if sequence numbers are assigned in
/// the order clocks arrive over the watchman connection — but subscription
/// notifications and command responses are delivered to *different* tasks,
/// so assigning a sequence number directly here could race with a
/// notification that was received earlier but not yet sequenced. Instead,
/// every assignment is delegated to the watchman reader task, which drains
/// all already-delivered subscription notifications (sequencing them) before
/// granting a number. Since the watchman client dispatches everything it
/// reads from the single connection serially, any notification the daemon
/// sent before our clock response has already been delivered by the time we
/// ask, and therefore gets the smaller sequence number.
#[derive(Clone)]
pub struct SeqAssigner {
    tx: mpsc::UnboundedSender<oneshot::Sender<u64>>,
}

/// The reader-task end of [`seq_assigner`]: a queue of pending grant
/// requests, each answered with a freshly assigned sequence number.
pub type SeqRequests = mpsc::UnboundedReceiver<oneshot::Sender<u64>>;

/// Create the channel pair connecting [`SeqAssigner::assign`] callers to
/// the watchman reader task.
pub fn seq_assigner() -> (SeqAssigner, SeqRequests) {
    let (tx, rx) = mpsc::unbounded_channel();
    (SeqAssigner { tx }, rx)
}

impl SeqAssigner {
    /// Assign the next receipt-order sequence number, ordered after every
    /// clock already received over the watchman connection. Call this only
    /// *after* the clock it sequences has been read (i.e. its response has
    /// arrived).
    pub async fn assign(&self) -> Result<u64> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(reply_tx)
            .map_err(|_| anyhow::anyhow!("the watchman reader task is gone"))?;
        reply_rx.await.context("the watchman reader task is gone")
    }
}

/// The path of the IPC socket for a repo.
pub fn socket_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".dsync").join("dsync.sock")
}

/// The `.dsync/` control directory, holding the instance lock. The flock on
/// `.dsync/lock` is held for as long as this value (i.e. the process)
/// lives; the kernel releases it on any kind of process death.
#[derive(Debug)]
pub struct ControlDir {
    dir: PathBuf,
    _lock: std::fs::File,
}

impl ControlDir {
    /// Create `.dsync/` (if needed) and take the instance lock. Errors if
    /// another `ds sync` already holds it.
    pub fn acquire(repo_root: &Path) -> Result<ControlDir> {
        let dir = repo_root.join(".dsync");
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
        let lock_path = dir.join("lock");
        let lock = std::fs::File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("cannot open {}", lock_path.display()))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                bail!(
                    "ds sync is already running in this repository (lock {} is held)",
                    lock_path.display()
                );
            }
            Err(std::fs::TryLockError::Error(err)) => {
                return Err(err).with_context(|| format!("cannot lock {}", lock_path.display()));
            }
        }
        Ok(ControlDir { dir, _lock: lock })
    }

    /// Bind the IPC socket. We hold the instance lock, so any existing
    /// socket was left behind by a dead server and can be unlinked.
    pub fn bind_socket(&self) -> Result<UnixListener> {
        let path = self.dir.join("dsync.sock");
        match std::fs::remove_file(&path) {
            Ok(()) => info!("removed stale socket {}", path.display()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("cannot remove stale socket {}", path.display()));
            }
        }
        UnixListener::bind(&path).with_context(|| format!("cannot bind {}", path.display()))
    }
}

/// Accept and serve IPC connections forever.
pub async fn run(
    listener: UnixListener,
    state: Arc<ServerState>,
    watchman: Arc<Watchman>,
    seq: SeqAssigner,
) -> Result<()> {
    loop {
        let (stream, _addr) = listener
            .accept()
            .await
            .context("accept on the IPC socket failed")?;
        let state = Arc::clone(&state);
        let watchman = Arc::clone(&watchman);
        let seq = seq.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_connection(stream, state, watchman, seq).await {
                debug!("IPC connection ended with error: {err:#}");
            }
        });
    }
}

/// Serve one client connection: a sequence of newline-delimited JSON
/// requests, each answered with one newline-delimited JSON response.
async fn handle_connection(
    stream: UnixStream,
    state: Arc<ServerState>,
    watchman: Arc<Watchman>,
    seq: SeqAssigner,
) -> Result<()> {
    let (read_half, write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();
    let mut writer = BufWriter::new(write_half);
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let result = match serde_json::from_str::<Request>(&line) {
            Ok(req) => handle_request(req, &state, &watchman, &seq).await,
            Err(err) => RpcResult::Error(format!("cannot parse request: {err}")),
        };
        if let RpcResult::Error(err) = &result {
            warn!("IPC request failed: {err}");
        }
        let response = Response {
            version: protocol::VERSION,
            result,
        };
        let mut buf = serde_json::to_vec(&response).expect("response serialization cannot fail");
        buf.push(b'\n');
        writer.write_all(&buf).await?;
        writer.flush().await?;
    }
    Ok(())
}

async fn handle_request(
    req: Request,
    state: &ServerState,
    watchman: &Watchman,
    seq: &SeqAssigner,
) -> RpcResult<Value> {
    if req.version != protocol::VERSION {
        return RpcResult::Error(format!(
            "unsupported protocol version {} (this server speaks {})",
            req.version,
            protocol::VERSION
        ));
    }
    debug!(op = ?req.op, "IPC request");
    match req.op {
        RequestOp::List => RpcResult::Ok(to_value(ListResponse {
            replicas: state.replica_names(),
        })),
        RequestOp::Status { replica } => handle_status(&replica, state, watchman).await,
        RequestOp::Barrier { replica, timeout } => {
            handle_barrier(&replica, timeout, state, watchman, seq).await
        }
    }
}

async fn handle_status(
    replica: &str,
    state: &ServerState,
    watchman: &Watchman,
) -> RpcResult<Value> {
    let Some(snapshot) = state.replica(replica) else {
        return RpcResult::Error(format!("unknown replica {replica:?}"));
    };
    // Up-to-dateness is *state computed now*, server-side: a cookie-
    // synchronized since-query against the synced clock yields the set of
    // files not yet covered by a completed sync. The clock itself never
    // leaves this process. With no completed sync there is no clock to
    // query against; `pending` stays `None`.
    let pending = match &snapshot.synced {
        Some(synced) => match pending_files(watchman, synced.clock.clone()).await {
            Ok(pending) => Some(pending),
            Err(err) => {
                return RpcResult::Error(format!(
                    "watchman since-query for replica {replica:?} failed: {err:#}"
                ));
            }
        },
        None => None,
    };
    RpcResult::Ok(to_value(StatusResponse {
        replica: replica.to_string(),
        pid: state.pid,
        server_started_at: protocol::unix_seconds(state.started_at),
        target: snapshot.target.to_string(),
        synced: snapshot.synced.map(|s| SyncedStatus {
            seq: s.seq,
            completed_at: protocol::unix_seconds(s.completed_at),
        }),
        syncing: snapshot.syncing.map(|s| SyncingStatus {
            seq: s.seq,
            started_at: protocol::unix_seconds(s.started_at),
        }),
        pending,
    }))
}

/// Handle a `barrier` request: pin down "now" and park until a completed
/// sync covers it.
///
/// The pin is a cookie-synchronized watchman clock read performed *after*
/// the request arrives ("as-of invocation" semantics): every filesystem
/// change that happened before the request is covered by that clock. The
/// clock value itself is discarded — only the receipt-order sequence
/// number assigned to it (via the reader task, see [`SeqAssigner`])
/// matters.
async fn handle_barrier(
    replica: &str,
    timeout: Option<f64>,
    state: &ServerState,
    watchman: &Watchman,
    seq: &SeqAssigner,
) -> RpcResult<Value> {
    if state.replica(replica).is_none() {
        return RpcResult::Error(format!("unknown replica {replica:?}"));
    }
    if let Some(t) = timeout
        && !(t.is_finite() && t >= 0.0)
    {
        return RpcResult::Error(format!(
            "invalid barrier timeout {t}: must be a non-negative number of seconds"
        ));
    }
    if let Err(err) = watchman
        .client
        .clock(&watchman.root, SyncTimeout::Default)
        .await
    {
        return RpcResult::Error(format!(
            "watchman clock read for replica {replica:?} failed: {err:#}"
        ));
    }
    let target_seq = match seq.assign().await {
        Ok(seq) => seq,
        Err(err) => return RpcResult::Error(format!("{err:#}")),
    };
    debug!(replica, target_seq, "barrier parked");

    let wait = wait_for_coverage(replica, target_seq, state, watchman);
    let outcome = match timeout {
        Some(t) => match tokio::time::timeout(Duration::from_secs_f64(t), wait).await {
            Ok(outcome) => outcome,
            // Timeout expired: reply with the current (not covered) state
            // and let the client judge it.
            Err(_elapsed) => state
                .replica(replica)
                .context("replica disappeared")
                .map(|snapshot| (snapshot, None)),
        },
        None => wait.await,
    };
    let (snapshot, pending) = match outcome {
        Ok(result) => result,
        Err(err) => {
            return RpcResult::Error(format!("barrier for replica {replica:?} failed: {err:#}"));
        }
    };
    RpcResult::Ok(to_value(BarrierResponse {
        replica: replica.to_string(),
        target_seq,
        synced: snapshot.synced.map(|s| SyncedStatus {
            seq: s.seq,
            completed_at: protocol::unix_seconds(s.completed_at),
        }),
        pending,
    }))
}

/// Park until the replica's sync state covers `target_seq`, re-checking on
/// every completed sync. Two ways to be covered:
///
/// - a completed sync's clock is sequenced at/after the barrier's
///   (`synced.seq >= target_seq`) — under continuous churn this is the
///   path that terminates, since the target is fixed while sync seqs only
///   grow; or
/// - a cookie-synchronized since-query against the synced clock comes back
///   empty: nothing has changed since the last completed sync, so the
///   replica is up-to-date as of *now* (>= the barrier's point in time).
///   This is the path that terminates when the barrier's clock was bumped
///   by non-file activity (e.g. its own sync cookie) and no further
///   notification — hence no further sync — is ever coming.
async fn wait_for_coverage(
    replica: &str,
    target_seq: u64,
    state: &ServerState,
    watchman: &Watchman,
) -> Result<(ReplicaState, Option<PendingStatus>)> {
    let mut completions = state.subscribe_synced();
    loop {
        // Mark the current generation seen *before* checking, so a sync
        // completing during the checks below re-wakes us immediately.
        completions.borrow_and_update();
        let snapshot = state.replica(replica).context("replica disappeared")?;
        if let Some(synced) = &snapshot.synced {
            if synced.seq >= target_seq {
                return Ok((snapshot, None));
            }
            let pending = pending_files(watchman, synced.clock.clone())
                .await
                .context("watchman since-query failed")?;
            if pending.files == 0 {
                return Ok((snapshot, Some(pending)));
            }
        }
        completions
            .changed()
            .await
            .context("the sync runner is gone")?;
    }
}

/// Count the files changed since `clock` with a cookie-synchronized
/// watchman since-query. `is_fresh_instance` means the clock belongs to a
/// dead watchman instance: the count covers the whole tree and the
/// subscription is independently delivering the full-resync signal.
async fn pending_files(watchman: &Watchman, clock: Clock) -> Result<PendingStatus> {
    let result: QueryResult<NameOnly> = watchman
        .client
        .query(
            &watchman.root,
            QueryRequestCommon {
                since: Some(clock),
                // Cookie synchronization: the result reflects all
                // filesystem changes that happened before this request.
                sync_timeout: SyncTimeout::Default,
                expression: Some(exclude_internal_paths()),
                ..Default::default()
            },
        )
        .await?;
    Ok(PendingStatus {
        files: result.files.map_or(0, |f| f.len()) as u64,
        fresh_instance: result.is_fresh_instance,
    })
}

/// Match everything except `.git/` and `.dsync/` (and their contents),
/// which are never synced and so are never "pending".
fn exclude_internal_paths() -> Expr {
    Expr::Not(Box::new(Expr::Any(vec![
        Expr::Name(NameTerm {
            paths: vec![".git".into(), ".dsync".into()],
            wholename: true,
        }),
        Expr::DirName(DirNameTerm {
            path: ".git".into(),
            depth: None,
        }),
        Expr::DirName(DirNameTerm {
            path: ".dsync".into(),
            depth: None,
        }),
    ])))
}

fn to_value<T: serde::Serialize>(payload: T) -> Value {
    serde_json::to_value(payload).expect("payload serialization cannot fail")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_is_exclusive() {
        let tmp = tempfile::tempdir().unwrap();
        let first = ControlDir::acquire(tmp.path()).unwrap();
        let err = ControlDir::acquire(tmp.path()).unwrap_err();
        assert!(
            err.to_string().contains("already running"),
            "unexpected error: {err:#}"
        );
        // Dropping the first lock frees it for a successor.
        drop(first);
        ControlDir::acquire(tmp.path()).unwrap();
    }

    #[tokio::test]
    async fn stale_socket_is_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let control = ControlDir::acquire(tmp.path()).unwrap();
        // A leftover socket from a dead server...
        let sock = socket_path(tmp.path());
        std::os::unix::net::UnixListener::bind(&sock).unwrap();
        // ...is unlinked and rebound.
        let _listener = control.bind_socket().unwrap();
        assert!(sock.exists());
    }
}
