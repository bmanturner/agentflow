# AgentFlow MVP Implementation Plan

## Purpose

This plan describes how to implement the AgentFlow MVP specified in `AGENTFLOW.md` as a Rust CLI. Together, `AGENTFLOW.md` and this file should be sufficient to build, test, and ship the MVP without adding workflow-engine behavior or speculative features.

## Non-goals

Do not implement these in the MVP:

- Workflow DAGs, branching, formal milestones, or implementation-plan models.
- Databases or durable job queues.
- Structured question models.
- Separate prompt files.
- Codex support.
- Multiple providers.
- Background daemons.
- Desktop notification libraries such as `notify-rust`; use `notify.command` only.
- Shell-form execution of configured commands.

## Target behavior summary

AgentFlow runs a counter-driven sequence of prompt steps against OMP RPC.

For each loop item:

1. Render `item_id` from the current counter.
2. Start with no active session for the item.
3. Start or reuse one OMP RPC session across prompt steps until a step sets `new_session: true`.
4. For each prompt step:
   - ensure the intended OMP session is active;
   - render the prompt text;
   - send the prompt over OMP RPC;
   - wait for prompt ack;
   - record all OMP events;
   - wait for `agent_end`;
   - apply any pending `notify`, `halt`, or `pause_after` state transition.
5. Increment the counter only after all prompts for the item complete without pause, halt, or failure.

Pause is a process boundary:

- AgentFlow persists state.
- AgentFlow closes the OMP RPC child process.
- `agentflow open` runs interactive `omp --resume <sessionFile>`.
- `agentflow resume` starts fresh `omp --mode rpc --resume <sessionFile>` and continues from the next prompt.

## Crate setup

Create a standard Rust binary crate.

Recommended `Cargo.toml` dependencies:

```toml
[package]
name = "agentflow"
version = "0.1.0"
edition = "2021"

[dependencies]
anyhow = "1"
clap = { version = "4", features = ["derive"] }
handlebars = "6"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
serde_yaml = "0.9"
thiserror = "2"
tokio = { version = "1", features = ["fs", "io-util", "macros", "net", "process", "rt-multi-thread", "signal", "time"] }

[dev-dependencies]
tempfile = "3"
```

Keep dependencies boring and minimal. Add other crates only if there is a concrete implementation need that cannot be solved cleanly with the standard library or the crates above.

## Module layout

Use explicit modules instead of a large `main.rs`.

```text
src/
  main.rs          # clap parse, top-level dispatch, error printing
  cli.rs           # command structs/enums
  config.rs        # .agentflow.yml structs, loading, validation
  template.rs      # Handlebars rendering context and strict rendering
  loop_spec.rs     # loop expansion and counter iteration
  state.rs         # .agentflow/state.json model and atomic persistence
  files.rs         # repo root paths, .agentflow paths, JSONL append helpers
  rpc.rs           # OMP RPC child process and JSONL protocol
  control.rs       # Unix socket server/client and control event types
  runner.rs        # run/resume orchestration state machine
  notify.rs        # external notify.command argv execution
  open.rs          # interactive omp --resume
  smoke.rs         # smoke test command implementation
  error.rs         # typed internal errors
```

Keep responsibilities narrow:

- `rpc.rs` must know OMP RPC wire behavior, but not loop semantics.
- `runner.rs` must know loop/session/pause semantics, but not socket framing details.
- `config.rs` must validate user config, but not start OMP.
- `state.rs` must persist and load state, but not decide transitions.

## CLI contract

Implement these commands:

```bash
agentflow run
agentflow run --start 14 --count 35
agentflow run --start 14 --end 35
agentflow status
agentflow open
agentflow resume
agentflow resume "..."
agentflow notify --message "..."
agentflow halt --message "..."
agentflow smoke-test
```

`smoke-test` is not listed in the PRD CLI block, but the PRD requires a smoke test. Add it as an explicit command instead of hiding it behind `run`.

### Global options

Support these options on relevant commands:

```text
--config <path>    default: .agentflow.yml
--repo-root <path> default: current directory canonicalized
--omp <path>       default: omp
--state <path>     default: <repo-root>/.agentflow/state.json
```

Keep defaults simple. Do not discover parent directories unless explicitly added later.

### Command behavior

#### `run`

- Load and validate config.
- Apply optional `--start`, `--end`, `--count` overrides after loading config and before validation of loop bounds.
- Refuse to start if `.agentflow/state.json` exists with status `running` or `paused`; print a clear message telling the user to use `status`, `open`, or `resume`.
- Create a new `run_id`.
- Start at prompt index `0` for the first loop item.
- Create `.agentflow/` and `.agentflow/runs/<item_id>/` as needed.

#### `status`

- Read `.agentflow/state.json`.
- If absent, report no active run and exit `0`.
- Print: status, run id, item id, counter, prompt id/index, session file, pending control event, and last error.
- Do not contact OMP.

#### `open`

- Load state.
- Require status `paused`.
- Require `current.session_file`.
- Run interactively from `repo_root`:

```bash
omp --resume <sessionFile>
```

- Return OMP's exit status.
- Do not mutate state unless the session file is missing from state, which is an error.

#### `resume`

Two forms:

```bash
agentflow resume
agentflow resume "message"
```

Common behavior:

- Load and validate config.
- Load state.
- Require status `paused`.
- Require `current.session_file`.
- Start fresh RPC with:

```bash
omp --mode rpc --resume <sessionFile>
```

- Wait for `ready`.
- Call `get_state` and record current `sessionId` / `sessionFile`.

No-message behavior:

- Continue with the next configured prompt without sending a user message.

Message behavior:

- Send the provided message to the active resumed session.
- Wait for ack and `agent_end`.
- If no new notify/halt occurs, continue with the next configured prompt.

#### `notify`

- Read `AGENTFLOW_CONTROL_SOCKET`.
- Fail non-zero if unset or empty.
- Send JSON event:

```json
{ "type": "notify", "message": "...", "item_id": "...", "counter": 14 }
```

- `item_id` and `counter` come from `AGENTFLOW_ITEM_ID` / `AGENTFLOW_COUNTER` when present.
- Print the message to stdout.
- Exit `0` only after the orchestrator accepts the event.

#### `halt`

Same as `notify`, but event type is `halt`.

#### `smoke-test`

Implement the PRD smoke test exactly:

1. Start OMP RPC and wait for `ready`.
2. Call `get_state`, record `sessionId` / `sessionFile`.
3. Send `Remember AGENTFLOW_SMOKE_TEST.`
4. Wait for prompt ack, then `agent_end`.
5. Send `What token did I ask you to remember?`
6. Wait for `agent_end`, then call `get_last_assistant_text`.
7. Pass only if response contains `AGENTFLOW_SMOKE_TEST`.
8. Send `new_session`.
9. Call `get_state`; pass only if `sessionId` changed.
10. Ask again.
11. Wait for `agent_end`, then call `get_last_assistant_text`.
12. Pass only if the response does not rely on the previous session.

For step 12, treat these as failures:

- response contains `AGENTFLOW_SMOKE_TEST`;
- response confidently states the token from the prior session.

## Config model

YAML shape:

```rust
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    provider: Provider,
    loop_: LoopConfig, // serde rename = "loop"
    notify: Option<NotifyConfig>,
    prompts: Vec<PromptStep>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Provider { Omp }

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LoopConfig {
    start: i64,
    end: Option<i64>,
    count: Option<u64>,
    step: i64,
    item_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NotifyConfig {
    command: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptStep {
    id: String,
    text: String,
    #[serde(default)]
    new_session: bool,
    #[serde(default)]
    pause_after: bool,
    message: Option<String>,
}
```

Use `#[serde(rename = "loop")]` for the field in `Config`.

Validation rules:

- `provider == omp`.
- Exactly one of `loop.end` / `loop.count` is set.
- `loop.count > 0` if set.
- `loop.step > 0`.
- `loop.end >= loop.start` when using positive step.
- `prompts` is non-empty.
- Prompt ids are non-empty and unique.
- Prompt text is non-empty after trimming.
- `pause_after: true` requires non-empty `message`.
- `notify.command`, if present, is non-empty and has no empty argv elements.
- Unknown fields are rejected by serde.
- Unknown template variables fail before the run starts.

Validation should return a list of errors where practical, not fail on only the first config mistake.

## Loop expansion

Represent the loop as an iterator that yields immutable item contexts.

```rust
struct LoopItem {
    counter: i64,
    item_id: String,
    iteration: u64,        // 1-based
    iteration_index: u64,  // 0-based
}
```

For `end` mode:

- Include `end`.
- With positive `step`, stop when `counter > end`.

For `count` mode:

- Yield exactly `count` items.

Counter math must use checked arithmetic:

- `counter.checked_add(step)`.
- Fail the run before overflow.

Do not pre-allocate the whole loop unless needed for validation. Validate `item_id` rendering by walking the planned counters once without starting OMP. For very large `count`, this can be expensive, but the MVP favors catching invalid templates before side effects. If later needed, add a maximum run length guard explicitly.

## Template rendering

Use Handlebars in strict mode.

Required variables for prompt rendering:

```json
{
  "counter": 14,
  "counter_padded": "014",
  "item_id": "M14",
  "iteration": 1,
  "iteration_index": 0,
  "repo_root": "/path/to/repo",
  "notify_command": "agentflow notify --message \"Need input for M14\"",
  "halt_command": "agentflow halt --message \"M14 is already complete; stopping flow.\""
}
```

`counter_padded`:

- Default to width `3` for MVP because the PRD's example is `014`.
- Keep it as a string.
- Do not add config for padding width in the MVP.

`notify_command` and `halt_command`:

- These render shell text for the agent to copy/call.
- Escape embedded double quotes and backslashes in generated message strings.
- Do not use these rendered strings for AgentFlow-owned subprocess execution.

`notify.command` rendering context:

- Each argv element is a Handlebars template.
- Add `message` to the context.
- Render with strict mode.
- Execute directly as argv; never through a shell.

Use helper functions that accept borrowed inputs where possible, but returning owned `String` for rendered output is fine because rendering necessarily creates new text.

## State model

Persist `.agentflow/state.json` atomically.

```rust
#[derive(Debug, Serialize, Deserialize)]
struct State {
    schema_version: u32,
    run_id: String,
    status: RunStatus,
    repo_root: PathBuf,
    config_path: PathBuf,
    current: CurrentState,
    pending_control: Option<ControlEvent>,
    last_error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum RunStatus {
    Running,
    Paused,
    Halted,
    Failed,
    Completed,
}

#[derive(Debug, Serialize, Deserialize)]
struct CurrentState {
    counter: i64,
    item_id: String,
    iteration: u64,
    prompt_index: usize,
    prompt_id: String,
    session_id: Option<String>,
    session_file: Option<PathBuf>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ControlEvent {
    Notify { message: String, item_id: Option<String>, counter: Option<i64> },
    Halt { message: String, item_id: Option<String>, counter: Option<i64> },
}
```

Atomic write algorithm:

1. Serialize pretty JSON to bytes.
2. Write to `.agentflow/state.json.tmp`.
3. Flush file.
4. Rename temp file to `state.json`.

On Unix, also sync the `.agentflow` directory after rename if practical. If directory sync is awkward cross-platform, document this as best-effort in code comments.

State update rules:

- Before sending any prompt, persist `running` with the current prompt index/id and current session metadata if known.
- After observing `get_state`, persist updated session metadata before sending the prompt.
- When pausing, persist `paused` before closing RPC.
- When halting, persist `halted` before closing RPC.
- On failure, persist `failed` and `last_error` before returning the error, unless the failure itself is state persistence.
- On completion, persist `completed`.

## Local file layout

Create these lazily:

```text
.agentflow/
  state.json
  runs/
    <item_id>/
      rendered-prompts.jsonl
      outputs.jsonl
      sessions.json
```

### `rendered-prompts.jsonl`

Append one JSON object per prompt:

```json
{
  "run_id": "...",
  "item_id": "M14",
  "counter": 14,
  "iteration": 1,
  "prompt_index": 0,
  "prompt_id": "plan",
  "session_id": "...",
  "session_file": "...",
  "text": "..."
}
```

### `outputs.jsonl`

Append in observation order:

```json
{ "kind": "omp_frame", "frame": { "type": "agent_start" } }
{ "kind": "control_event", "event": { "type": "notify", "message": "..." } }
{ "kind": "state_transition", "from": "running", "to": "paused", "reason": "notify" }
{ "kind": "error", "message": "..." }
```

Keep raw OMP frames unmodified. They may contain large fields; do not parse and re-emit only a subset.

### `sessions.json`

Keep this as a JSON array for readability:

```json
[
  {
    "prompt_id": "plan",
    "session_id": "...",
    "session_file": "...",
    "started_at": "2026-05-21T15:49:43Z",
    "reason": "initial"
  }
]
```

Because session count is small, rewriting this file atomically as an array is acceptable.

## OMP RPC implementation

Use `tokio::process::Command`.

Start command:

```text
omp --mode rpc
```

Resume command:

```text
omp --mode rpc --resume <sessionFile>
```

Child setup:

- `stdin` piped.
- `stdout` piped.
- `stderr` piped.
- `current_dir(repo_root)`.
- Set these environment variables for OMP and any agent subprocesses:
  - `AGENTFLOW_CONTROL_SOCKET`
  - `AGENTFLOW_ITEM_ID`
  - `AGENTFLOW_COUNTER`

RPC framing:

- Write one JSON object per line to stdin.
- Read one line at a time from stdout.
- Parse each line as `serde_json::Value` first.
- Preserve raw parsed frames in `outputs.jsonl`.
- Treat invalid JSON from stdout as a run failure.
- Read stderr concurrently and include it in error messages if the child exits unexpectedly.

Command ids:

- Generate monotonic ids per RPC process, e.g. `req-1`, `req-2`.
- Include the command kind in the id where useful, e.g. `prompt-3`.

### RPC types

Use flexible deserialization because OMP frames include varied event shapes.

```rust
enum ExpectedResponse {
    Ready,
    Command { id: String },
    AgentEnd,
}
```

Do not over-model every OMP event. Model only what is needed:

- `type`
- `id`
- `command`
- `success`
- `data.sessionId`
- `data.sessionFile`
- `data.text`

Keep full frames as `serde_json::Value` for recording.

### `start()`

- Spawn OMP.
- Start stdout/stderr reader tasks.
- Wait for first stdout frame.
- Require `{ "type": "ready" }`.
- If anything else appears first, fail.

### `command()`

Input: JSON object without id or with explicit id.

Behavior:

- Assign id if missing.
- Write JSONL.
- Loop over frames until matching response id.
- Record all non-response frames too.
- If response has `success: false`, fail with the whole response included.
- Return the successful response `Value`.

### `prompt_and_wait()`

Input: rendered prompt string.

Behavior:

1. Send `{ "type": "prompt", "message": rendered }` with an id.
2. Read frames until matching prompt response.
3. Require response success.
4. Continue reading frames until the next `agent_end`.
5. If `agent_end` arrives before prompt ack, fail.
6. Do not send another prompt before this returns.

Because observed `agent_end` does not include prompt/session correlation, strict sequential prompting is required.

### `get_state()`

Send:

```json
{ "type": "get_state" }
```

Extract:

- `data.sessionId` as non-empty string.
- `data.sessionFile` as non-empty path string.

Fail if either is missing after a prompt/session creation boundary.

### `new_session()`

Send:

```json
{ "type": "new_session" }
```

Then call `get_state()` and require `sessionId` changed from previous session when there was a previous session.

### `switch_session()`

Implement even if the main pause flow uses fresh `--resume`.

Send:

```json
{ "type": "switch_session", "sessionPath": "..." }
```

Then call `get_state()` and require `sessionFile` equals the requested path.

### `get_last_assistant_text()`

Send:

```json
{ "type": "get_last_assistant_text" }
```

Accept either:

```json
{ "data": { "text": "..." } }
```

or fallback to string-like data if OMP changes shape slightly. Prefer strict extraction for tests.

### Closing OMP RPC

On normal completion/pause/halt:

1. Close child stdin.
2. Wait briefly for exit.
3. If it does not exit, terminate.
4. If it still does not exit, kill.

Do not leave an OMP RPC process running across pause.

## Control socket implementation

Use Unix domain sockets for MVP because the documented socket path is `/tmp/...sock` and the workstation target is Unix-like.

Server:

- Bind `<temp_dir>/agentflow-<run-id>.sock`.
- Remove any stale file at that exact path before binding only if no active run owns it.
- Accept JSON lines or one JSON payload per connection. Recommendation: one JSON payload per connection, then close.
- Validate `type`, `message`, optional `item_id`, optional `counter`.
- Send a simple response:

```json
{ "success": true }
```

or:

```json
{ "success": false, "error": "..." }
```

Client:

- `agentflow notify` and `agentflow halt` connect to `AGENTFLOW_CONTROL_SOCKET`.
- Send one JSON payload.
- Wait for success response.
- Exit non-zero on missing socket, connection failure, invalid response, or rejection.

In-process handling:

- Runner owns a `watch`/`mpsc` channel receiving `ControlEvent`s from the socket task.
- During an OMP turn, collect events while still waiting for `agent_end`.
- After `agent_end`, drain any immediately available control events before deciding whether to advance.

Control precedence:

- `halt` wins over `notify`.
- Duplicate `notify` events collapse to one pending `notify`, preserving the latest message.
- A control event received after `agent_end` but before the next prompt is sent must still apply.

## External notification command

`notify.command` is optional.

Execution rules:

- Render each argv element with strict Handlebars using normal template variables plus `message`.
- Execute directly with `tokio::process::Command`, not through shell.
- Run on:
  - `pause_after`;
  - `notify`;
  - `halt`.
- If the command fails, record the error in `last_error` and `outputs.jsonl`, but do not prevent pause/halt.
- Do not retry.
- Do not block forever; add a short timeout, e.g. 30 seconds.

## Runner state machine

Use explicit transitions. Avoid ad-hoc booleans.

```text
running -> paused    notify or pause_after
running -> halted    halt
running -> failed    config/RPC/state/prompt error
running -> completed all loop items complete
paused  -> running   resume
```

Invalid transitions should fail clearly:

- `open` only allowed from `paused`.
- `resume` only allowed from `paused`.
- `run` refused when state is `running` or `paused`.
- `notify` / `halt` require a live control socket.

### Prompt index semantics

Persist `prompt_index` as the next prompt to run when paused.

Before sending a prompt:

- state contains that prompt's index and id.

After a prompt completes normally:

- if another prompt remains, advance in memory;
- before sending the next prompt, persist the next prompt state.

On pause after prompt completion:

- persist `paused` with `prompt_index` set to the next prompt index.
- If the pause happened after the last prompt in an item, persist the next item and prompt `0` as the resume point.
- If no next item exists, mark `completed` instead of `paused` unless the pause was caused by explicit `notify`; explicit user-requested notify should still pause for review.

On halt:

- keep state pointing at the current item/prompt that caused the halt.

### Session semantics

Each loop item starts with no active session.

For prompt index `0`:

- the first prompt should use the RPC process's current session;
- call `get_state` and record it before sending.

For later prompts:

- reuse current session unless `new_session: true`.
- if `new_session: true`, send `new_session`, then `get_state`, then send prompt.

On resume:

- OMP RPC starts on saved `sessionFile`.
- Continue with the saved next prompt index.
- If the next prompt has `new_session: true`, send `new_session` before that prompt.

## Error handling

Use `thiserror` for internal error enums and `anyhow` at CLI boundaries.

Example:

```rust
#[derive(Debug, thiserror::Error)]
enum AgentFlowError {
    #[error("invalid config: {0}")]
    InvalidConfig(String),
    #[error("OMP RPC failed: {0}")]
    Rpc(String),
    #[error("state error: {0}")]
    State(String),
    #[error("control socket error: {0}")]
    Control(String),
}
```

Guidelines:

- No `unwrap()` / `expect()` in production code.
- Include path context on file errors.
- Include RPC response JSON for failed OMP command responses.
- Include stderr tail when OMP exits unexpectedly.
- Before returning a run failure, attempt to persist `failed` with `last_error`.

## Testing plan

Tests are required. Default automated tests should cover deterministic local logic. Tests that drive OMP itself should be real OMP integration tests gated behind `AGENTFLOW_REAL_OMP_TEST=1`, because the MVP's correctness depends on observed OMP protocol behavior.

### Unit tests

#### Config validation

- rejects unsupported provider;
- rejects both `end` and `count`;
- rejects neither `end` nor `count`;
- rejects `count: 0`;
- rejects `step: 0` and negative step;
- rejects duplicate prompt ids;
- rejects empty prompt text;
- rejects `pause_after: true` without message;
- rejects unknown YAML fields;
- rejects unknown template vars.

#### Loop expansion

- `start=14,end=16,step=1` yields 14, 15, 16;
- `start=14,count=3,step=2` yields 14, 16, 18;
- overflow is reported as error;
- `iteration` is 1-based and `iteration_index` is 0-based.

#### Template rendering

- renders all required variables;
- renders `item_id` before prompt text;
- strict mode fails unknown vars;
- generated notify/halt command escapes quotes;
- `notify.command` argv templates render with `message`.

#### State persistence

- writes readable JSON;
- load after write round-trips;
- atomic write replaces old state;
- missing state is handled cleanly by `status`.

#### Control events

- parses notify payload;
- parses halt payload;
- rejects missing message;
- halt wins over notify;
- duplicate notify preserves latest message.

#### RPC frame handling

- recognizes `ready`;
- matches command responses by id while preserving unrelated frames;
- treats `success: false` as an error;
- fails if `agent_end` appears before prompt ack;
- extracts `sessionId` / `sessionFile` from `get_state`;
- extracts text from `get_last_assistant_text`;
- treats malformed JSON as an error.

### Integration tests with real OMP

Real OMP integration tests should be opt-in because they require a configured OMP environment and may call a model.

Use this env gate:

```text
AGENTFLOW_REAL_OMP_TEST=1
```

When enabled, cover:

- `smoke-test` session reuse and `new_session` behavior;
- `run` sends prompts sequentially and waits for `agent_end`;
- `new_session: true` sends `new_session` before the prompt;
- rendered prompts are recorded;
- raw OMP frames are recorded;
- `pause_after` persists `paused` and closes RPC;
- `resume` starts RPC with `--mode rpc --resume <sessionFile>`;
- `resume "message"` sends the message before continuing;
- prompt ack failure, premature OMP exit, and malformed stdout JSON are handled by lower-level RPC tests where those conditions can be exercised without pretending to be OMP.

### Real OMP smoke test

`agentflow smoke-test` uses real OMP and is manually runnable. It should also be the first opt-in integration test under `AGENTFLOW_REAL_OMP_TEST=1`.

## Implementation sequence

Implement in this order to keep behavior verifiable:

1. Crate scaffolding and CLI parsing.
2. Config structs, loading, and validation.
3. Loop expansion and template rendering.
4. State model and atomic persistence.
5. JSONL file append helpers and `.agentflow` path management.
6. Real OMP smoke-test command skeleton behind an explicit manual/opt-in path.
7. OMP RPC process wrapper: start, ready, command, prompt wait, get_state.
8. Basic `run` for prompts without pause/control/new_session.
9. `new_session` support and session recording.
10. Control socket client/server and `notify` / `halt` commands.
11. Pause-after and notification command execution.
12. Pause process-boundary behavior.
13. `open`.
14. `resume` without message.
15. `resume "message"`.
16. `status`.
17. Failure-state persistence.
18. `smoke-test` against real OMP.
19. Full test pass, clippy, and formatting.

Do not proceed to later orchestration steps until the earlier deterministic units are tested.

## Verification commands

Run before considering implementation complete:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --all-features --locked
```

Manual smoke test when real OMP is available:

```bash
agentflow smoke-test
```

Manual pause/resume scenario with a disposable config:

```bash
agentflow run --start 14 --count 1
agentflow status
agentflow open
agentflow resume
agentflow status
```

## Important edge cases

Handle these explicitly:

- State says `paused` but `session_file` is missing: `open` and `resume` fail clearly.
- State says `running` but no process exists: `status` reports stale running state; MVP should not auto-recover it.
- Control socket is missing: `notify` / `halt` fail non-zero.
- OMP emits extension UI events while waiting for command response: record and keep waiting.
- OMP emits `agent_end` before prompt ack: fail the run.
- OMP prompt response has `success: false`: fail the run.
- `new_session` does not change `sessionId`: fail the run.
- `switch_session` does not report requested `sessionFile`: fail if used.
- Counter overflow: fail before sending next prompt.
- External notification command missing: record error but still pause/halt.
- User runs `resume` while state is `halted`, `failed`, or `completed`: fail with clear status-specific message.

## Rust implementation standards

- Prefer borrowed parameters: `&Path`, `&str`, slices.
- Avoid cloning config/prompt strings in loops unless ownership is required.
- Keep long-lived owned strings in state/records; use borrowed references for rendering inputs.
- Do not panic in production paths.
- Use typed errors below `main`; convert to `anyhow::Result` at command dispatch.
- Keep async boundaries clear: process I/O, socket I/O, file I/O, and notification commands are async; pure config/template/loop logic should stay synchronous.
- Do not introduce traits until there are at least two real implementations or tests require a narrow fake boundary. Prefer simple structs/functions.

## Done definition

The MVP implementation is complete only when:

- all acceptance criteria in `AGENTFLOW.md` are satisfied;
- all commands in the CLI contract exist and behave as specified;
- deterministic unit/integration tests pass;
- real OMP smoke test passes in an environment with OMP configured;
- `cargo fmt`, `cargo clippy`, and `cargo test` pass;
- no MVP non-goals were added as hidden abstractions.
