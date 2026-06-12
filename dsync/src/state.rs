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

use tokio::sync::watch;
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
    /// Bumped whenever any replica records a completed sync; parked
    /// barrier requests subscribe to this and re-check their replica's
    /// state on every wake-up.
    synced_generation: watch::Sender<u64>,
}

impl ServerState {
    pub fn new() -> ServerState {
        ServerState {
            pid: std::process::id(),
            started_at: SystemTime::now(),
            next_seq: AtomicU64::new(1),
            replicas: Mutex::new(BTreeMap::new()),
            synced_generation: watch::Sender::new(0),
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

    /// Record a completed sync for one replica (clearing any in-flight
    /// marker) and wake every waiter subscribed via [`subscribe_synced`].
    ///
    /// [`subscribe_synced`]: ServerState::subscribe_synced
    pub fn record_synced(&self, name: &str, synced: SyncedClock) {
        let updated = self
            .with_replica(name, |r| {
                r.syncing = None;
                r.synced = Some(synced);
            })
            .is_some();
        debug_assert!(updated, "record_synced for unknown replica {name:?}");
        self.synced_generation.send_modify(|generation| {
            *generation += 1;
        });
    }

    /// Subscribe to sync completions: the receiver is marked changed every
    /// time [`record_synced`] runs (for any replica — wake-ups re-check
    /// state, so spurious ones are harmless).
    ///
    /// [`record_synced`]: ServerState::record_synced
    pub fn subscribe_synced(&self) -> watch::Receiver<u64> {
        self.synced_generation.subscribe()
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

    #[test]
    fn record_synced_updates_state_and_wakes_subscribers() {
        let state = ServerState::new();
        state.add_replica("default", Target::Local("/tmp/replica".into()));
        state.with_replica("default", |r| {
            r.syncing = Some(SyncingClock {
                seq: 4,
                started_at: SystemTime::now(),
            });
        });

        let mut rx = state.subscribe_synced();
        rx.mark_unchanged();
        assert!(!rx.has_changed().unwrap());

        state.record_synced(
            "default",
            SyncedClock {
                seq: 4,
                clock: Clock::Spec(watchman_client::prelude::ClockSpec::default()),
                completed_at: SystemTime::now(),
            },
        );

        let snap = state.replica("default").unwrap();
        assert_eq!(snap.synced.unwrap().seq, 4);
        assert!(snap.syncing.is_none(), "syncing marker should be cleared");
        assert!(
            rx.has_changed().unwrap(),
            "subscribers should be woken by record_synced"
        );
    }
}
