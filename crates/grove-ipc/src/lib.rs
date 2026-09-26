//! The CLI ↔ app protocol: one JSON request per connection over a Unix
//! socket, answered by zero or more `output` lines and one final line.

mod server;

pub use server::{ServerHandle, serve};

use grove_core::{ClusterPlan, NewClusterSource, TeardownOptions};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Snapshot,
    Reload,
    Doctor,
    /// Start a cluster, or one process with `target = Some((project, process))`.
    Start {
        cluster: String,
        target: Option<(String, String)>,
    },
    Stop {
        cluster: String,
        target: Option<(String, String)>,
    },
    Restart {
        cluster: String,
        target: Option<(String, String)>,
    },
    Plan {
        source: NewClusterSource,
    },
    Create {
        plan: ClusterPlan,
    },
    Retry {
        cluster: String,
        start: bool,
    },
    TeardownReport {
        cluster: String,
    },
    Teardown {
        cluster: String,
        options: TeardownOptions,
    },
    Pull {
        cluster: String,
        project: String,
    },
    Logs {
        cluster: String,
        project: String,
        process: String,
        follow: bool,
    },
    OpenUrl {
        url: String,
    },
    Edit {
        cluster: String,
        project: String,
    },
    ExternalWorktrees,
    /// Show the main window (used when the CLI launches or finds the app).
    ShowWindow,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    /// Streamed text (logs, progress).
    Output {
        text: String,
    },
    Ok {
        value: serde_json::Value,
    },
    Err {
        message: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("Grove isn't running")]
    NotRunning,
    #[error("{0}")]
    Remote(String),
    #[error("connection closed before a reply")]
    Closed,
    #[error("bad message: {0}")]
    Protocol(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Sends one request and returns the final value. `on_output` receives
/// streamed text as it arrives.
pub async fn call(
    socket: &Path,
    request: &Request,
    mut on_output: impl FnMut(&str),
) -> Result<serde_json::Value, IpcError> {
    let stream = UnixStream::connect(socket)
        .await
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
                IpcError::NotRunning
            }
            _ => IpcError::Io(e),
        })?;
    let (read, mut write) = stream.into_split();
    let mut line = serde_json::to_vec(request)?;
    line.push(b'\n');
    write.write_all(&line).await?;
    write.flush().await?;

    let mut lines = BufReader::new(read).lines();
    while let Some(line) = lines.next_line().await? {
        match serde_json::from_str::<Response>(&line)? {
            Response::Output { text } => on_output(&text),
            Response::Ok { value } => return Ok(value),
            Response::Err { message } => return Err(IpcError::Remote(message)),
        }
    }
    Err(IpcError::Closed)
}

/// True if something answers on the socket.
pub async fn is_running(socket: &Path) -> bool {
    call(socket, &Request::Ping, |_| {}).await.is_ok()
}
