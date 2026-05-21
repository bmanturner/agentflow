use std::path::Path;
use std::process::ExitStatus;

use anyhow::{Context, Result};
use tokio::process::Command;

pub async fn open_session(omp: &str, repo_root: &Path, session_file: &Path) -> Result<ExitStatus> {
    Command::new(omp)
        .arg("--resume")
        .arg(session_file)
        .current_dir(repo_root)
        .status()
        .await
        .with_context(|| {
            format!(
                "failed to run {} --resume {} from {}",
                omp,
                session_file.display(),
                repo_root.display()
            )
        })
}
