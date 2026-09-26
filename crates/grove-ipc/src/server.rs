use crate::{IpcError, Request, Response};
use grove_core::{Core, CoreError};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;

/// Called for [`Request::ShowWindow`]; the GUI brings its window forward.
pub type ShowWindow = Arc<dyn Fn() + Send + Sync>;

pub struct ServerHandle {
    task: JoinHandle<()>,
    path: PathBuf,
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Listens on `socket`. Fails if another Grove already answers there.
pub async fn serve(
    core: Core,
    socket: &Path,
    show_window: Option<ShowWindow>,
) -> Result<ServerHandle, IpcError> {
    if socket.exists() {
        if crate::is_running(socket).await {
            return Err(IpcError::Remote(format!(
                "another Grove is already running ({})",
                socket.display()
            )));
        }
        std::fs::remove_file(socket)?;
    }
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(socket)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    }
    let task = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let core = core.clone();
                    let show = show_window.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(core, stream, show).await {
                            tracing::debug!("ipc connection: {e}");
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("ipc accept: {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
        }
    });
    Ok(ServerHandle {
        task,
        path: socket.to_path_buf(),
    })
}

async fn handle_connection(
    core: Core,
    stream: UnixStream,
    show_window: Option<ShowWindow>,
) -> Result<(), IpcError> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let Some(line) = lines.next_line().await? else {
        return Ok(());
    };
    let reply = match serde_json::from_str::<Request>(&line) {
        Ok(request) => dispatch(&core, request, &mut write, show_window)
            .await
            .map_err(|e| e.to_string()),
        Err(e) => Err(format!("bad request: {e}")),
    };
    let response = match reply {
        Ok(value) => Response::Ok { value },
        Err(message) => Response::Err { message },
    };
    send(&mut write, &response).await
}

async fn send(write: &mut OwnedWriteHalf, response: &Response) -> Result<(), IpcError> {
    let mut line = serde_json::to_vec(response)?;
    line.push(b'\n');
    write.write_all(&line).await?;
    Ok(())
}

fn to_value<T: serde::Serialize>(v: T) -> Result<Value, CoreError> {
    serde_json::to_value(v).map_err(|e| CoreError::Invalid(e.to_string()))
}

async fn dispatch(
    core: &Core,
    request: Request,
    write: &mut OwnedWriteHalf,
    show_window: Option<ShowWindow>,
) -> Result<Value, CoreError> {
    match request {
        Request::Ping => Ok(json!("pong")),
        Request::Snapshot => to_value(core.snapshot()),
        Request::Reload => to_value(core.reload_config()),
        Request::Doctor => to_value(core.doctor().await),
        Request::Start { cluster, target } => {
            match target {
                Some((p, n)) => core.start_process(&cluster, &p, &n).await?,
                None => core.start_cluster(&cluster).await?,
            }
            Ok(Value::Null)
        }
        Request::Stop { cluster, target } => {
            match target {
                Some((p, n)) => core.stop_process(&cluster, &p, &n).await?,
                None => core.stop_cluster(&cluster).await?,
            }
            Ok(Value::Null)
        }
        Request::Restart { cluster, target } => {
            match target {
                Some((p, n)) => core.restart_process(&cluster, &p, &n).await?,
                None => core.restart_cluster(&cluster).await?,
            }
            Ok(Value::Null)
        }
        Request::Plan { source } => to_value(core.plan_cluster(source).await?),
        Request::Create { plan } => {
            let name = plan.name.clone();
            core.create_cluster(plan).await?;
            Ok(json!(name))
        }
        Request::Retry { cluster, start } => {
            core.retry_cluster(&cluster, start).await?;
            Ok(Value::Null)
        }
        Request::TeardownReport { cluster } => to_value(core.teardown_report(&cluster).await?),
        Request::Teardown { cluster, options } => {
            core.teardown(&cluster, options).await?;
            Ok(Value::Null)
        }
        Request::Pull { cluster, project } => to_value(core.pull(&cluster, &project).await?),
        Request::Logs {
            cluster,
            project,
            process,
            follow,
        } => {
            stream_logs(core, &cluster, &project, &process, follow, write).await?;
            Ok(Value::Null)
        }
        Request::OpenUrl { url } => {
            core.open_url(&url)?;
            Ok(Value::Null)
        }
        Request::Edit { cluster, project } => {
            core.open_in_editor(&cluster, &project).await?;
            Ok(Value::Null)
        }
        Request::ExternalWorktrees => to_value(core.external_worktrees().await?),
        Request::ShowWindow => {
            if let Some(show) = show_window {
                show();
            }
            Ok(Value::Null)
        }
    }
}

const LOG_TAIL_BYTES: u64 = 64 * 1024;

async fn stream_logs(
    core: &Core,
    cluster: &str,
    project: &str,
    process: &str,
    follow: bool,
    write: &mut OwnedWriteHalf,
) -> Result<(), CoreError> {
    let mut decoder = Utf8Stream::default();
    let Some(handle) = core.process_handle(cluster, project, process) else {
        // Not running: show the end of its log file.
        let path = core.home().log_path(cluster, project, process);
        let text = read_tail(&path, LOG_TAIL_BYTES).unwrap_or_default();
        let text = if text.is_empty() {
            format!("(no output yet for {project}.{process} in {cluster})\n")
        } else {
            text
        };
        let _ = send(write, &Response::Output { text }).await;
        return Ok(());
    };

    let mut sub = handle.subscribe();
    let text = decoder.push(&sub.replay);
    if !text.is_empty() && send(write, &Response::Output { text }).await.is_err() {
        return Ok(());
    }
    if !follow {
        return Ok(());
    }
    loop {
        tokio::select! {
            chunk = sub.rx.recv() => match chunk {
                Ok(bytes) => {
                    let text = decoder.push(&bytes);
                    if !text.is_empty() && send(write, &Response::Output { text }).await.is_err() {
                        return Ok(());
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
            },
            exit = handle.wait() => {
                // Drain what's left, then report the exit.
                while let Ok(bytes) = sub.rx.try_recv() {
                    let text = decoder.push(&bytes);
                    let _ = send(write, &Response::Output { text }).await;
                }
                let text = format!("\n[{project}.{process} {exit}]\n");
                let _ = send(write, &Response::Output { text }).await;
                return Ok(());
            }
        }
    }
}

fn read_tail(path: &Path, max: u64) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(max)))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Decodes UTF-8 across chunk boundaries.
#[derive(Default)]
struct Utf8Stream {
    pending: Vec<u8>,
}

impl Utf8Stream {
    fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let valid_up_to = match std::str::from_utf8(&self.pending) {
            Ok(_) => self.pending.len(),
            Err(e) if e.error_len().is_none() => e.valid_up_to(),
            Err(_) => self.pending.len(),
        };
        let rest = self.pending.split_off(valid_up_to);
        let text = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending = rest;
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_across_chunks() {
        let mut d = Utf8Stream::default();
        let bytes = "héllo".as_bytes();
        assert_eq!(d.push(&bytes[..2]), "h");
        assert_eq!(d.push(&bytes[2..]), "éllo");
    }
}
