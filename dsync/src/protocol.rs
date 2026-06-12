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
