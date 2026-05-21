use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct State {
    pub schema_version: u32,
    pub run_id: String,
    pub status: RunStatus,
    pub repo_root: PathBuf,
    pub config_path: PathBuf,
    pub current: CurrentState,
    pub pending_control: Option<ControlEvent>,
    pub last_error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Paused,
    Halted,
    Failed,
    Completed,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CurrentState {
    pub counter: i64,
    pub item_id: String,
    pub iteration: u64,
    pub prompt_index: usize,
    pub prompt_id: String,
    pub session_id: Option<String>,
    pub session_file: Option<PathBuf>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlEvent {
    Notify {
        message: String,
        item_id: Option<String>,
        counter: Option<i64>,
    },
    Halt {
        message: String,
        item_id: Option<String>,
        counter: Option<i64>,
    },
}

pub async fn load_state(path: &Path) -> Result<Option<State>> {
    match tokio::fs::read(path).await {
        Ok(bytes) => {
            let state = serde_json::from_slice(&bytes)
                .with_context(|| format!("failed to parse state file {}", path.display()))?;
            Ok(Some(state))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("failed to read state file {}", path.display()))
        }
    }
}

pub async fn require_state(path: &Path) -> Result<State> {
    load_state(path)
        .await?
        .with_context(|| format!("state file not found: {}", path.display()))
}

pub async fn save_state(path: &Path, state: &State) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(state).context("failed to serialize state")?;

    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create state directory {}", parent.display()))?;
    }

    let tmp_path = path.with_extension(
        match path.extension().and_then(|extension| extension.to_str()) {
            Some(extension) => format!("{extension}.tmp"),
            None => String::from("tmp"),
        },
    );

    let mut file = tokio::fs::File::create(&tmp_path).await.with_context(|| {
        format!(
            "failed to create temporary state file {}",
            tmp_path.display()
        )
    })?;
    file.write_all(&bytes).await.with_context(|| {
        format!(
            "failed to write temporary state file {}",
            tmp_path.display()
        )
    })?;
    file.write_all(b"\n").await.with_context(|| {
        format!(
            "failed to write temporary state file {}",
            tmp_path.display()
        )
    })?;
    file.flush().await.with_context(|| {
        format!(
            "failed to flush temporary state file {}",
            tmp_path.display()
        )
    })?;
    file.sync_all()
        .await
        .with_context(|| format!("failed to sync temporary state file {}", tmp_path.display()))?;
    drop(file);

    tokio::fs::rename(&tmp_path, path)
        .await
        .with_context(|| format!("failed to replace state file {}", path.display()))?;

    sync_parent_dir(path).await;

    Ok(())
}

#[cfg(unix)]
async fn sync_parent_dir(path: &Path) {
    if let Some(parent) = path.parent() {
        // Best-effort: the state file is already durable; directory fsync makes the
        // rename durable on Unix filesystems but should not mask a successful save.
        if let Ok(directory) = tokio::fs::File::open(parent).await {
            let _ = directory.sync_all().await;
        }
    }
}

#[cfg(not(unix))]
async fn sync_parent_dir(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_state(run_id: &str, status: RunStatus) -> State {
        State {
            schema_version: 1,
            run_id: run_id.to_owned(),
            status,
            repo_root: PathBuf::from("/repo"),
            config_path: PathBuf::from("/repo/.agentflow.yml"),
            current: CurrentState {
                counter: 14,
                item_id: String::from("M14"),
                iteration: 1,
                prompt_index: 0,
                prompt_id: String::from("plan"),
                session_id: Some(String::from("session-1")),
                session_file: Some(PathBuf::from("/tmp/session.json")),
            },
            pending_control: Some(ControlEvent::Notify {
                message: String::from("review"),
                item_id: Some(String::from("M14")),
                counter: Some(14),
            }),
            last_error: None,
        }
    }

    #[tokio::test]
    async fn save_state_should_round_trip_pretty_json() -> Result<()> {
        let tempdir = tempfile::tempdir()?;
        let path = tempdir.path().join(".agentflow/state.json");
        let state = sample_state("run-1", RunStatus::Paused);

        save_state(&path, &state).await?;
        let saved = tokio::fs::read_to_string(&path).await?;
        let loaded = load_state(&path).await?;

        assert!(saved.contains("\n  \"schema_version\": 1,"));
        assert_eq!(loaded, Some(state));
        Ok(())
    }

    #[tokio::test]
    async fn save_state_should_replace_existing_state_atomically() -> Result<()> {
        let tempdir = tempfile::tempdir()?;
        let path = tempdir.path().join("state.json");
        let first = sample_state("run-1", RunStatus::Running);
        let second = sample_state("run-2", RunStatus::Completed);

        save_state(&path, &first).await?;
        save_state(&path, &second).await?;

        assert_eq!(load_state(&path).await?, Some(second));
        Ok(())
    }

    #[tokio::test]
    async fn load_state_should_return_none_when_missing() -> Result<()> {
        let tempdir = tempfile::tempdir()?;
        let path = tempdir.path().join("missing-state.json");

        assert_eq!(load_state(&path).await?, None);
        Ok(())
    }
}
