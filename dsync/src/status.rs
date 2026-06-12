//! `ds status`: report the state of the running sync server, per replica.

use std::time::SystemTime;

use anyhow::Result;

use crate::client::IpcClient;
use crate::protocol::{ListResponse, RequestOp, StatusResponse, unix_seconds};
use crate::repo;

pub async fn cmd_status() -> Result<()> {
    let repo_root = repo::find_repo_root(&std::env::current_dir()?)?;
    let mut client = IpcClient::connect(&repo_root).await?;
    let list: ListResponse = client.request(RequestOp::List).await?;
    for replica in list.replicas {
        let status: StatusResponse = client.request(RequestOp::Status { replica }).await?;
        println!("{}", render_status(&status, SystemTime::now()));
    }
    Ok(())
}

/// Render one replica's status as a single line:
/// `REPLICA: pid PID -> TARGET: up-to-date (synced 2.3s ago)` and friends.
fn render_status(status: &StatusResponse, now: SystemTime) -> String {
    let mut line = format!(
        "{}: pid {} -> {}: ",
        status.replica, status.pid, status.target
    );
    match (&status.synced, &status.pending) {
        (None, _) => line.push_str("initial sync not yet complete"),
        (Some(synced), Some(pending)) if pending.files == 0 => {
            line.push_str(&format!(
                "up-to-date (synced {} ago)",
                fmt_ago(now, synced.completed_at)
            ));
        }
        (Some(synced), Some(pending)) => {
            line.push_str(&format!(
                "{} file{} pending (last sync {} ago)",
                pending.files,
                if pending.files == 1 { "" } else { "s" },
                fmt_ago(now, synced.completed_at)
            ));
            if pending.fresh_instance {
                line.push_str(" [watchman restarted; full resync due]");
            }
        }
        // The server always computes `pending` when a sync has completed;
        // tolerate its absence anyway.
        (Some(synced), None) => {
            line.push_str(&format!("synced {} ago", fmt_ago(now, synced.completed_at)));
        }
    }
    if status.syncing.is_some() {
        line.push_str(", sync in progress");
    }
    line
}

/// How long ago the unix-seconds timestamp `then` was, humanized.
fn fmt_ago(now: SystemTime, then: f64) -> String {
    let now = unix_seconds(now);
    let secs = (now - then).max(0.0);
    if secs < 120.0 {
        format!("{secs:.1}s")
    } else if secs < 2.0 * 3600.0 {
        format!("{:.0}m", secs / 60.0)
    } else {
        format!("{:.1}h", secs / 3600.0)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::protocol::{PendingStatus, SyncedStatus, SyncingStatus};

    fn base(synced: Option<SyncedStatus>, pending: Option<PendingStatus>) -> StatusResponse {
        StatusResponse {
            replica: "default".into(),
            pid: 4242,
            server_started_at: 1000.0,
            target: "host:/srv/replica".into(),
            synced,
            syncing: None,
            pending,
        }
    }

    fn at(secs: f64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs_f64(secs)
    }

    #[test]
    fn renders_up_to_date() {
        let status = base(
            Some(SyncedStatus {
                seq: 5,
                completed_at: 1100.0,
            }),
            Some(PendingStatus {
                files: 0,
                fresh_instance: false,
            }),
        );
        assert_eq!(
            render_status(&status, at(1102.5)),
            "default: pid 4242 -> host:/srv/replica: up-to-date (synced 2.5s ago)"
        );
    }

    #[test]
    fn renders_pending_and_syncing() {
        let mut status = base(
            Some(SyncedStatus {
                seq: 5,
                completed_at: 1100.0,
            }),
            Some(PendingStatus {
                files: 3,
                fresh_instance: false,
            }),
        );
        status.syncing = Some(SyncingStatus {
            seq: 6,
            started_at: 1101.0,
        });
        assert_eq!(
            render_status(&status, at(1101.0)),
            "default: pid 4242 -> host:/srv/replica: 3 files pending (last sync 1.0s ago), sync in progress"
        );
    }

    #[test]
    fn renders_initial_sync() {
        let status = base(None, None);
        assert_eq!(
            render_status(&status, at(1101.0)),
            "default: pid 4242 -> host:/srv/replica: initial sync not yet complete"
        );
    }

    #[test]
    fn renders_fresh_instance() {
        let status = base(
            Some(SyncedStatus {
                seq: 5,
                completed_at: 1100.0,
            }),
            Some(PendingStatus {
                files: 1,
                fresh_instance: true,
            }),
        );
        let line = render_status(&status, at(1101.0));
        assert!(line.contains("1 file pending"), "got: {line}");
        assert!(line.contains("watchman restarted"), "got: {line}");
    }

    #[test]
    fn fmt_ago_scales() {
        assert_eq!(fmt_ago(at(1010.0), 1000.0), "10.0s");
        assert_eq!(fmt_ago(at(1600.0), 1000.0), "10m");
        assert_eq!(fmt_ago(at(1000.0 + 3.0 * 3600.0), 1000.0), "3.0h");
        // Clock skew never yields a negative age.
        assert_eq!(fmt_ago(at(1000.0), 1010.0), "0.0s");
    }
}
