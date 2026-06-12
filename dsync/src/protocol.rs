//! The wire protocol spoken over `.dsync/dsync.sock`: newline-delimited
//! JSON requests and responses, versioned via a `{"version": 1}` handshake
//! field carried on every message.
//!
//! Replica multiplexing is in-band: requests carry a replica name (default
//! `"default"`), and a `list` request enumerates the live replicas.
//!
//! Invariant: watchman clocks never appear on the wire. Responses report
//! *state* — receipt-order sequence numbers and wall-clock times — never
//! clock strings and never transient boolean flags.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// The protocol version both sides must speak.
pub const VERSION: u32 = 1;

/// The replica name used when none is given.
pub const DEFAULT_REPLICA: &str = "default";

/// A request, one JSON object per line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub version: u32,
    #[serde(flatten)]
    pub op: RequestOp,
}

impl Request {
    pub fn new(op: RequestOp) -> Request {
        Request {
            version: VERSION,
            op,
        }
    }
}

/// The operation a request asks for, tagged by the `request` field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
pub enum RequestOp {
    /// Enumerate the live replicas.
    List,
    /// Report the sync state of one replica.
    Status {
        #[serde(default = "default_replica")]
        replica: String,
    },
    /// Block until a completed sync covers everything that changed before
    /// this request arrived. The request is bare: the server reads the
    /// watchman clock (cookie-synchronized) on the client's behalf, so the
    /// "as-of" point is the moment the server receives the request.
    Barrier {
        #[serde(default = "default_replica")]
        replica: String,
        /// Give up after this many seconds: the server replies with the
        /// (not yet covered) state at expiry instead of parking forever.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout: Option<f64>,
    },
}

fn default_replica() -> String {
    DEFAULT_REPLICA.to_string()
}

/// A response, one JSON object per line: `{"version":1,"ok":{...}}` on
/// success or `{"version":1,"error":"..."}` on failure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response<T> {
    pub version: u32,
    #[serde(flatten)]
    pub result: RpcResult<T>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RpcResult<T> {
    Ok(T),
    Error(String),
}

/// Payload of a successful `list` response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ListResponse {
    pub replicas: Vec<String>,
}

/// Payload of a successful `status` response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusResponse {
    pub replica: String,
    /// PID of the `ds sync` server process.
    pub pid: u32,
    /// When the server started, as unix seconds.
    pub server_started_at: f64,
    /// The sync destination, as `[HOST:]PATH`.
    pub target: String,
    /// The last completed sync, if any.
    pub synced: Option<SyncedStatus>,
    /// The in-flight sync, if any.
    pub syncing: Option<SyncingStatus>,
    /// Files changed since the last completed sync, computed server-side
    /// at request time via a cookie-synchronized watchman since-query
    /// against the synced clock. `None` iff no sync has completed yet
    /// (`synced` is also `None`): there is no clock to query against.
    pub pending: Option<PendingStatus>,
}

/// State of the last completed sync: "synced as-of (internal) clock with
/// sequence number `seq`, at wall-clock time `completed_at`".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncedStatus {
    /// Receipt-order sequence number of the clock this sync covers.
    pub seq: u64,
    /// When the sync completed, as unix seconds.
    pub completed_at: f64,
}

/// State of an in-flight sync.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncingStatus {
    /// Receipt-order sequence number of the clock this sync will cover.
    pub seq: u64,
    /// When the sync started, as unix seconds.
    pub started_at: f64,
}

/// Result of the server's since-query against the synced clock.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingStatus {
    /// Number of files changed since the last completed sync (0 means the
    /// replica is up-to-date as of the query).
    pub files: u64,
    /// True if watchman reported `is_fresh_instance` for the query: the
    /// synced clock belongs to a dead watchman instance, `files` counts
    /// the whole tree, and a full resync is due.
    pub fresh_instance: bool,
}

/// Payload of a `barrier` response. Per the "state not flags" principle,
/// the server never says "done" or "timed out": it reports the sequence
/// number it assigned to the request's clock and the sync state at reply
/// time, and the client judges coverage with [`BarrierResponse::is_covered`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BarrierResponse {
    pub replica: String,
    /// Receipt-order sequence number assigned to the cookie-synchronized
    /// clock the server read when the request arrived: the point in time
    /// the barrier asks to be covered.
    pub target_seq: u64,
    /// The last completed sync at reply time.
    pub synced: Option<SyncedStatus>,
    /// Result of the server's since-query against the synced clock, when
    /// one ran at reply time: `files == 0` means nothing has changed since
    /// the last completed sync, i.e. the replica is up-to-date as of a
    /// moment no earlier than the barrier itself.
    pub pending: Option<PendingStatus>,
}

impl BarrierResponse {
    /// Does this reply say the barrier's point in time is covered by a
    /// completed sync? Either a sync covers a clock sequenced at/after the
    /// barrier's, or nothing at all changed since the last completed sync.
    pub fn is_covered(&self) -> bool {
        if let Some(synced) = &self.synced
            && synced.seq >= self.target_seq
        {
            return true;
        }
        matches!(&self.pending, Some(p) if p.files == 0)
    }
}

/// Render a `SystemTime` as unix seconds for the wire.
pub fn unix_seconds(t: SystemTime) -> f64 {
    match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => d.as_secs_f64(),
        Err(e) => -e.duration().as_secs_f64(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_wire_format() {
        let req = Request::new(RequestOp::List);
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            r#"{"version":1,"request":"list"}"#
        );

        let req = Request::new(RequestOp::Status {
            replica: "default".into(),
        });
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            r#"{"version":1,"request":"status","replica":"default"}"#
        );
    }

    #[test]
    fn barrier_request_wire_format() {
        // A bare barrier request: no timeout key, replica defaulted.
        let req: Request = serde_json::from_str(r#"{"version":1,"request":"barrier"}"#).unwrap();
        assert_eq!(
            req.op,
            RequestOp::Barrier {
                replica: "default".into(),
                timeout: None,
            }
        );
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            r#"{"version":1,"request":"barrier","replica":"default"}"#
        );

        let req = Request::new(RequestOp::Barrier {
            replica: "default".into(),
            timeout: Some(1.5),
        });
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            r#"{"version":1,"request":"barrier","replica":"default","timeout":1.5}"#
        );
    }

    #[test]
    fn barrier_response_round_trips_without_clocks() {
        let resp = Response {
            version: VERSION,
            result: RpcResult::Ok(BarrierResponse {
                replica: "default".into(),
                target_seq: 12,
                synced: Some(SyncedStatus {
                    seq: 12,
                    completed_at: 1.5e9,
                }),
                pending: None,
            }),
        };
        let line = serde_json::to_string(&resp).unwrap();
        // No watchman clock ever appears on the wire.
        assert!(!line.contains("clock"));
        let back: Response<BarrierResponse> = serde_json::from_str(&line).unwrap();
        assert_eq!(back, resp);
    }

    #[test]
    fn barrier_coverage_judgement() {
        let base = BarrierResponse {
            replica: "default".into(),
            target_seq: 10,
            synced: None,
            pending: None,
        };
        // Nothing synced, nothing known: not covered.
        assert!(!base.is_covered());
        // A sync covering a clock sequenced at/after the barrier's.
        let synced = |seq: u64, files: Option<u64>| BarrierResponse {
            synced: Some(SyncedStatus {
                seq,
                completed_at: 0.0,
            }),
            pending: files.map(|files| PendingStatus {
                files,
                fresh_instance: false,
            }),
            ..base.clone()
        };
        assert!(synced(10, None).is_covered());
        assert!(synced(11, None).is_covered());
        assert!(!synced(9, None).is_covered());
        assert!(!synced(9, Some(3)).is_covered());
        // An older sync with nothing pending since it: up-to-date as-of
        // a moment at/after the barrier.
        assert!(synced(9, Some(0)).is_covered());
    }

    #[test]
    fn status_request_defaults_replica() {
        let req: Request = serde_json::from_str(r#"{"version":1,"request":"status"}"#).unwrap();
        assert_eq!(
            req.op,
            RequestOp::Status {
                replica: "default".into()
            }
        );
    }

    #[test]
    fn response_wire_format() {
        let resp = Response {
            version: VERSION,
            result: RpcResult::Ok(ListResponse {
                replicas: vec!["default".into()],
            }),
        };
        let line = serde_json::to_string(&resp).unwrap();
        assert_eq!(line, r#"{"version":1,"ok":{"replicas":["default"]}}"#);
        let back: Response<ListResponse> = serde_json::from_str(&line).unwrap();
        assert_eq!(back, resp);

        let err: Response<ListResponse> =
            serde_json::from_str(r#"{"version":1,"error":"nope"}"#).unwrap();
        assert_eq!(err.result, RpcResult::Error("nope".into()));
    }

    #[test]
    fn status_response_round_trips() {
        let resp = Response {
            version: VERSION,
            result: RpcResult::Ok(StatusResponse {
                replica: "default".into(),
                pid: 1234,
                server_started_at: 1.5e9,
                target: "host:/srv/replica".into(),
                synced: Some(SyncedStatus {
                    seq: 7,
                    completed_at: 1.5e9 + 10.0,
                }),
                syncing: Some(SyncingStatus {
                    seq: 9,
                    started_at: 1.5e9 + 11.0,
                }),
                pending: Some(PendingStatus {
                    files: 3,
                    fresh_instance: false,
                }),
            }),
        };
        let line = serde_json::to_string(&resp).unwrap();
        // No watchman clock ever appears on the wire.
        assert!(!line.contains("clock"));
        let back: Response<StatusResponse> = serde_json::from_str(&line).unwrap();
        assert_eq!(back, resp);
    }

    #[test]
    fn unknown_request_op_is_a_parse_error() {
        let res: Result<Request, _> =
            serde_json::from_str(r#"{"version":1,"request":"frobnicate"}"#);
        assert!(res.is_err());
    }
}
