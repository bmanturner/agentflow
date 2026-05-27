use std::path::Path;

use anyhow::{bail, Result};

use crate::rpc::OmpRpc;

pub async fn run_smoke_test(omp: &str, repo_root: &Path) -> Result<()> {
    let socket = std::env::temp_dir().join("agentflow-smoke-test.sock");
    let mut rpc = OmpRpc::start(omp, repo_root, None, &socket, "SMOKE", 0, true).await?;

    let initial = rpc.get_state().await?;
    let token = "AGENTFLOW_SMOKE_TEST";

    rpc.prompt_and_wait(
        &format!("Remember {token}. Reply with exactly: remembered {token}."),
        false,
    )
    .await?;
    rpc.prompt_and_wait(
        "What token did I ask you to remember? Reply with only the token.",
        false,
    )
    .await?;
    let same_session_text = rpc.get_last_assistant_text().await?;
    if !same_session_text.contains(token) {
        bail!(
            "same-session smoke test failed; response did not contain {token}: {same_session_text}"
        );
    }

    let new_session = rpc.new_session().await?;
    if new_session.session_id == initial.session_id {
        bail!("new_session did not change sessionId");
    }

    rpc.prompt_and_wait(
        "What token did I ask you to remember? Reply briefly.",
        false,
    )
    .await?;
    let new_session_text = rpc.get_last_assistant_text().await?;
    if new_session_text.contains(token) {
        bail!("new-session smoke test failed; response still contained prior token: {new_session_text}");
    }

    rpc.switch_session(&initial.session_file).await?;

    rpc.close().await?;
    println!("AgentFlow OMP smoke test passed");
    Ok(())
}
