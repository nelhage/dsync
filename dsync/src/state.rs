//! State shared between the sync loop and the IPC server.
//!
//! Clock-handling invariants (see doc/plan.md "Clock handling"): watchman
//! clocks are opaque and live only inside this process. Every clock the
//! server receives is tagged with a local monotonic sequence number drawn
//! from [`ServerState::next_seq`], and all ordering is done on those
//! sequence numbers — clock strings are never parsed or compared.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use watchman_client::prelude::Clock;

use crate::target::Target;

/// The record of a completed sync: the watchman clock it covers (opaque;
/// ordered only by `seq`, the receipt-order sequence number assigned when
/// the clock arrived over our watchman connection) and when it finished.
#[derive(Debug, Clone)]
pub struct SyncedClock {
    pub seq: u64,
    pub clock: Clock,
    pub completed_at: SystemTime,
}

/// The record of an in-flight sync: the sequence number of the clock it
/// will cover once it completes, and when it started.
#[derive(Debug, Clone)]
pub struct SyncingClock {
    pub seq: u64,
    pub started_at: SystemTime,
}

/// Per-replica sync state, per the "state not flags" principle: we record
/// what has been synced (and what is being synced), never booleans like
/// "up to date".
#[derive(Debug, Clone)]
pub struct ReplicaState {
    pub target: Target,
    pub synced: Option<SyncedClock>,
    pub syncing: Option<SyncingClock>,
}

/// State for the whole `ds sync` server process.
#[derive(Debug)]
pub struct ServerState {
    pub pid: u32,
    pub started_at: SystemTime,
    next_seq: AtomicU64,
    replicas: Mutex<BTreeMap<String, ReplicaState>>,
}

impl ServerState {
    pub fn new() -> ServerState {
        ServerState {
            pid: std::process::id(),
            started_at: SystemTime::now(),
            next_seq: AtomicU64::new(1),
            replicas: Mutex::new(BTreeMap::new()),
        }
    }

    /// Assign the next receipt-order sequence number. Called (serially, in
    /// receipt order) for each clock observed over the watchman connection.
    pub fn next_seq(&self) -> u64 {
        self.next_seq.fetch_add(1, Ordering::Relaxed)
    }

    /// Register a replica. Panics if the name is already taken.
    pub fn add_replica(&self, name: &str, target: Target) {
        let mut replicas = self.replicas.lock().unwrap();
        let prev = replicas.insert(
            name.to_string(),
            ReplicaState {
                target,
                synced: None,
                syncing: None,
            },
        );
        assert!(prev.is_none(), "replica {name:?} registered twice");
    }

    /// Names of all live replicas.
    pub fn replica_names(&self) -> Vec<String> {
        self.replicas.lock().unwrap().keys().cloned().collect()
    }

    /// A point-in-time copy of one replica's state.
    pub fn replica(&self, name: &str) -> Option<ReplicaState> {
        self.replicas.lock().unwrap().get(name).cloned()
    }

    /// Mutate one replica's state. Returns `None` if the replica does not
    /// exist.
    pub fn with_replica<T>(&self, name: &str, f: impl FnOnce(&mut ReplicaState) -> T) -> Option<T> {
        self.replicas.lock().unwrap().get_mut(name).map(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seq_is_monotonic() {
        let state = ServerState::new();
        let a = state.next_seq();
        let b = state.next_seq();
        assert!(b > a);
    }

    #[test]
    fn replica_registration_and_updates() {
        let state = ServerState::new();
        assert!(state.replica_names().is_empty());
        assert!(state.replica("default").is_none());
        assert!(state.with_replica("default", |_| ()).is_none());

        state.add_replica("default", Target::Local("/tmp/replica".into()));
        assert_eq!(state.replica_names(), vec!["default".to_string()]);

        state.with_replica("default", |r| {
            r.syncing = Some(SyncingClock {
                seq: 3,
                started_at: SystemTime::now(),
            });
        });
        let snap = state.replica("default").unwrap();
        assert_eq!(snap.syncing.unwrap().seq, 3);
        assert!(snap.synced.is_none());
    }
}
