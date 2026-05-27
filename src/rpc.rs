use std::env;
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
use tokio::time::{timeout, Instant};

const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(300);
const DEFAULT_PROMPT_IDLE_TIMEOUT: Duration = Duration::from_secs(1800);
const READY_TIMEOUT_ENV: &str = "AGENTFLOW_OMP_READY_TIMEOUT_SECONDS";
const COMMAND_TIMEOUT_ENV: &str = "AGENTFLOW_OMP_COMMAND_TIMEOUT_SECONDS";
const PROMPT_IDLE_TIMEOUT_ENV: &str = "AGENTFLOW_OMP_PROMPT_IDLE_TIMEOUT_SECONDS";
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
const STDERR_BUFFER_LIMIT: usize = 1024 * 1024;
const KILL_TIMEOUT: Duration = Duration::from_secs(5);
const RETRY_INITIAL_DELAY: Duration = Duration::from_secs(1);
const RETRY_MAX_DELAY: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub session_id: String,
    pub session_file: PathBuf,
}

#[derive(Debug)]
pub struct PromptOutcome {
    pub ack: Option<Value>,
    pub frames: Vec<Value>,
}

enum RpcResponse {
    Success { response: Value, frames: Vec<Value> },
    RetryableBusy { response: Value, frames: Vec<Value> },
}

#[derive(Debug, Clone, Copy)]
struct RpcTimeouts {
    ready: Duration,
    command: Duration,
    prompt_idle: Duration,
}

impl RpcTimeouts {
    fn from_env() -> Result<Self> {
        Ok(Self {
            ready: duration_from_env(READY_TIMEOUT_ENV, DEFAULT_READY_TIMEOUT)?,
            command: duration_from_env(COMMAND_TIMEOUT_ENV, DEFAULT_COMMAND_TIMEOUT)?,
            prompt_idle: duration_from_env(PROMPT_IDLE_TIMEOUT_ENV, DEFAULT_PROMPT_IDLE_TIMEOUT)?,
        })
    }

    const fn get(self, kind: TimeoutKind) -> Duration {
        match kind {
            TimeoutKind::Ready => self.ready,
            TimeoutKind::CommandResponse => self.command,
            TimeoutKind::PromptIdle => self.prompt_idle,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum TimeoutKind {
    Ready,
    CommandResponse,
    PromptIdle,
}

impl TimeoutKind {
    const fn description(self) -> &'static str {
        match self {
            Self::Ready => "ready frame",
            Self::CommandResponse => "command response frame",
            Self::PromptIdle => "prompt progress or completion frame",
        }
    }

    const fn env_var(self) -> &'static str {
        match self {
            Self::Ready => READY_TIMEOUT_ENV,
            Self::CommandResponse => COMMAND_TIMEOUT_ENV,
            Self::PromptIdle => PROMPT_IDLE_TIMEOUT_ENV,
        }
    }
}

pub struct OmpRpc {
    child: Child,
    stdin: Option<ChildStdin>,
    frames: mpsc::Receiver<Result<Value, String>>,
    stdout_task: JoinHandle<()>,
    stderr_task: JoinHandle<()>,
    stderr: Arc<Mutex<String>>,
    timeouts: RpcTimeouts,
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
        persist_session: bool,
    ) -> Result<Self> {
        let timeouts = RpcTimeouts::from_env()?;

        let mut command = Command::new(omp);
        command
            .arg("--mode")
            .arg("rpc")
            .current_dir(repo_root.as_ref())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .env("AGENTFLOW_CONTROL_SOCKET", control_socket.as_ref())
            .env("AGENTFLOW_ITEM_ID", item_id)
            .env("AGENTFLOW_COUNTER", counter.to_string());

        if let Some(session) = resume_session {
            command.arg("--resume").arg(session);
        } else if should_disable_omp_session(resume_session, persist_session) {
            command.arg("--no-session");
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
                        append_capped_stderr_line(&mut buffer, &line);
                    }
                    Ok(None) => break,
                    Err(error) => {
                        let mut buffer = stderr_for_task.lock().await;
                        append_capped_stderr_line(
                            &mut buffer,
                            &format!("failed reading omp stderr: {error}"),
                        );
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
            timeouts,
            next_id: 1,
        };

        let ready = rpc.read_frame_with_timeout(TimeoutKind::Ready).await?;
        if frame_type(&ready) != Some("ready") {
            bail!("expected omp ready frame, got {ready}");
        }

        Ok(rpc)
    }

    pub async fn command(
        &mut self,
        mut command: Value,
        collect_frames: bool,
    ) -> Result<(Value, Vec<Value>)> {
        let (command_name, caller_supplied_id) = {
            let object = command
                .as_object_mut()
                .context("rpc command must be a JSON object")?;
            let command_name = object
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("command")
                .to_owned();
            let caller_supplied_id = object
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| !id.is_empty());
            (command_name, caller_supplied_id)
        };
        let mut collected_frames = Vec::new();
        let retry_deadline = retry_deadline(self.timeouts.prompt_idle);
        let mut retry_count = 0_usize;

        loop {
            let id = {
                let object = command
                    .as_object_mut()
                    .context("rpc command must be a JSON object")?;
                if caller_supplied_id {
                    object
                        .get("id")
                        .and_then(Value::as_str)
                        .context("rpc command id must be a string")?
                        .to_owned()
                } else {
                    let id = self.next_request_id(&command_name);
                    object.insert("id".to_owned(), Value::String(id.clone()));
                    id
                }
            };

            self.write_frame(&command).await?;
            match self.wait_for_command_response(&id, collect_frames).await? {
                RpcResponse::Success { response, frames } => {
                    if collect_frames {
                        collected_frames.extend(frames);
                    }
                    return Ok((response, collected_frames));
                }
                RpcResponse::RetryableBusy { response, frames } => {
                    if collect_frames {
                        collected_frames.extend(frames);
                        collected_frames.push(response);
                    }
                    self.wait_before_retry(
                        retry_deadline,
                        retry_count,
                        collect_frames,
                        &mut collected_frames,
                    )
                    .await?;
                    retry_count = retry_count.saturating_add(1);
                }
            }
        }
    }

    pub async fn prompt_and_wait(
        &mut self,
        rendered: &str,
        collect_frames: bool,
    ) -> Result<PromptOutcome> {
        let mut frames = Vec::new();
        let retry_deadline = retry_deadline(self.timeouts.prompt_idle);
        let mut retry_count = 0_usize;

        let ack = loop {
            let id = self.next_request_id("prompt");
            let command = json!({
                "type": "prompt",
                "id": id,
                "message": rendered,
            });
            self.write_frame(&command).await?;

            match self.wait_for_prompt_ack(&id, collect_frames).await? {
                RpcResponse::Success {
                    response,
                    frames: ack_frames,
                } => {
                    if collect_frames {
                        frames.extend(ack_frames);
                        break Some(response);
                    }
                    break None;
                }
                RpcResponse::RetryableBusy {
                    response,
                    frames: ack_frames,
                } => {
                    if collect_frames {
                        frames.extend(ack_frames);
                        frames.push(response);
                    }
                    self.wait_before_retry(
                        retry_deadline,
                        retry_count,
                        collect_frames,
                        &mut frames,
                    )
                    .await?;
                    retry_count = retry_count.saturating_add(1);
                }
            }
        };

        loop {
            let frame = self
                .read_frame_with_timeout(TimeoutKind::PromptIdle)
                .await?;
            let is_agent_end = frame_type(&frame) == Some("agent_end");
            if collect_frames {
                frames.push(frame);
            }
            if is_agent_end {
                break;
            }
        }

        Ok(PromptOutcome { ack, frames })
    }

    pub async fn get_state(&mut self) -> Result<SessionInfo> {
        let (info, _) = self.get_state_with_frames(false).await?;
        Ok(info)
    }
    pub async fn get_state_with_frames(
        &mut self,
        collect_frames: bool,
    ) -> Result<(SessionInfo, Vec<Value>)> {
        let (response, mut frames) = self
            .command(json!({ "type": "get_state" }), collect_frames)
            .await?;
        let info = session_info_from_response(&response)?;
        if collect_frames {
            frames.push(response);
        }
        Ok((info, frames))
    }

    async fn get_state_if_available(&mut self) -> Result<Option<SessionInfo>> {
        let (response, _) = self.command(json!({ "type": "get_state" }), false).await?;
        Ok(optional_session_info_from_response(&response))
    }

    pub async fn new_session(&mut self) -> Result<SessionInfo> {
        let (current, _) = self.new_session_with_frames(false).await?;
        Ok(current)
    }
    pub async fn new_session_with_frames(
        &mut self,
        collect_frames: bool,
    ) -> Result<(SessionInfo, Vec<Value>)> {
        let previous = self.get_state_if_available().await?;
        let (response, mut frames) = self
            .command(json!({ "type": "new_session" }), collect_frames)
            .await?;
        if collect_frames {
            frames.push(response);
        }
        let (current, state_frames) = self.get_state_with_frames(collect_frames).await?;
        if collect_frames {
            frames.extend(state_frames);
        }
        if let Some(previous) = previous {
            if previous.session_id == current.session_id {
                bail!("new_session did not change sessionId");
            }
        }
        Ok((current, frames))
    }

    pub async fn new_session_without_state(&mut self, collect_frames: bool) -> Result<Vec<Value>> {
        let (response, mut frames) = self
            .command(json!({ "type": "new_session" }), collect_frames)
            .await?;
        if collect_frames {
            frames.push(response);
        }
        Ok(frames)
    }

    pub async fn switch_session(&mut self, session_path: &Path) -> Result<SessionInfo> {
        let session_path_text = session_path.to_string_lossy();
        let _ = self
            .command(
                json!({
                    "type": "switch_session",
                    "sessionPath": session_path_text.as_ref(),
                }),
                false,
            )
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
            .command(json!({ "type": "get_last_assistant_text" }), false)
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

    async fn wait_for_command_response(
        &mut self,
        id: &str,
        collect_frames: bool,
    ) -> Result<RpcResponse> {
        let mut frames = Vec::new();
        loop {
            let frame = self
                .read_frame_with_timeout(TimeoutKind::CommandResponse)
                .await?;
            if frame_id(&frame) == Some(id) {
                return response_status(frame, frames);
            }
            if collect_frames {
                frames.push(frame);
            }
        }
    }

    async fn wait_for_prompt_ack(&mut self, id: &str, collect_frames: bool) -> Result<RpcResponse> {
        let mut frames = Vec::new();
        loop {
            let frame = self
                .read_frame_with_timeout(TimeoutKind::CommandResponse)
                .await?;
            if frame_type(&frame) == Some("agent_end") {
                bail!("agent_end arrived before prompt ack for {id}");
            }
            if frame_id(&frame) == Some(id) {
                return response_status(frame, frames);
            }
            if collect_frames {
                frames.push(frame);
            }
        }
    }

    async fn wait_before_retry(
        &mut self,
        deadline: Instant,
        retry_count: usize,
        collect_frames: bool,
        frames: &mut Vec<Value>,
    ) -> Result<()> {
        let delay = retry_delay(retry_count);
        let now = Instant::now();
        if now >= deadline {
            bail!(
                "omp rpc remained busy for {} seconds",
                self.timeouts.prompt_idle.as_secs()
            );
        }
        let retry_at = (now + delay).min(deadline);

        loop {
            let remaining = retry_at.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(());
            }
            match timeout(remaining, self.frames.recv()).await {
                Ok(Some(Ok(frame))) => {
                    if collect_frames {
                        frames.push(frame);
                    }
                }
                Ok(Some(Err(error))) => bail!("{error}"),
                Ok(None) => {
                    let stderr = self.stderr.lock().await;
                    if stderr.is_empty() {
                        bail!("omp rpc process closed stdout while waiting to retry busy command");
                    }
                    bail!(
                        "omp rpc process closed stdout while waiting to retry busy command; stderr: {stderr}"
                    );
                }
                Err(_) => return Ok(()),
            }
        }
    }

    async fn read_frame_with_timeout(&mut self, kind: TimeoutKind) -> Result<Value> {
        let duration = self.timeouts.get(kind);
        match timeout(duration, self.frames.recv()).await {
            Ok(Some(Ok(frame))) => Ok(frame),
            Ok(Some(Err(error))) => bail!("{error}"),
            Ok(None) => {
                let stderr = self.stderr.lock().await;
                if stderr.is_empty() {
                    bail!(
                        "omp rpc process closed stdout while waiting for {}",
                        kind.description()
                    );
                }
                bail!(
                    "omp rpc process closed stdout while waiting for {}; stderr: {stderr}",
                    kind.description()
                );
            }
            Err(_) => {
                let stderr = self.stderr.lock().await;
                let seconds = duration.as_secs();
                if stderr.is_empty() {
                    bail!(
                        "timed out after {seconds}s waiting for omp rpc {}; set {} to a larger number of seconds if OMP is still working",
                        kind.description(),
                        kind.env_var()
                    );
                }
                bail!(
                    "timed out after {seconds}s waiting for omp rpc {}; stderr: {stderr}; set {} to a larger number of seconds if OMP is still working",
                    kind.description(),
                    kind.env_var()
                );
            }
        }
    }
}
fn append_capped_stderr_line(buffer: &mut String, line: &str) {
    if !buffer.is_empty() {
        buffer.push('\n');
    }
    buffer.push_str(line);
    if buffer.len() <= STDERR_BUFFER_LIMIT {
        return;
    }

    const TRUNCATED_MARKER: &str = "[stderr truncated]\n";
    let tail_len = STDERR_BUFFER_LIMIT.saturating_sub(TRUNCATED_MARKER.len());
    let mut tail_start = buffer.len().saturating_sub(tail_len);
    while tail_start < buffer.len() && !buffer.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let tail = buffer.split_off(tail_start);
    buffer.clear();
    buffer.push_str(TRUNCATED_MARKER);
    buffer.push_str(&tail);
}

fn response_status(response: Value, frames: Vec<Value>) -> Result<RpcResponse> {
    if response.get("success").and_then(Value::as_bool) == Some(false) {
        if is_retryable_busy_response(&response) {
            return Ok(RpcResponse::RetryableBusy { response, frames });
        }
        bail!("rpc command failed: {response}");
    }

    Ok(RpcResponse::Success { response, frames })
}

fn is_retryable_busy_response(response: &Value) -> bool {
    response.get("success").and_then(Value::as_bool) == Some(false)
        && response
            .get("error")
            .and_then(Value::as_str)
            .is_some_and(is_retryable_busy_error)
}

fn is_retryable_busy_error(error: &str) -> bool {
    error.contains("already processing") || error.contains("Already processing")
}

fn should_disable_omp_session(resume_session: Option<&Path>, persist_session: bool) -> bool {
    resume_session.is_none() && !persist_session
}

fn retry_deadline(duration: Duration) -> Instant {
    Instant::now() + duration
}

fn retry_delay(retry_count: usize) -> Duration {
    let multiplier = 1_u64 << retry_count.min(4);
    let seconds = RETRY_INITIAL_DELAY
        .as_secs()
        .saturating_mul(multiplier)
        .min(RETRY_MAX_DELAY.as_secs());
    Duration::from_secs(seconds)
}

fn duration_from_env(name: &'static str, default: Duration) -> Result<Duration> {
    match env::var(name) {
        Ok(value) => parse_timeout_seconds(name, &value),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(env::VarError::NotUnicode(_)) => bail!("{name} must be valid UTF-8"),
    }
}

fn parse_timeout_seconds(name: &str, value: &str) -> Result<Duration> {
    let seconds = value
        .trim()
        .parse::<u64>()
        .with_context(|| format!("{name} must be a positive integer number of seconds"))?;
    if seconds == 0 {
        bail!("{name} must be greater than zero");
    }
    Ok(Duration::from_secs(seconds))
}

fn frame_type(frame: &Value) -> Option<&str> {
    frame.get("type").and_then(Value::as_str)
}

fn frame_id(frame: &Value) -> Option<&str> {
    frame.get("id").and_then(Value::as_str)
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
    fn response_status_marks_busy_failure_retryable() -> Result<()> {
        let response = json!({
            "id": "prompt-90",
            "command": "prompt",
            "success": false,
            "error": "Agent is already processing. Use steer() or followUp() to queue messages, or wait for completion.",
        });

        let status = response_status(response, Vec::new())?;

        assert!(matches!(status, RpcResponse::RetryableBusy { .. }));
        Ok(())
    }

    #[test]
    fn response_status_rejects_non_retryable_failure() {
        let response = json!({
            "id": "prompt-91",
            "command": "prompt",
            "success": false,
            "error": "malformed prompt",
        });

        assert!(response_status(response, Vec::new()).is_err());
    }

    #[test]
    fn retry_delay_should_exponentially_back_off_and_cap() {
        assert_eq!(retry_delay(0), Duration::from_secs(1));
        assert_eq!(retry_delay(1), Duration::from_secs(2));
        assert_eq!(retry_delay(4), Duration::from_secs(15));
        assert_eq!(retry_delay(10), Duration::from_secs(15));
    }

    #[test]
    fn should_disable_omp_session_only_without_resume_or_persistence() {
        assert!(should_disable_omp_session(None, false));
        assert!(!should_disable_omp_session(None, true));
        assert!(!should_disable_omp_session(
            Some(Path::new("/tmp/session.jsonl")),
            false
        ));
    }

    #[test]
    fn parse_timeout_seconds_accepts_positive_integer() -> Result<()> {
        assert_eq!(parse_timeout_seconds("TEST_TIMEOUT", " 42 ")?.as_secs(), 42);
        Ok(())
    }

    #[test]
    fn parse_timeout_seconds_rejects_zero() {
        let error = parse_timeout_seconds("TEST_TIMEOUT", "0").unwrap_err();

        assert!(error.to_string().contains("greater than zero"));
    }

    #[test]
    fn parse_timeout_seconds_rejects_non_integer() {
        let error = parse_timeout_seconds("TEST_TIMEOUT", "five").unwrap_err();

        assert!(error
            .to_string()
            .contains("positive integer number of seconds"));
    }
}
