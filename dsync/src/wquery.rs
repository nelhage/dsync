//! Raw watchman `since` queries carrying a `dsync-ignore`-produced JSON
//! expression.
//!
//! `dsync-ignore` emits its watchman translations as `serde_json::Value`
//! expressions, property-tested directly against `watchman query`. But
//! `watchman_client`'s typed `query` API only accepts its own `Expr` enum,
//! which has no escape hatch for a raw expression. We therefore build the
//! query PDU by hand and send it through the client's `generic_request`
//! (which serializes any `Serialize` value), so the exact, property-tested
//! expression reaches watchman unchanged.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};
use watchman_client::prelude::Clock;

use crate::server::Watchman;

/// watchman's default cookie-synchronization timeout, in milliseconds — the
/// same value `watchman_client` substitutes for `SyncTimeout::Default`.
/// Passing it explicitly makes every since-query cookie-synchronized
/// ("as-of now") regardless of the server's configured default.
const SYNC_TIMEOUT_MS: i64 = 60_000;

/// One changed path returned by a since-query. The `exists`/`file_type`
/// fields are read by the fast path; the pending-count caller uses only the
/// list length, so they appear unused until the fast path lands.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ChangedFile {
    /// Path relative to the watched root.
    pub name: PathBuf,
    /// Whether the path exists as of the query (`false` ⇒ it was deleted).
    pub exists: bool,
    /// watchman's file type: `"f"` regular file, `"d"` directory, `"l"`
    /// symlink, etc. Absent if `"type"` was not among the requested fields.
    pub file_type: Option<String>,
}

/// The outcome of a since-query.
#[derive(Debug)]
pub struct SinceResult {
    /// watchman reported a fresh instance: the clock we queried against
    /// belongs to a dead instance, so the result covers the whole tree.
    /// Callers must treat this as "resync everything" (a full rsync).
    pub is_fresh_instance: bool,
    pub files: Vec<ChangedFile>,
}

#[derive(Deserialize)]
struct RawResult {
    #[serde(default)]
    is_fresh_instance: bool,
    files: Option<Vec<RawFile>>,
    // The result `clock` is intentionally ignored: the sync loop records the
    // triggering event's clock for ordering, never a clock read out-of-band
    // here (which would not carry a receipt-order sequence number).
}

#[derive(Deserialize)]
struct RawFile {
    name: PathBuf,
    #[serde(default)]
    exists: bool,
    #[serde(rename = "type")]
    file_type: Option<String>,
}

/// Run a cookie-synchronized `since` query over the shared watchman
/// connection, returning the paths changed since `since` that match `expr`.
///
/// The query always requests the `name`/`exists`/`type` fields: watchman
/// renders the file list as bare strings when only `name` is requested but
/// as objects for any larger field set, so requesting all three keeps the
/// response shape uniform (and supplies everything the fast path needs).
pub async fn since_query(wm: &Watchman, since: &Clock, expr: &Value) -> Result<SinceResult> {
    let mut params = serde_json::Map::new();
    params.insert("since".into(), serde_json::to_value(since)?);
    params.insert("expression".into(), expr.clone());
    params.insert("fields".into(), json!(["name", "exists", "type"]));
    params.insert("sync_timeout".into(), json!(SYNC_TIMEOUT_MS));
    if let Some(rel) = wm.root.project_relative_path() {
        params.insert("relative_root".into(), json!(rel));
    }
    let request = json!(["query", wm.root.project_root(), Value::Object(params)]);
    let raw: RawResult = wm
        .client
        .generic_request(request)
        .await
        .context("watchman since-query failed")?;
    Ok(SinceResult {
        is_fresh_instance: raw.is_fresh_instance,
        files: raw
            .files
            .unwrap_or_default()
            .into_iter()
            .map(|f| ChangedFile {
                name: f.name,
                exists: f.exists,
                file_type: f.file_type,
            })
            .collect(),
    })
}

/// The expression selecting files that *should be synced*: everything not
/// matched by the ignore rules' `ignored` expression. Wrapping in `["not",
/// …]` here (rather than using `dsync-ignore`'s `watchman_synced_files_expr`,
/// which also requires `["type", "f"]`) keeps deletions in the result — a
/// deleted synced file still needs to be propagated.
pub fn not_ignored(ignored_expr: &Value) -> Value {
    json!(["not", ignored_expr])
}
