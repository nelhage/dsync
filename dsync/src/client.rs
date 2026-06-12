//! The IPC client side: connecting to a running `ds sync` server and
//! issuing requests over the newline-delimited JSON protocol.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::protocol::{self, Request, RequestOp, Response, RpcResult};
use crate::server;

/// A connection to the `ds sync` server for a repo.
pub struct IpcClient {
    stream: BufReader<UnixStream>,
}

impl IpcClient {
    /// Connect to the server socket for the repo rooted at `repo_root`.
    /// A missing socket or a connection refusal (a stale socket whose
    /// server died) both mean no server is running.
    pub async fn connect(repo_root: &Path) -> Result<IpcClient> {
        let path = server::socket_path(repo_root);
        let stream = UnixStream::connect(&path).await.map_err(|err| {
            use std::io::ErrorKind;
            match err.kind() {
                ErrorKind::NotFound | ErrorKind::ConnectionRefused => anyhow!(
                    "no ds sync is running in this repository (cannot connect to {})",
                    path.display()
                ),
                _ => anyhow!("cannot connect to {}: {err}", path.display()),
            }
        })?;
        Ok(IpcClient {
            stream: BufReader::new(stream),
        })
    }

    /// Issue one request and parse the typed response payload.
    pub async fn request<T: DeserializeOwned>(&mut self, op: RequestOp) -> Result<T> {
        let mut line =
            serde_json::to_vec(&Request::new(op)).expect("request serialization cannot fail");
        line.push(b'\n');
        self.stream
            .get_mut()
            .write_all(&line)
            .await
            .context("cannot send request to ds sync")?;

        let mut response = String::new();
        let n = self
            .stream
            .read_line(&mut response)
            .await
            .context("cannot read response from ds sync")?;
        if n == 0 {
            bail!("ds sync closed the connection without responding");
        }
        let response: Response<T> = serde_json::from_str(&response)
            .with_context(|| format!("cannot parse server response: {}", response.trim_end()))?;
        if response.version != protocol::VERSION {
            bail!(
                "server speaks protocol version {} but we speak {}",
                response.version,
                protocol::VERSION
            );
        }
        match response.result {
            RpcResult::Ok(payload) => Ok(payload),
            RpcResult::Error(err) => bail!("ds sync reported an error: {err}"),
        }
    }
}
