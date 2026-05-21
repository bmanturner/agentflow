use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio::time::timeout;

const READY_TIMEOUT: Duration = Duration::from_secs(30);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(300);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
const KILL_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub session_id: String,
    pub session_file: PathBuf,
}

#[derive(Debug)]
pub struct PromptOutcome {
    pub ack: Value,
    pub frames: Vec<Value>,
}

pub struct OmpRpc {
    child: Child,
    stdin: Option<ChildStdin>,
    frames: mpsc::Receiver<Result<Value, String>>,
    stdout_task: JoinHandle<()>,
    stderr_task: JoinHandle<()>,
    stderr: Arc<Mutex<String>>,
    next_id: u64,
}

impl Drop for OmpRpc {
    fn drop(&mut self) {
        self.stdout_task.abort();
        self.stderr_task.abort();
        let _ = self.child.start_kill();
    }
}

impl OmpRpc {
    pub async fn start(
        omp: impl AsRef<OsStr>,
        repo_root: impl AsRef<Path>,
        resume_session: Option<&Path>,
        control_socket: impl AsRef<Path>,
        item_id: &str,
        counter: i64,
    ) -> Result<Self> {
        let mut command = Command::new(omp);
        command
            .arg("--mode")
            .arg("rpc")
            .current_dir(repo_root.as_ref())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("AGENTFLOW_CONTROL_SOCKET", control_socket.as_ref())
            .env("AGENTFLOW_ITEM_ID", item_id)
            .env("AGENTFLOW_COUNTER", counter.to_string());

        if let Some(session) = resume_session {
            command.arg("--resume").arg(session);
        }

        let mut child = command.spawn().context("failed to start omp rpc process")?;
        let stdin = child.stdin.take().context("failed to open omp rpc stdin")?;
        let stdout = child
            .stdout
            .take()
            .context("failed to open omp rpc stdout")?;
        let stderr = child
            .stderr
            .take()
            .context("failed to open omp rpc stderr")?;

        let (sender, receiver) = mpsc::channel(64);
        let stdout_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        let parsed = serde_json::from_str::<Value>(&line).map_err(|error| {
                            format!("invalid json from omp stdout: {error}; line: {line}")
                        });
                        if sender.send(parsed).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        let _ = sender
                            .send(Err(format!("failed reading omp stdout: {error}")))
                            .await;
                        break;
                    }
                }
            }
        });

        let stderr_buffer = Arc::new(Mutex::new(String::new()));
        let stderr_for_task = Arc::clone(&stderr_buffer);
        let stderr_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        let mut buffer = stderr_for_task.lock().await;
                        if !buffer.is_empty() {
                            buffer.push('\n');
                        }
                        buffer.push_str(&line);
                    }
                    Ok(None) => break,
                    Err(error) => {
                        let mut buffer = stderr_for_task.lock().await;
                        if !buffer.is_empty() {
                            buffer.push('\n');
                        }
                        buffer.push_str("failed reading omp stderr: ");
                        buffer.push_str(&error.to_string());
                        break;
                    }
                }
            }
        });

        let mut rpc = Self {
            child,
            stdin: Some(stdin),
            frames: receiver,
            stdout_task,
            stderr_task,
            stderr: stderr_buffer,
            next_id: 1,
        };

        let ready = rpc.read_frame_with_timeout(READY_TIMEOUT).await?;
        if frame_type(&ready) != Some("ready") {
            bail!("expected omp ready frame, got {ready}");
        }

        Ok(rpc)
    }

    pub async fn command(&mut self, mut command: Value) -> Result<(Value, Vec<Value>)> {
        let object = command
            .as_object_mut()
            .context("rpc command must be a JSON object")?;
        let id = match object.get("id").and_then(Value::as_str) {
            Some(id) if !id.is_empty() => id.to_owned(),
            _ => {
                let command_name = object
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("command");
                let id = self.next_request_id(command_name);
                object.insert("id".to_owned(), Value::String(id.clone()));
                id
            }
        };

        self.write_frame(&command).await?;
        self.wait_for_command_response(&id).await
    }

    pub async fn prompt_and_wait(&mut self, rendered: &str) -> Result<PromptOutcome> {
        let id = self.next_request_id("prompt");
        let command = json!({
            "type": "prompt",
            "id": id,
            "message": rendered,
        });
        self.write_frame(&command).await?;

        let (ack, mut frames) = self.wait_for_prompt_ack(&id).await?;
        loop {
            let frame = self.read_frame_with_timeout(COMMAND_TIMEOUT).await?;
            let is_agent_end = frame_type(&frame) == Some("agent_end");
            frames.push(frame);
            if is_agent_end {
                break;
            }
        }

        Ok(PromptOutcome { ack, frames })
    }

    pub async fn get_state(&mut self) -> Result<SessionInfo> {
        let (info, _) = self.get_state_with_frames().await?;
        Ok(info)
    }
    pub async fn get_state_with_frames(&mut self) -> Result<(SessionInfo, Vec<Value>)> {
        let (response, mut frames) = self.command(json!({ "type": "get_state" })).await?;
        let info = session_info_from_response(&response)?;
        frames.push(response);
        Ok((info, frames))
    }

    async fn get_state_if_available(&mut self) -> Result<Option<SessionInfo>> {
        let (response, _) = self.command(json!({ "type": "get_state" })).await?;
        Ok(optional_session_info_from_response(&response))
    }

    pub async fn new_session(&mut self) -> Result<SessionInfo> {
        let (current, _) = self.new_session_with_frames().await?;
        Ok(current)
    }
    pub async fn new_session_with_frames(&mut self) -> Result<(SessionInfo, Vec<Value>)> {
        let previous = self.get_state_if_available().await?;
        let (response, mut frames) = self.command(json!({ "type": "new_session" })).await?;
        frames.push(response);
        let (current, state_frames) = self.get_state_with_frames().await?;
        frames.extend(state_frames);
        if let Some(previous) = previous {
            if previous.session_id == current.session_id {
                bail!("new_session did not change sessionId");
            }
        }
        Ok((current, frames))
    }

    pub async fn switch_session(&mut self, session_path: &Path) -> Result<SessionInfo> {
        let session_path_text = session_path.to_string_lossy();
        let _ = self
            .command(json!({
                "type": "switch_session",
                "sessionPath": session_path_text.as_ref(),
            }))
            .await?;
        let current = self.get_state().await?;
        if current.session_file.as_path() != session_path {
            bail!(
                "switch_session returned sessionFile {}, expected {}",
                current.session_file.display(),
                session_path.display()
            );
        }
        Ok(current)
    }

    pub async fn get_last_assistant_text(&mut self) -> Result<String> {
        let (response, _) = self
            .command(json!({ "type": "get_last_assistant_text" }))
            .await?;
        assistant_text_from_response(&response)
    }

    pub async fn close(mut self) -> Result<()> {
        if let Some(mut stdin) = self.stdin.take() {
            stdin
                .shutdown()
                .await
                .context("failed to close omp rpc stdin")?;
        }

        match timeout(CLOSE_TIMEOUT, self.child.wait()).await {
            Ok(status) => {
                status.context("failed waiting for omp rpc process")?;
            }
            Err(_) => {
                self.child
                    .start_kill()
                    .context("failed to terminate omp rpc process")?;
                let _ = timeout(KILL_TIMEOUT, self.child.wait()).await;
            }
        }

        self.stdout_task.abort();
        self.stderr_task.abort();
        Ok(())
    }

    fn next_request_id(&mut self, kind: &str) -> String {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        format!("{kind}-{id}")
    }

    async fn write_frame(&mut self, frame: &Value) -> Result<()> {
        let bytes = serde_json::to_vec(frame).context("failed to encode rpc frame")?;
        let stdin = self.stdin.as_mut().context("omp rpc stdin is closed")?;
        stdin
            .write_all(&bytes)
            .await
            .context("failed to write rpc frame")?;
        stdin
            .write_all(b"\n")
            .await
            .context("failed to write rpc newline")?;
        stdin.flush().await.context("failed to flush rpc command")
    }

    async fn wait_for_command_response(&mut self, id: &str) -> Result<(Value, Vec<Value>)> {
        let mut frames = Vec::new();
        loop {
            let frame = self.read_frame_with_timeout(COMMAND_TIMEOUT).await?;
            if frame_id(&frame) == Some(id) {
                ensure_success(&frame)?;
                return Ok((frame, frames));
            }
            frames.push(frame);
        }
    }

    async fn wait_for_prompt_ack(&mut self, id: &str) -> Result<(Value, Vec<Value>)> {
        let mut frames = Vec::new();
        loop {
            let frame = self.read_frame_with_timeout(COMMAND_TIMEOUT).await?;
            if frame_type(&frame) == Some("agent_end") {
                bail!("agent_end arrived before prompt ack for {id}");
            }
            if frame_id(&frame) == Some(id) {
                ensure_success(&frame)?;
                return Ok((frame, frames));
            }
            frames.push(frame);
        }
    }

    async fn read_frame_with_timeout(&mut self, duration: Duration) -> Result<Value> {
        match timeout(duration, self.frames.recv()).await {
            Ok(Some(Ok(frame))) => Ok(frame),
            Ok(Some(Err(error))) => bail!("{error}"),
            Ok(None) => {
                let stderr = self.stderr.lock().await;
                if stderr.is_empty() {
                    bail!("omp rpc process closed stdout");
                }
                bail!("omp rpc process closed stdout; stderr: {stderr}");
            }
            Err(_) => {
                let stderr = self.stderr.lock().await;
                if stderr.is_empty() {
                    bail!("timed out waiting for omp rpc frame");
                }
                bail!("timed out waiting for omp rpc frame; stderr: {stderr}");
            }
        }
    }
}

fn frame_type(frame: &Value) -> Option<&str> {
    frame.get("type").and_then(Value::as_str)
}

fn frame_id(frame: &Value) -> Option<&str> {
    frame.get("id").and_then(Value::as_str)
}

fn ensure_success(frame: &Value) -> Result<()> {
    match frame.get("success").and_then(Value::as_bool) {
        Some(false) => bail!("rpc command failed: {frame}"),
        _ => Ok(()),
    }
}

fn session_info_from_response(response: &Value) -> Result<SessionInfo> {
    let data = response
        .get("data")
        .and_then(Value::as_object)
        .context("get_state response missing data object")?;
    let session_id = required_non_empty_string(data.get("sessionId"), "data.sessionId")?;
    let session_file = required_non_empty_string(data.get("sessionFile"), "data.sessionFile")?;
    Ok(SessionInfo {
        session_id: session_id.to_owned(),
        session_file: PathBuf::from(session_file),
    })
}

fn optional_session_info_from_response(response: &Value) -> Option<SessionInfo> {
    let data = response.get("data").and_then(Value::as_object)?;
    let session_id = data.get("sessionId").and_then(Value::as_str)?;
    let session_file = data.get("sessionFile").and_then(Value::as_str)?;
    if session_id.is_empty() || session_file.is_empty() {
        return None;
    }
    Some(SessionInfo {
        session_id: session_id.to_owned(),
        session_file: PathBuf::from(session_file),
    })
}

fn assistant_text_from_response(response: &Value) -> Result<String> {
    if let Some(text) = response
        .get("data")
        .and_then(|data| data.get("text"))
        .and_then(Value::as_str)
    {
        return Ok(text.to_owned());
    }
    if let Some(text) = response.get("data").and_then(Value::as_str) {
        return Ok(text.to_owned());
    }
    if let Some(text) = response.get("text").and_then(Value::as_str) {
        return Ok(text.to_owned());
    }
    Err(anyhow!("get_last_assistant_text response missing text"))
}

fn required_non_empty_string<'a>(value: Option<&'a Value>, field: &str) -> Result<&'a str> {
    let text = value
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing {field}"))?;
    if text.is_empty() {
        bail!("{field} is empty");
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_info_from_response_rejects_missing_session_file() {
        let response = json!({ "success": true, "data": { "sessionId": "s1" } });

        assert!(session_info_from_response(&response).is_err());
    }

    #[test]
    fn assistant_text_from_response_accepts_strict_shape() -> Result<()> {
        let response = json!({ "success": true, "data": { "text": "hello" } });

        assert_eq!(assistant_text_from_response(&response)?, "hello");
        Ok(())
    }

    #[test]
    fn ensure_success_rejects_false_response() {
        let response = json!({ "id": "req-1", "success": false });

        assert!(ensure_success(&response).is_err());
    }
}
