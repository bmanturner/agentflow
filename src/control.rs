use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::state::ControlEvent;

const MAX_PAYLOAD_BYTES: usize = 64 * 1024;

pub struct ControlServer {
    receiver: Option<mpsc::Receiver<ControlEvent>>,
    socket_path: PathBuf,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl ControlServer {
    pub fn take_receiver(&mut self) -> Result<mpsc::Receiver<ControlEvent>> {
        self.receiver
            .take()
            .ok_or_else(|| anyhow!("control server receiver already taken"))
    }

    pub async fn shutdown(mut self) -> Result<()> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
        remove_socket_file(&self.socket_path).await
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

pub async fn start_control_server(socket_path: PathBuf) -> Result<ControlServer> {
    if let Some(parent) = socket_path.parent() {
        fs::create_dir_all(parent).await.with_context(|| {
            format!(
                "failed to create control socket directory {}",
                parent.display()
            )
        })?;
    }

    remove_socket_file(&socket_path).await?;
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind control socket {}", socket_path.display()))?;

    let (event_sender, receiver) = mpsc::channel(16);
    let (shutdown_sender, mut shutdown_receiver) = oneshot::channel();

    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = &mut shutdown_receiver => break,
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, _)) => {
                            handle_connection(stream, event_sender.clone()).await;
                        }
                        Err(_) => break,
                    }
                }
            }
        }
    });

    Ok(ControlServer {
        receiver: Some(receiver),
        socket_path,
        shutdown: Some(shutdown_sender),
        task: Some(task),
    })
}

pub async fn send_control_event(socket_path: &Path, event: &ControlEvent) -> Result<()> {
    let mut stream = UnixStream::connect(socket_path).await.with_context(|| {
        format!(
            "failed to connect to control socket {}",
            socket_path.display()
        )
    })?;
    let payload = serde_json::to_vec(event).context("failed to encode control event")?;
    stream
        .write_all(&payload)
        .await
        .context("failed to write control event")?;
    stream
        .shutdown()
        .await
        .context("failed to finish control event request")?;

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .context("failed to read control response")?;
    parse_control_response(&response)
}

async fn handle_connection(mut stream: UnixStream, sender: mpsc::Sender<ControlEvent>) {
    let result = async {
        let event = read_control_event(&mut stream).await?;
        sender
            .send(event)
            .await
            .map_err(|_| anyhow!("control server is shutting down"))?;
        Ok::<_, anyhow::Error>(())
    }
    .await;

    let response = match result {
        Ok(()) => ControlResponse::success(),
        Err(error) => ControlResponse::failure(error.to_string()),
    };

    let payload = match serde_json::to_vec(&response) {
        Ok(payload) => payload,
        Err(_) => b"{\"success\":false,\"error\":\"failed to encode response\"}".to_vec(),
    };
    let _ = stream.write_all(&payload).await;
    let _ = stream.shutdown().await;
}

async fn read_control_event(stream: &mut UnixStream) -> Result<ControlEvent> {
    let mut payload = Vec::new();
    let mut limited = stream.take(MAX_PAYLOAD_BYTES as u64 + 1);
    limited
        .read_to_end(&mut payload)
        .await
        .context("failed to read control event")?;
    if payload.is_empty() {
        bail!("empty control event");
    }
    if payload.len() > MAX_PAYLOAD_BYTES {
        bail!("control event exceeds {MAX_PAYLOAD_BYTES} bytes");
    }
    serde_json::from_slice(&payload).context("invalid control event")
}

fn parse_control_response(payload: &[u8]) -> Result<()> {
    if payload.is_empty() {
        bail!("empty control response");
    }
    let response: ControlResponse =
        serde_json::from_slice(payload).context("invalid control response")?;
    if response.success {
        return Ok(());
    }
    let error = response
        .error
        .unwrap_or_else(|| "control event rejected".to_owned());
    bail!("{error}")
}

async fn remove_socket_file(socket_path: &Path) -> Result<()> {
    match fs::remove_file(socket_path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("failed to remove control socket {}", socket_path.display())),
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct ControlResponse {
    success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(flatten)]
    extra: serde_json::Map<String, Value>,
}

impl ControlResponse {
    fn success() -> Self {
        Self {
            success: true,
            error: None,
            extra: serde_json::Map::new(),
        }
    }

    fn failure(error: String) -> Self {
        Self {
            success: false,
            error: Some(error),
            extra: serde_json::Map::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_control_response_accepts_success() {
        assert!(parse_control_response(br#"{"success":true}"#).is_ok());
    }

    #[test]
    fn parse_control_response_rejects_failure() {
        let result = parse_control_response(br#"{"success":false,"error":"nope"}"#);

        assert!(result.is_err());
    }
}
