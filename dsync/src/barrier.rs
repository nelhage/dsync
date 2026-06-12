//! `ds barrier`: block until the replica is up-to-date as-of this
//! invocation.
//!
//! The request is bare: the server reads the watchman clock (with cookie
//! synchronization) on our behalf when the request arrives, so clients
//! never observe clocks. The server parks the request until a completed
//! sync covers that point in time — or until the timeout, if one was
//! given, in which case it replies with the current (not covered) state
//! and we exit with [`TIMEOUT_EXIT_CODE`].

use anyhow::{Result, bail};

use crate::client::IpcClient;
use crate::protocol::{BarrierResponse, DEFAULT_REPLICA, RequestOp};
use crate::repo;

/// Exit code for a barrier that timed out, distinct from generic errors
/// (1) and CLI usage errors (2, from clap).
pub const TIMEOUT_EXIT_CODE: i32 = 3;

/// What the barrier reply told us.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// A completed sync covers this invocation's point in time.
    Synced,
    /// The timeout expired first.
    TimedOut,
}

/// Validate a `--timeout` value (shared with `ds exec`).
pub fn validate_timeout(timeout: Option<f64>) -> Result<()> {
    if let Some(t) = timeout
        && !(t.is_finite() && t >= 0.0)
    {
        bail!("--timeout must be a non-negative number of seconds");
    }
    Ok(())
}

pub async fn cmd_barrier(timeout: Option<f64>) -> Result<Outcome> {
    validate_timeout(timeout)?;
    let repo_root = repo::find_repo_root(&std::env::current_dir()?)?;
    let mut client = IpcClient::connect(&repo_root).await?;
    let response: BarrierResponse = client
        .request(RequestOp::Barrier {
            replica: DEFAULT_REPLICA.to_string(),
            timeout,
        })
        .await?;
    if response.is_covered() {
        return Ok(Outcome::Synced);
    }
    // The server replies with not-covered state only when the (requested)
    // timeout expired.
    // Phrased for both callers (`ds barrier` itself and `ds exec`'s
    // implicit barrier).
    match &response.synced {
        Some(synced) => eprintln!(
            "timed out waiting for sync: last completed sync covers seq {}, but this barrier needs seq {}",
            synced.seq, response.target_seq
        ),
        None => eprintln!(
            "timed out waiting for sync: no sync has completed yet (this barrier needs seq {})",
            response.target_seq
        ),
    }
    Ok(Outcome::TimedOut)
}
