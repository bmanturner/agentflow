use std::env;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use crate::cli::{Cli, Command, RunArgs};
use crate::config::{apply_overrides, load_config, validate_config, Config, LoopOverrides};
use crate::control::{send_control_event, start_control_server};
use crate::files::{append_output, append_rendered_prompt, record_session, AgentFlowPaths};
use crate::loop_spec::{build_loop_plan, LoopItem, LoopPlan};
use crate::notify::run_notify_command;
use crate::open::open_session;
use crate::rpc::{OmpRpc, PromptOutcome, SessionInfo};
use crate::smoke::run_smoke_test;
use crate::state::{
    load_state, require_state, save_state, ControlEvent, CurrentState, RunStatus, State,
};
use crate::template::{build_context, render_message, render_prompt};

pub async fn main_entry() -> Result<()> {
    let cli = Cli::parse();
    let repo_root = cli.repo_root.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize repo root {}",
            cli.repo_root.display()
        )
    })?;
    let config_path = resolve_path(&repo_root, &cli.config);
    let paths = AgentFlowPaths::new(
        repo_root.clone(),
        cli.state.map(|path| resolve_path(&repo_root, &path)),
    );

    match cli.command {
        Command::Run(args) => run_command(&cli.omp, &repo_root, &config_path, &paths, args).await,
        Command::Status => status_command(&paths.state_path).await,
        Command::Open => open_command(&cli.omp, &paths.state_path).await,
        Command::Resume(args) => {
            resume_command(&cli.omp, &repo_root, &config_path, &paths, args.message).await
        }
        Command::Notify(args) => control_command(ControlKind::Notify, args.message).await,
        Command::Halt(args) => control_command(ControlKind::Halt, args.message).await,
        Command::SmokeTest => run_smoke_test(&cli.omp, &repo_root).await,
    }
}

fn resolve_path(repo_root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo_root.join(path)
    }
}

async fn run_command(
    omp: &str,
    repo_root: &Path,
    config_path: &Path,
    paths: &AgentFlowPaths,
    args: RunArgs,
) -> Result<()> {
    refuse_conflicting_state(&paths.state_path).await?;
    let mut config = load_and_validate_config(config_path, repo_root, args).await?;
    let plan = build_loop_plan(&config, repo_root)?;
    if plan.items.is_empty() {
        bail!("loop produced no items");
    }
    let run_id = new_run_id(&plan.items[0].item_id)?;
    drive_flow(DriveRequest {
        omp,
        repo_root,
        config_path,
        paths,
        config: &mut config,
        plan: &plan,
        run_id,
        start_item_index: 0,
        start_prompt_index: 0,
        resume_session: None,
        resume_message: None,
    })
    .await
}

async fn resume_command(
    omp: &str,
    repo_root: &Path,
    config_path: &Path,
    paths: &AgentFlowPaths,
    message: Option<String>,
) -> Result<()> {
    let state = require_state(&paths.state_path).await?;
    ensure_resumable_status(state.status)?;
    let session_file = resume_session_file(paths, &state.current).await?;
    let mut config = load_and_validate_config(config_path, repo_root, RunArgs::default()).await?;
    let plan = build_loop_plan(&config, repo_root)?;
    let (item_index, prompt_index) = find_resume_position(&plan, &state.current)?;

    drive_flow(DriveRequest {
        omp,
        repo_root,
        config_path,
        paths,
        config: &mut config,
        plan: &plan,
        run_id: state.run_id,
        start_item_index: item_index,
        start_prompt_index: prompt_index,
        resume_session: Some(session_file),
        resume_message: message,
    })
    .await
}

async fn status_command(state_path: &Path) -> Result<()> {
    let Some(state) = load_state(state_path).await? else {
        println!("no active agentflow run");
        return Ok(());
    };

    println!("status: {:?}", state.status);
    println!("run_id: {}", state.run_id);
    println!("item_id: {}", state.current.item_id);
    println!("counter: {}", state.current.counter);
    println!("iteration: {}", state.current.iteration);
    println!("prompt_index: {}", state.current.prompt_index);
    println!("prompt_id: {}", state.current.prompt_id);
    if let Some(session_file) = state.current.session_file.as_ref() {
        println!("session_file: {}", session_file.display());
    }
    if let Some(event) = state.pending_control.as_ref() {
        println!("pending_control: {}", serde_json::to_string(event)?);
    }
    if let Some(error) = state.last_error.as_ref() {
        println!("last_error: {error}");
    }
    Ok(())
}

async fn open_command(omp: &str, state_path: &Path) -> Result<()> {
    let state = require_state(state_path).await?;
    if state.status != RunStatus::Paused {
        bail!(
            "cannot open run with status {:?}; only paused runs can be opened",
            state.status
        );
    }
    let session_file = state
        .current
        .session_file
        .as_deref()
        .ok_or_else(|| anyhow!("paused state is missing current.session_file"))?;
    let status = open_session(omp, &state.repo_root, session_file).await?;
    if !status.success() {
        bail!("omp --resume exited with status {status}");
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum ControlKind {
    Notify,
    Halt,
}

async fn control_command(kind: ControlKind, message: String) -> Result<()> {
    let socket = env::var("AGENTFLOW_CONTROL_SOCKET")
        .context("AGENTFLOW_CONTROL_SOCKET is not set; no active AgentFlow run is reachable")?;
    if socket.is_empty() {
        bail!("AGENTFLOW_CONTROL_SOCKET is empty");
    }
    let event = match kind {
        ControlKind::Notify => ControlEvent::Notify {
            message: message.clone(),
            item_id: None,
            counter: None,
        },
        ControlKind::Halt => ControlEvent::Halt {
            message: message.clone(),
            item_id: None,
            counter: None,
        },
    };
    send_control_event(Path::new(&socket), &event).await?;
    println!("{message}");
    Ok(())
}

fn ensure_resumable_status(status: RunStatus) -> Result<()> {
    if matches!(status, RunStatus::Paused | RunStatus::Failed) {
        return Ok(());
    }
    bail!("cannot resume run with status {status:?}; only paused or failed runs can resume")
}

async fn resume_session_file(paths: &AgentFlowPaths, current: &CurrentState) -> Result<PathBuf> {
    if let Some(session_file) = current.session_file.as_ref() {
        return Ok(session_file.clone());
    }

    latest_recorded_session_file(paths, &current.item_id)
        .await?
        .ok_or_else(|| {
            anyhow!(
                "resumable state is missing current.session_file and no recorded session exists for {}",
                current.item_id
            )
        })
}

async fn latest_recorded_session_file(
    paths: &AgentFlowPaths,
    item_id: &str,
) -> Result<Option<PathBuf>> {
    let path = paths.sessions_path(item_id);
    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read sessions file {}", path.display()))
        }
    };
    let records = serde_json::from_slice::<Vec<Value>>(&bytes)
        .with_context(|| format!("failed to parse sessions file {}", path.display()))?;

    Ok(records
        .iter()
        .rev()
        .filter_map(|record| record.get("session_file").and_then(Value::as_str))
        .find(|session_file| !session_file.is_empty())
        .map(PathBuf::from))
}

async fn refuse_conflicting_state(state_path: &Path) -> Result<()> {
    let Some(state) = load_state(state_path).await? else {
        return Ok(());
    };
    if matches!(state.status, RunStatus::Running | RunStatus::Paused) {
        bail!(
            "refusing to start: existing AgentFlow run '{}' is {:?}; use status/open/resume",
            state.run_id,
            state.status
        );
    }
    Ok(())
}

async fn load_and_validate_config(
    config_path: &Path,
    repo_root: &Path,
    args: RunArgs,
) -> Result<Config> {
    let mut config = load_config(config_path).await?;
    let overrides = LoopOverrides {
        start: args.start,
        end: args.end,
        count: args.count,
    };
    apply_overrides(&mut config, &overrides);
    validate_config(&config, repo_root)?;
    Ok(config)
}

struct DriveRequest<'a> {
    omp: &'a str,
    repo_root: &'a Path,
    config_path: &'a Path,
    paths: &'a AgentFlowPaths,
    config: &'a mut Config,
    plan: &'a LoopPlan,
    run_id: String,
    start_item_index: usize,
    start_prompt_index: usize,
    resume_session: Option<PathBuf>,
    resume_message: Option<String>,
}

async fn drive_flow(request: DriveRequest<'_>) -> Result<()> {
    let socket_path = env::temp_dir().join(format!(
        "agentflow-{}.sock",
        sanitize_run_id(&request.run_id)
    ));
    let mut control_server = start_control_server(socket_path.clone()).await?;
    let mut control_rx = control_server.take_receiver()?;
    let first_item = request
        .plan
        .items
        .get(request.start_item_index)
        .ok_or_else(|| anyhow!("resume item index is out of range"))?;
    let mut rpc = OmpRpc::start(
        request.omp,
        request.repo_root,
        request.resume_session.as_deref(),
        &socket_path,
        &first_item.item_id,
        first_item.counter,
    )
    .await?;

    let result = drive_flow_inner(&mut rpc, &mut control_rx, &request).await;
    match result {
        Ok(DriveEnd::Completed) => {
            let state = completed_state(&request, first_item);
            save_state(&request.paths.state_path, &state).await?;
            let _ = rpc.close().await;
            control_server.shutdown().await?;
            Ok(())
        }
        Ok(DriveEnd::Stopped) => {
            let _ = rpc.close().await;
            control_server.shutdown().await?;
            Ok(())
        }
        Err(error) => {
            let _ = persist_failure(&request, first_item, &error.to_string()).await;
            let _ = rpc.close().await;
            let _ = control_server.shutdown().await;
            Err(error)
        }
    }
}

enum DriveEnd {
    Completed,
    Stopped,
}

async fn prompt_with_progress(
    rpc: &mut OmpRpc,
    rendered: &str,
    item: &LoopItem,
    prompt_index: usize,
    total_prompts: usize,
    prompt_id: &str,
) -> Result<PromptOutcome> {
    let mut tick = 0_usize;
    let mut ticker = tokio::time::interval(Duration::from_millis(120));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    write_progress_line(item, prompt_index, total_prompts, prompt_id, tick)?;

    let prompt = rpc.prompt_and_wait(rendered);
    tokio::pin!(prompt);

    loop {
        tokio::select! {
            result = &mut prompt => {
                clear_progress_line()?;
                match &result {
                    Ok(_) => eprintln!(
                        "✓ {} step {}/{} complete: {}",
                        item.item_id,
                        display_step(prompt_index, total_prompts),
                        total_prompts.max(1),
                        prompt_id
                    ),
                    Err(_) => eprintln!(
                        "✗ {} step {}/{} failed: {}",
                        item.item_id,
                        display_step(prompt_index, total_prompts),
                        total_prompts.max(1),
                        prompt_id
                    ),
                }
                return result;
            }
            _ = ticker.tick() => {
                tick = tick.wrapping_add(1);
                write_progress_line(item, prompt_index, total_prompts, prompt_id, tick)?;
            }
        }
    }
}

fn write_progress_line(
    item: &LoopItem,
    prompt_index: usize,
    total_prompts: usize,
    prompt_id: &str,
    tick: usize,
) -> Result<()> {
    const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let spinner = SPINNER[tick % SPINNER.len()];
    let bar = progress_bar(prompt_index, total_prompts, 24);
    eprint!(
        "\r\x1b[2K{spinner} {} {bar} step {}/{}: {}",
        item.item_id,
        display_step(prompt_index, total_prompts),
        total_prompts.max(1),
        prompt_id
    );
    io::stderr().flush()?;
    Ok(())
}

fn clear_progress_line() -> Result<()> {
    eprint!("\r\x1b[2K");
    io::stderr().flush()?;
    Ok(())
}

fn display_step(prompt_index: usize, total_prompts: usize) -> usize {
    prompt_index.saturating_add(1).min(total_prompts.max(1))
}

fn progress_bar(prompt_index: usize, total_prompts: usize, width: usize) -> String {
    let total = total_prompts.max(1);
    let completed = display_step(prompt_index, total);
    let filled = (completed * width).div_ceil(total);
    let empty = width.saturating_sub(filled);
    format!("[{}{}]", "█".repeat(filled), "░".repeat(empty))
}

async fn drive_flow_inner(
    rpc: &mut OmpRpc,
    control_rx: &mut mpsc::Receiver<ControlEvent>,
    request: &DriveRequest<'_>,
) -> Result<DriveEnd> {
    let mut session: Option<SessionInfo> = None;
    let mut force_new_session = request.resume_session.is_some() && request.start_prompt_index == 0;
    if request.resume_session.is_some() {
        let item = request
            .plan
            .items
            .get(request.start_item_index)
            .ok_or_else(|| anyhow!("resume item index is out of range"))?;
        let (resumed_session, frames) = rpc.get_state_with_frames().await?;
        append_rpc_frames(request.paths, &item.item_id, &frames).await?;
        record_session_info(
            request.paths,
            item,
            current_prompt_id(request.config, request.start_prompt_index),
            &resumed_session,
            "resume",
        )
        .await?;
        session = Some(resumed_session);
    }

    if let Some(message) = request.resume_message.as_deref() {
        let item = request
            .plan
            .items
            .get(request.start_item_index)
            .ok_or_else(|| anyhow!("resume item index is out of range"))?;
        let current = current_state(
            item,
            request.start_prompt_index,
            request.config,
            session.as_ref(),
        );
        save_state(
            &request.paths.state_path,
            &running_state(request, current, None),
        )
        .await?;
        let outcome = prompt_with_progress(
            rpc,
            message,
            item,
            request.start_prompt_index,
            request.config.prompts.len(),
            "resume-message",
        )
        .await?;
        append_rpc_frame(request.paths, &item.item_id, &outcome.ack).await?;
        append_rpc_frames(request.paths, &item.item_id, &outcome.frames).await?;
        let pending = drain_control_events(control_rx, None);
        if let Some(event) = pending {
            return handle_control_stop(
                request,
                rpc,
                item,
                request.start_prompt_index,
                session.as_ref(),
                event,
            )
            .await;
        }
    }

    for item_index in request.start_item_index..request.plan.items.len() {
        let item = &request.plan.items[item_index];
        let first_prompt = if item_index == request.start_item_index {
            request.start_prompt_index
        } else {
            0
        };

        if item_index > request.start_item_index && first_prompt == 0 {
            force_new_session = true;
        }

        for prompt_index in first_prompt..request.config.prompts.len() {
            let prompt = &request.config.prompts[prompt_index];
            let mut pending_control = drain_control_events(control_rx, None);
            if let Some(event) = pending_control.take() {
                return handle_control_stop(
                    request,
                    rpc,
                    item,
                    prompt_index,
                    session.as_ref(),
                    event,
                )
                .await;
            }

            let need_new_session = force_new_session || (session.is_some() && prompt.new_session);
            if need_new_session {
                let (new_session, frames) = rpc.new_session_with_frames().await?;
                append_rpc_frames(request.paths, &item.item_id, &frames).await?;
                record_session_info(
                    request.paths,
                    item,
                    prompt.id.as_str(),
                    &new_session,
                    "new_session",
                )
                .await?;
                session = Some(new_session);
                force_new_session = false;
            } else if session.is_none() {
                let (initial_session, frames) = rpc.get_state_with_frames().await?;
                append_rpc_frames(request.paths, &item.item_id, &frames).await?;
                record_session_info(
                    request.paths,
                    item,
                    prompt.id.as_str(),
                    &initial_session,
                    "initial",
                )
                .await?;
                session = Some(initial_session);
            }

            let current = current_state(item, prompt_index, request.config, session.as_ref());
            save_state(
                &request.paths.state_path,
                &running_state(request, current, None),
            )
            .await?;

            if let Some(event) = drain_control_events(control_rx, None) {
                return handle_control_stop(
                    request,
                    rpc,
                    item,
                    prompt_index,
                    session.as_ref(),
                    event,
                )
                .await;
            }

            let context = build_context(item, request.repo_root, None);
            let rendered = render_prompt(prompt, &context)?;
            append_rendered_prompt(
                request.paths,
                &item.item_id,
                &json!({
                    "run_id": request.run_id,
                    "item_id": item.item_id,
                    "counter": item.counter,
                    "iteration": item.iteration,
                    "prompt_index": prompt_index,
                    "prompt_id": prompt.id,
                    "session_id": session.as_ref().map(|s| s.session_id.as_str()),
                    "session_file": session.as_ref().map(|s| s.session_file.to_string_lossy().into_owned()),
                    "text": rendered,
                }),
            )
            .await?;

            if let Some(event) = drain_control_events(control_rx, None) {
                return handle_control_stop(
                    request,
                    rpc,
                    item,
                    prompt_index,
                    session.as_ref(),
                    event,
                )
                .await;
            }

            let outcome = prompt_with_progress(
                rpc,
                &rendered,
                item,
                prompt_index,
                request.config.prompts.len(),
                &prompt.id,
            )
            .await?;
            append_rpc_frame(request.paths, &item.item_id, &outcome.ack).await?;
            append_rpc_frames(request.paths, &item.item_id, &outcome.frames).await?;

            let mut control = drain_control_events(control_rx, None);
            if prompt.pause_after && control.is_none() {
                let message_template = prompt
                    .message
                    .as_deref()
                    .ok_or_else(|| anyhow!("pause_after prompt '{}' has no message", prompt.id))?;
                let message = render_message(message_template, &context)?;
                control = Some(ControlEvent::Notify {
                    message,
                    item_id: Some(item.item_id.clone()),
                    counter: Some(item.counter),
                });
            }

            if let Some(event) = control {
                return handle_control_stop(
                    request,
                    rpc,
                    item,
                    prompt_index + 1,
                    session.as_ref(),
                    event,
                )
                .await;
            }
        }
        session = None;
        force_new_session = true;
    }

    Ok(DriveEnd::Completed)
}

async fn handle_control_stop(
    request: &DriveRequest<'_>,
    _rpc: &mut OmpRpc,
    item: &LoopItem,
    next_prompt_index: usize,
    session: Option<&SessionInfo>,
    event: ControlEvent,
) -> Result<DriveEnd> {
    let event = control_event_for_item(event, item);
    let message = control_message(&event);
    let context = build_context(item, request.repo_root, Some(message));
    let notify_error =
        run_notify_command(request.config.notify.as_ref(), &context, message).await?;
    if let Some(error) = notify_error.as_ref() {
        append_output(
            request.paths,
            &item.item_id,
            &json!({ "kind": "error", "message": error }),
        )
        .await?;
    }

    match event {
        ControlEvent::Halt { .. } => {
            let current = current_state(
                item,
                next_prompt_index.saturating_sub(1),
                request.config,
                session,
            );
            let state = State {
                schema_version: 1,
                run_id: request.run_id.clone(),
                status: RunStatus::Halted,
                repo_root: request.repo_root.to_path_buf(),
                config_path: request.config_path.to_path_buf(),
                current,
                pending_control: Some(event),
                last_error: notify_error.clone(),
            };
            append_transition(
                request.paths,
                &item.item_id,
                RunStatus::Running,
                RunStatus::Halted,
                "halt",
            )
            .await?;
            save_state(&request.paths.state_path, &state).await?;
        }
        notify @ ControlEvent::Notify { .. } => {
            let (pause_item, pause_prompt_index) =
                next_position_or_current(request.plan, request.config, item, next_prompt_index);
            let current = current_state(pause_item, pause_prompt_index, request.config, session);
            let state = State {
                schema_version: 1,
                run_id: request.run_id.clone(),
                status: RunStatus::Paused,
                repo_root: request.repo_root.to_path_buf(),
                config_path: request.config_path.to_path_buf(),
                current,
                pending_control: Some(notify),
                last_error: notify_error,
            };
            append_transition(
                request.paths,
                &item.item_id,
                RunStatus::Running,
                RunStatus::Paused,
                "notify",
            )
            .await?;
            save_state(&request.paths.state_path, &state).await?;
        }
    }
    Ok(DriveEnd::Stopped)
}

fn next_position_or_current<'a>(
    plan: &'a LoopPlan,
    config: &Config,
    item: &'a LoopItem,
    next_prompt_index: usize,
) -> (&'a LoopItem, usize) {
    if next_prompt_index < config.prompts.len() {
        return (item, next_prompt_index);
    }
    if let Some(current_index) = plan.items.iter().position(|candidate| candidate == item) {
        if let Some(next_item) = plan.items.get(current_index + 1) {
            return (next_item, 0);
        }
    }
    (item, next_prompt_index)
}

fn drain_control_events(
    receiver: &mut mpsc::Receiver<ControlEvent>,
    mut pending: Option<ControlEvent>,
) -> Option<ControlEvent> {
    while let Ok(event) = receiver.try_recv() {
        merge_control_event(&mut pending, event);
    }
    pending
}

fn merge_control_event(pending: &mut Option<ControlEvent>, event: ControlEvent) {
    match event {
        halt @ ControlEvent::Halt { .. } => *pending = Some(halt),
        notify @ ControlEvent::Notify { .. } => {
            if !matches!(pending, Some(ControlEvent::Halt { .. })) {
                *pending = Some(notify);
            }
        }
    }
}

fn control_message(event: &ControlEvent) -> &str {
    match event {
        ControlEvent::Notify { message, .. } | ControlEvent::Halt { message, .. } => message,
    }
}

fn control_event_for_item(event: ControlEvent, item: &LoopItem) -> ControlEvent {
    match event {
        ControlEvent::Notify { message, .. } => ControlEvent::Notify {
            message,
            item_id: Some(item.item_id.clone()),
            counter: Some(item.counter),
        },
        ControlEvent::Halt { message, .. } => ControlEvent::Halt {
            message,
            item_id: Some(item.item_id.clone()),
            counter: Some(item.counter),
        },
    }
}

fn current_prompt_id(config: &Config, prompt_index: usize) -> &str {
    config
        .prompts
        .get(prompt_index)
        .map(|prompt| prompt.id.as_str())
        .unwrap_or("<complete>")
}

fn current_state(
    item: &LoopItem,
    prompt_index: usize,
    config: &Config,
    session: Option<&SessionInfo>,
) -> CurrentState {
    let prompt_id = config
        .prompts
        .get(prompt_index)
        .map(|prompt| prompt.id.clone())
        .unwrap_or_else(|| "<complete>".to_owned());
    CurrentState {
        counter: item.counter,
        item_id: item.item_id.clone(),
        iteration: item.iteration,
        prompt_index,
        prompt_id,
        session_id: session.map(|session| session.session_id.clone()),
        session_file: session.map(|session| session.session_file.clone()),
    }
}

fn running_state(
    request: &DriveRequest<'_>,
    current: CurrentState,
    pending: Option<ControlEvent>,
) -> State {
    State {
        schema_version: 1,
        run_id: request.run_id.clone(),
        status: RunStatus::Running,
        repo_root: request.repo_root.to_path_buf(),
        config_path: request.config_path.to_path_buf(),
        current,
        pending_control: pending,
        last_error: None,
    }
}

fn completed_state(request: &DriveRequest<'_>, item: &LoopItem) -> State {
    State {
        schema_version: 1,
        run_id: request.run_id.clone(),
        status: RunStatus::Completed,
        repo_root: request.repo_root.to_path_buf(),
        config_path: request.config_path.to_path_buf(),
        current: current_state(
            item,
            request.config.prompts.len().saturating_sub(1),
            request.config,
            None,
        ),
        pending_control: None,
        last_error: None,
    }
}

async fn persist_failure(request: &DriveRequest<'_>, item: &LoopItem, error: &str) -> Result<()> {
    let previous = load_state(&request.paths.state_path).await?;
    let current = previous
        .as_ref()
        .filter(|state| state.run_id == request.run_id)
        .map(|state| CurrentState {
            counter: state.current.counter,
            item_id: state.current.item_id.clone(),
            iteration: state.current.iteration,
            prompt_index: state.current.prompt_index,
            prompt_id: state.current.prompt_id.clone(),
            session_id: state.current.session_id.clone(),
            session_file: state.current.session_file.clone(),
        })
        .unwrap_or_else(|| current_state(item, request.start_prompt_index, request.config, None));
    let pending_control = previous
        .filter(|state| state.run_id == request.run_id)
        .and_then(|state| state.pending_control);
    let state = State {
        schema_version: 1,
        run_id: request.run_id.clone(),
        status: RunStatus::Failed,
        repo_root: request.repo_root.to_path_buf(),
        config_path: request.config_path.to_path_buf(),
        current,
        pending_control,
        last_error: Some(error.to_owned()),
    };
    save_state(&request.paths.state_path, &state).await
}

async fn append_rpc_frame(paths: &AgentFlowPaths, item_id: &str, frame: &Value) -> Result<()> {
    append_output(
        paths,
        item_id,
        &json!({ "kind": "omp_frame", "frame": frame }),
    )
    .await
}

async fn append_rpc_frames(paths: &AgentFlowPaths, item_id: &str, frames: &[Value]) -> Result<()> {
    for frame in frames {
        append_rpc_frame(paths, item_id, frame).await?;
    }
    Ok(())
}

async fn append_transition(
    paths: &AgentFlowPaths,
    item_id: &str,
    from: RunStatus,
    to: RunStatus,
    reason: &str,
) -> Result<()> {
    append_output(
        paths,
        item_id,
        &json!({
            "kind": "state_transition",
            "from": format!("{from:?}"),
            "to": format!("{to:?}"),
            "reason": reason,
        }),
    )
    .await
}

async fn record_session_info(
    paths: &AgentFlowPaths,
    item: &LoopItem,
    prompt_id: &str,
    session: &SessionInfo,
    reason: &str,
) -> Result<()> {
    record_session(
        paths,
        &item.item_id,
        &SessionRecord {
            prompt_id,
            session_id: &session.session_id,
            session_file: session.session_file.to_string_lossy().as_ref(),
            started_at: unix_millis_string()?,
            reason,
        },
    )
    .await
}

#[derive(Serialize)]
struct SessionRecord<'a> {
    prompt_id: &'a str,
    session_id: &'a str,
    session_file: &'a str,
    started_at: String,
    reason: &'a str,
}

fn find_resume_position(plan: &LoopPlan, current: &CurrentState) -> Result<(usize, usize)> {
    let item_index = plan
        .items
        .iter()
        .position(|item| {
            item.counter == current.counter
                && item.item_id == current.item_id
                && item.iteration == current.iteration
        })
        .ok_or_else(|| {
            anyhow!(
                "saved state item '{}' is not present in current loop",
                current.item_id
            )
        })?;
    Ok((item_index, current.prompt_index))
}

fn new_run_id(first_item_id: &str) -> Result<String> {
    Ok(format!("{}-{first_item_id}", unix_millis_string()?))
}

fn unix_millis_string() -> Result<String> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before UNIX_EPOCH")?;
    Ok(duration.as_millis().to_string())
}

fn sanitize_run_id(run_id: &str) -> String {
    run_id
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::config::{Config, LoopConfig, PromptStep, Provider};

    use super::*;

    fn test_config() -> Config {
        Config {
            provider: Provider::Omp,
            loop_: LoopConfig {
                start: 1,
                end: None,
                count: Some(2),
                step: 1,
                item_id: "M{{counter}}".to_owned(),
            },
            notify: None,
            prompts: vec![
                PromptStep {
                    id: "plan".to_owned(),
                    text: "plan".to_owned(),
                    new_session: false,
                    pause_after: false,
                    message: None,
                },
                PromptStep {
                    id: "implement".to_owned(),
                    text: "implement".to_owned(),
                    new_session: true,
                    pause_after: false,
                    message: None,
                },
            ],
        }
    }

    fn test_plan() -> LoopPlan {
        LoopPlan {
            items: vec![
                LoopItem {
                    counter: 1,
                    item_id: "M1".to_owned(),
                    iteration: 1,
                    iteration_index: 0,
                },
                LoopItem {
                    counter: 2,
                    item_id: "M2".to_owned(),
                    iteration: 2,
                    iteration_index: 1,
                },
            ],
        }
    }

    #[test]
    fn next_position_should_preserve_terminal_review_pause_without_replay_clamp() {
        let config = test_config();
        let plan = test_plan();
        let (item, prompt_index) =
            next_position_or_current(&plan, &config, &plan.items[1], config.prompts.len());

        assert_eq!(item.item_id, "M2");
        assert_eq!(prompt_index, config.prompts.len());
        assert_eq!(
            current_state(item, prompt_index, &config, None).prompt_id,
            "<complete>"
        );
    }

    #[test]
    fn next_position_should_advance_to_next_item_after_item_prompt_end() {
        let config = test_config();
        let plan = test_plan();
        let (item, prompt_index) =
            next_position_or_current(&plan, &config, &plan.items[0], config.prompts.len());

        assert_eq!(item.item_id, "M2");
        assert_eq!(prompt_index, 0);
    }

    #[test]
    fn control_event_for_item_should_replace_missing_or_stale_metadata() {
        let item = LoopItem {
            counter: 2,
            item_id: "M2".to_owned(),
            iteration: 2,
            iteration_index: 1,
        };
        let event = ControlEvent::Notify {
            message: "pause".to_owned(),
            item_id: Some("M1".to_owned()),
            counter: Some(1),
        };

        assert_eq!(
            control_event_for_item(event, &item),
            ControlEvent::Notify {
                message: "pause".to_owned(),
                item_id: Some("M2".to_owned()),
                counter: Some(2),
            }
        );
    }

    #[test]
    fn ensure_resumable_status_should_accept_paused_and_failed() -> Result<()> {
        ensure_resumable_status(RunStatus::Paused)?;
        ensure_resumable_status(RunStatus::Failed)?;

        Ok(())
    }

    #[test]
    fn ensure_resumable_status_should_reject_terminal_and_running_states() {
        assert!(ensure_resumable_status(RunStatus::Running).is_err());
        assert!(ensure_resumable_status(RunStatus::Halted).is_err());
        assert!(ensure_resumable_status(RunStatus::Completed).is_err());
    }

    #[tokio::test]
    async fn resume_session_file_should_fall_back_to_latest_session_record() -> Result<()> {
        let tempdir = tempfile::tempdir()?;
        let paths = AgentFlowPaths::new(tempdir.path().to_path_buf(), None);
        let sessions_path = paths.sessions_path("M1");
        if let Some(parent) = sessions_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(
            &sessions_path,
            serde_json::to_vec(&vec![
                serde_json::json!({ "session_file": "/tmp/old-session.jsonl" }),
                serde_json::json!({ "session_file": "/tmp/latest-session.jsonl" }),
            ])?,
        )
        .await?;
        let current = CurrentState {
            counter: 1,
            item_id: "M1".to_owned(),
            iteration: 1,
            prompt_index: 0,
            prompt_id: "plan".to_owned(),
            session_id: None,
            session_file: None,
        };

        let session_file = resume_session_file(&paths, &current).await?;

        assert_eq!(session_file, PathBuf::from("/tmp/latest-session.jsonl"));
        Ok(())
    }

    #[test]
    fn find_resume_position_should_accept_complete_prompt_index() {
        let plan = test_plan();
        let current = CurrentState {
            counter: 2,
            item_id: "M2".to_owned(),
            iteration: 2,
            prompt_index: 2,
            prompt_id: "<complete>".to_owned(),
            session_id: None,
            session_file: Some(PathBuf::from("/tmp/session.jsonl")),
        };

        let position = find_resume_position(&plan, &current).expect("position should resolve");
        assert_eq!(position, (1, 2));
    }

    #[tokio::test]
    async fn persist_failure_should_preserve_saved_resume_position() -> Result<()> {
        let tempdir = tempfile::tempdir()?;
        let repo_root = tempdir.path().to_path_buf();
        let config_path = repo_root.join(".agentflow.yml");
        let paths = AgentFlowPaths::new(repo_root.clone(), None);
        let mut config = test_config();
        let plan = test_plan();
        let session_file = PathBuf::from("/tmp/recover-session.jsonl");
        let state = State {
            schema_version: 1,
            run_id: "run-1".to_owned(),
            status: RunStatus::Running,
            repo_root: repo_root.clone(),
            config_path: config_path.clone(),
            current: CurrentState {
                counter: 2,
                item_id: "M2".to_owned(),
                iteration: 2,
                prompt_index: 1,
                prompt_id: "implement".to_owned(),
                session_id: Some("session-2".to_owned()),
                session_file: Some(session_file.clone()),
            },
            pending_control: None,
            last_error: None,
        };
        save_state(&paths.state_path, &state).await?;
        let request = DriveRequest {
            omp: "omp",
            repo_root: &repo_root,
            config_path: &config_path,
            paths: &paths,
            config: &mut config,
            plan: &plan,
            run_id: "run-1".to_owned(),
            start_item_index: 0,
            start_prompt_index: 0,
            resume_session: None,
            resume_message: None,
        };

        persist_failure(&request, &plan.items[0], "rpc timeout").await?;
        let failed = load_state(&paths.state_path)
            .await?
            .expect("state should exist");

        assert_eq!(failed.status, RunStatus::Failed);
        assert_eq!(failed.current.item_id, "M2");
        assert_eq!(failed.current.prompt_index, 1);
        assert_eq!(failed.current.session_file, Some(session_file));
        assert_eq!(failed.last_error.as_deref(), Some("rpc timeout"));
        Ok(())
    }

    #[test]
    fn progress_bar_should_fill_by_current_step() {
        assert_eq!(progress_bar(0, 5, 10), "[██░░░░░░░░]");
        assert_eq!(progress_bar(2, 5, 10), "[██████░░░░]");
        assert_eq!(progress_bar(4, 5, 10), "[██████████]");
    }
}
