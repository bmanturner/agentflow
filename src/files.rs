use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use tokio::io::AsyncWriteExt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentFlowPaths {
    pub repo_root: PathBuf,
    pub agentflow_dir: PathBuf,
    pub state_path: PathBuf,
    pub runs_dir: PathBuf,
}

impl AgentFlowPaths {
    pub fn new(repo_root: impl Into<PathBuf>, state_override: Option<PathBuf>) -> Self {
        let repo_root = repo_root.into();
        let agentflow_dir = repo_root.join(".agentflow");
        let state_path = state_override.unwrap_or_else(|| agentflow_dir.join("state.json"));
        let runs_dir = agentflow_dir.join("runs");

        Self {
            repo_root,
            agentflow_dir,
            state_path,
            runs_dir,
        }
    }

    pub fn run_dir(&self, item_id: &str) -> PathBuf {
        self.runs_dir.join(item_id)
    }

    pub fn rendered_prompts_path(&self, item_id: &str) -> PathBuf {
        self.run_dir(item_id).join("rendered-prompts.jsonl")
    }

    pub fn outputs_path(&self, item_id: &str) -> PathBuf {
        self.run_dir(item_id).join("outputs.jsonl")
    }

    pub fn sessions_path(&self, item_id: &str) -> PathBuf {
        self.run_dir(item_id).join("sessions.json")
    }
}

pub async fn append_jsonl<T>(path: &Path, value: &T) -> Result<()>
where
    T: Serialize + ?Sized,
{
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .with_context(|| format!("failed to open JSONL file {}", path.display()))?;

    let line = serde_json::to_vec(value).context("failed to serialize JSONL record")?;
    file.write_all(&line)
        .await
        .with_context(|| format!("failed to append JSONL record to {}", path.display()))?;
    file.write_all(b"\n")
        .await
        .with_context(|| format!("failed to append JSONL newline to {}", path.display()))?;
    file.flush()
        .await
        .with_context(|| format!("failed to flush JSONL file {}", path.display()))?;

    Ok(())
}

pub async fn append_output<T>(paths: &AgentFlowPaths, item_id: &str, record: &T) -> Result<()>
where
    T: Serialize + ?Sized,
{
    append_jsonl(&paths.outputs_path(item_id), record).await
}

pub async fn append_rendered_prompt<T>(
    paths: &AgentFlowPaths,
    item_id: &str,
    record: &T,
) -> Result<()>
where
    T: Serialize + ?Sized,
{
    append_jsonl(&paths.rendered_prompts_path(item_id), record).await
}

pub async fn record_session<T>(paths: &AgentFlowPaths, item_id: &str, record: &T) -> Result<()>
where
    T: Serialize + ?Sized,
{
    let path = paths.sessions_path(item_id);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }

    let mut records = match tokio::fs::read(&path).await {
        Ok(bytes) => serde_json::from_slice::<Vec<serde_json::Value>>(&bytes)
            .with_context(|| format!("failed to parse sessions file {}", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read sessions file {}", path.display()))
        }
    };

    records.push(serde_json::to_value(record).context("failed to serialize session record")?);

    let bytes = serde_json::to_vec_pretty(&records).context("failed to serialize sessions file")?;
    let tmp_path = path.with_extension("json.tmp");
    let mut file = tokio::fs::File::create(&tmp_path).await.with_context(|| {
        format!(
            "failed to create temporary sessions file {}",
            tmp_path.display()
        )
    })?;
    file.write_all(&bytes).await.with_context(|| {
        format!(
            "failed to write temporary sessions file {}",
            tmp_path.display()
        )
    })?;
    file.write_all(b"\n").await.with_context(|| {
        format!(
            "failed to write temporary sessions file {}",
            tmp_path.display()
        )
    })?;
    file.flush().await.with_context(|| {
        format!(
            "failed to flush temporary sessions file {}",
            tmp_path.display()
        )
    })?;
    file.sync_all().await.with_context(|| {
        format!(
            "failed to sync temporary sessions file {}",
            tmp_path.display()
        )
    })?;
    drop(file);

    tokio::fs::rename(&tmp_path, &path)
        .await
        .with_context(|| format!("failed to replace sessions file {}", path.display()))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn paths_should_match_agentflow_layout_without_override() {
        let paths = AgentFlowPaths::new(PathBuf::from("/repo"), None);

        assert_eq!(paths.agentflow_dir, PathBuf::from("/repo/.agentflow"));
        assert_eq!(
            paths.state_path,
            PathBuf::from("/repo/.agentflow/state.json")
        );
        assert_eq!(
            paths.run_dir("M14"),
            PathBuf::from("/repo/.agentflow/runs/M14")
        );
        assert_eq!(
            paths.rendered_prompts_path("M14"),
            PathBuf::from("/repo/.agentflow/runs/M14/rendered-prompts.jsonl")
        );
        assert_eq!(
            paths.outputs_path("M14"),
            PathBuf::from("/repo/.agentflow/runs/M14/outputs.jsonl")
        );
        assert_eq!(
            paths.sessions_path("M14"),
            PathBuf::from("/repo/.agentflow/runs/M14/sessions.json")
        );
    }

    #[test]
    fn paths_should_use_state_override() {
        let paths = AgentFlowPaths::new(
            PathBuf::from("/repo"),
            Some(PathBuf::from("/tmp/state.json")),
        );

        assert_eq!(paths.state_path, PathBuf::from("/tmp/state.json"));
        assert_eq!(paths.runs_dir, PathBuf::from("/repo/.agentflow/runs"));
    }

    #[tokio::test]
    async fn append_jsonl_should_create_parent_and_append_compact_json() -> Result<()> {
        let tempdir = tempfile::tempdir()?;
        let path = tempdir.path().join(".agentflow/runs/M14/outputs.jsonl");

        append_jsonl(&path, &json!({ "kind": "first" })).await?;
        append_jsonl(&path, &json!({ "kind": "second" })).await?;

        let text = tokio::fs::read_to_string(path).await?;
        assert_eq!(text, "{\"kind\":\"first\"}\n{\"kind\":\"second\"}\n");
        Ok(())
    }

    #[tokio::test]
    async fn record_session_should_rewrite_readable_json_array() -> Result<()> {
        let tempdir = tempfile::tempdir()?;
        let paths = AgentFlowPaths::new(tempdir.path(), None);

        record_session(&paths, "M14", &json!({ "session_id": "one" })).await?;
        record_session(&paths, "M14", &json!({ "session_id": "two" })).await?;

        let text = tokio::fs::read_to_string(paths.sessions_path("M14")).await?;
        let records: Vec<serde_json::Value> = serde_json::from_str(&text)?;
        assert_eq!(
            records,
            vec![
                json!({ "session_id": "one" }),
                json!({ "session_id": "two" })
            ]
        );
        assert!(text.starts_with("[\n"));
        Ok(())
    }
}
