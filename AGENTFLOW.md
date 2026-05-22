# AgentFlow MVP PRD

## Goal

AgentFlow is a tiny local loop runner for OMP.

It should:

1. Read one `.agentflow.yml` file.
2. Render inline prompt templates.
3. Send prompts to OMP in the same session until a step says `new_session: true`.
4. Increment a simple counter between loop items.
5. Let the agent pause the flow and notify the user.
6. Let the agent halt the flow when continuing would be wrong or unnecessary.

No workflow engine. No database. No formal milestone model. No implementation-plan model. No structured question model. No separate prompt files. No Codex support in the MVP.

## Implementation Language

The production MVP should be implemented in Rust.

Rationale:

- AgentFlow is a local CLI that should install as a single reliable binary.
- Rust has strong process-management and async I/O support for driving `omp --mode rpc` over JSONL stdin/stdout.
- Strong typing is useful for config, run state, prompt steps, counters, and OMP RPC events.
- Rust avoids Python/Node runtime dependency drift for a tool that should work across many repositories.
- The MVP is small enough that Rust is not overkill.

Recommended crates:

```toml
clap = \"4\"          # CLI parsing
serde = \"1\"        # config/state structs
serde_yaml = \"0.9\" # .agentflow.yml
serde_json = \"1\"   # OMP RPC JSONL
tokio = \"1\"        # async subprocess/stdin/stdout
anyhow = \"1\"       # app-level error handling
thiserror = \"2\"    # typed internal errors
handlebars = \"6\"   # simple {{var}} templating
```

Optional later:

```toml
notify-rust = \"4\"  # desktop notifications
```

Python is acceptable for throwaway proof-of-concept scripts only. TypeScript/Node is not recommended for the production MVP because packaging/runtime drift is worse than shipping one Rust binary.

## Config

Everything important lives in `.agentflow.yml`.

```yaml
provider: omp

loop:
  start: 14
  end: 35          # inclusive; use either end or count
  count: null
  step: 1
  item_id: "M{{counter}}"

notify:
  command: null    # optional argv list, e.g. ["terminal-notifier", "-message", "{{message}}"]

prompts:
  - id: plan
    text: |
      Read all documentation in docs/sons_of_cain/*.md and create docs/sons_of_cain/implementation/{{item_id}}_IMPLEMENTATION_PLAN.md detailing how to implement {{item_id}}.

      Do research into the game engine and Bevy documentation if necessary to provide accurate implementation details following all best practices for Rust, game engine architecture, and Bevy.

      Surface any open questions for us to address together. Provide your recommendation with each open question.

      If you need my input before safely continuing, call:
      {{notify_command}}

      If {{item_id}} is already complete, say so and finish this step without calling halt.

  - id: review-plan
    text: |
      Review the work so far, ensuring we're not re-implementing anything offered by the game engine, that we are utilizing the game engine the way it is intended to be done, and that we're designing systems to be reusable beyond this single encounter.

      If you need my input before safely continuing, call:
      {{notify_command}}

      If there is nothing left to do for this step, say so and finish without calling halt.

  - id: summarize-open-questions
    pause_after: true
    message: "Review open questions for {{item_id}}."
    text: |
      Briefly describe the open questions and your recommendation.

  - id: implement
    new_session: true
    text: |
      /goal Implement docs/sons_of_cain/implementation/{{item_id}}_IMPLEMENTATION_PLAN.md. Atomic commits. Don't cut corners. Don't defer any work. Review and follow-up to address any surfaced gaps, issues, or accidentally deferred work.

      If you need my input or hit a blocker, call:
      {{notify_command}}

      If this is already implemented and verified, say so and finish this step without calling halt.

  - id: review-and-fix
    text: |
      Use a sub-agent to review your implementation. Follow-up to address any surfaced gaps, issues, or accidentally deferred work.

      If you need my input or hit a blocker, call:
      {{notify_command}}

      If there is nothing left to review or fix, say so and finish this step without calling halt.
```

### Config validation

Recommendation:

- `provider` must be `omp` for the MVP.
- `loop.start` is required.
- Exactly one of `loop.end` or `loop.count` must be set.
- `loop.end` is inclusive.
- `loop.count` must be greater than `0` when set.
- `loop.step` must be greater than `0`.
- `loop.item_id` must render successfully for every counter value.
- `prompts` must be non-empty.
- Every prompt must have a non-empty unique `id`.
- Every prompt must have non-empty `text`.
- Unknown fields should be rejected instead of ignored.
- Unknown template variables should fail config validation before the run starts.
- `pause_after: true` requires a non-empty `message`.
- `notify.command` should be an argv list, not a shell string, to avoid quoting bugs. Example: `["terminal-notifier", "-message", "{{message}}"]`.

## Template Variables

```text
{{counter}}          # 14
{{counter_padded}}   # optional padded form, e.g. 014
{{item_id}}          # M14
{{iteration}}        # 1-based loop iteration
{{iteration_index}}  # 0-based loop iteration
{{repo_root}}
{{notify_command}}
{{halt_command}}
{{message}}           # notify.command only; rendered to pause/notify/halt message text
```

`{{notify_command}}` renders to shell text inside the prompt:

```bash
agentflow notify --message "Need input for {{item_id}}"
```

`{{halt_command}}` renders to shell text inside the prompt:

```bash
agentflow halt --message "{{item_id}} is already complete; stopping flow."
```

These are instructions for the agent, not commands AgentFlow itself should execute through a shell. AgentFlow-owned commands should use argv arrays internally.

The user is responsible for putting `agentflow` on `PATH`.

## Core Loop

```text
counter = config.loop.start
start one OMP RPC process from repo root

repeat until end/count exhausted:
  item_id = render(config.loop.item_id, counter)
  current_session = null

  for prompt_step in config.prompts:
    if current_session is null or prompt_step.new_session:
      tell OMP to start a new session
      record OMP session id/path

    render prompt_step.text
    send rendered prompt to OMP
    wait for OMP agent_end

    if prompt_step.pause_after:
      notify user with prompt_step.message
      pause before the next configured step

    if agent called `agentflow notify`:
      pause before the next configured step

    if agent called `agentflow halt`:
      stop the whole flow

  counter += config.loop.step
```

Each loop item starts with no session. Within one item, the same session is reused until a prompt step sets `new_session: true`.

## OMP RPC Behavior

AgentFlow uses OMP RPC mode:

```bash
omp --mode rpc
```

Observed behavior from `omp/15.2.1` RPC experiments:

- Commands are JSONL over stdin.
- Events are JSONL over stdout.
- Startup emits `{ "type": "ready" }` before commands should be sent.
- Command responses have `{ "type": "response", "id": "...", "command": "...", "success": true|false, "data": ... }`.
- Unknown commands return a failed `response`; malformed JSONL terminates the RPC process.
- `prompt` is acknowledged immediately with a successful `response`; acceptance is not completion.
- Completion is observed with the later `agent_end` event.
- Observed `agent_end` frames do not include a prompt id or session id. AgentFlow must therefore send prompts strictly sequentially and treat the next `agent_end` after the prompt ack as completion.
- Assistant text is available in streaming `message_update` / `message_end` events and via the RPC command `get_last_assistant_text`.
- `get_state` returns `sessionId` and `sessionFile`.
- `new_session` starts a new session inside the same RPC process.
- `switch_session` with `sessionPath` switches an RPC process back to a saved session file.
- `omp --mode rpc --resume <sessionFile>` starts RPC directly on a saved session.
- A second OMP process can append to a saved session file, but an already-running RPC process does not automatically observe those appended messages until it reloads the session, e.g. with `switch_session`.

Therefore the MVP should send prompts sequentially, wait for the prompt ack before accepting events for that prompt, wait for `agent_end` before sending the next prompt, and restart RPC with `--resume <sessionFile>` or explicitly `switch_session` after any interactive `agentflow open`.

### Start / inspect session

```json
{ "id": "state-1", "type": "get_state" }
```

Record:

```json
{
  "sessionId": "...",
  "sessionFile": "..."
}
```

### Send prompt

```json
{ "id": "prompt-1", "type": "prompt", "message": "<rendered prompt>" }
```

Wait for `agent_end`.

To inspect assistant text for smoke tests or diagnostics, send:

```json
{ "id": "last-text-1", "type": "get_last_assistant_text" }
```

Observed successful response:

```json
{
  "type": "response",
  "id": "last-text-1",
  "command": "get_last_assistant_text",
  "success": true,
  "data": { "text": "..." }
}
```

### New session

For a step with:

```yaml
new_session: true
```

send:

```json
{ "id": "new-session-1", "type": "new_session" }
```

Then call `get_state`, record the new `sessionId` / `sessionFile`, and send the prompt.

To return to a saved session inside an existing RPC process, send:

```json
{ "id": "switch-session-1", "type": "switch_session", "sessionPath": "<sessionFile>" }
```

## Pause and Halt Control

No `check-in.json`. No `halt.json`.

When AgentFlow starts OMP, it sets environment variables for the agent subprocess:

```text
AGENTFLOW_CONTROL_SOCKET=/tmp/agentflow-<run-id>.sock
AGENTFLOW_ITEM_ID=M14
AGENTFLOW_COUNTER=14
```

`agentflow notify` and `agentflow halt` are tiny CLI commands that connect to `AGENTFLOW_CONTROL_SOCKET`, send a control event to the running orchestrator, print the message, and exit `0`.

Control socket protocol recommendation:

```json
{ "type": "notify", "message": "...", "item_id": "M14", "counter": 14 }
```

```json
{ "type": "halt", "message": "...", "item_id": "M14", "counter": 14 }
```

- The socket path should include the run id and live under the platform temp directory.
- The orchestrator should remove the socket on clean exit.
- `agentflow notify` / `agentflow halt` should fail non-zero if `AGENTFLOW_CONTROL_SOCKET` is unset, the socket is missing, or the orchestrator rejects the event.
- `halt` wins over `notify` if both arrive during the same OMP turn.
- Duplicate `notify` events during one turn should collapse into one paused state, preserving the latest message.
- A control event received after `agent_end` but before the next prompt is sent must still apply before advancing.

### Notify

Agent command:

```bash
agentflow notify --message "Please review the open questions for M14."
```

Effect:

- send `notify` event to the active AgentFlow process
- run configured `notify.command`, if present
- pause after the current OMP turn reaches `agent_end`
- do not advance to the next prompt until the user resumes

### Halt

Agent command:

```bash
agentflow halt --message "M14 is already complete. No further work needed."
```

Effect:

- send `halt` event to the active AgentFlow process
- run configured `notify.command`, if present
- stop the whole flow after the current OMP turn reaches `agent_end`
- do not advance to the next prompt or counter value

### Orchestrator-owned pause

For a step with:

```yaml
pause_after: true
message: "Review open questions for {{item_id}}."
```

AgentFlow pauses after that step reaches `agent_end`, even if the agent did not call `agentflow notify`.

## User Input While Paused

When paused, the user should be able to open the exact OMP session that needs input:

```bash
agentflow open
```

AgentFlow runs OMP interactively from the repo root, resuming the saved active session file:

```bash
omp --resume <sessionFile>
```

The user works directly in OMP, answers questions, asks the agent to revise files, and exits OMP with `/exit` when done.

Then the user continues the flow:

```bash
agentflow resume
```

`agentflow resume` does not send a user message. It starts a fresh OMP RPC process with `omp --mode rpc --resume <sessionFile>` from the repo root and continues with the next configured prompt.

Recommendation: treat pause as a process boundary. On pause, persist state, close the RPC process, and leave the flow in `paused`. This avoids two long-lived OMP processes holding stale views of the same session file while the user works interactively.

If a one-shot non-interactive response is useful, AgentFlow may also support:

```bash
agentflow resume "Use these answers to revise the document, remove resolved questions, and continue: ..."
```

In that case AgentFlow sends the message to the active OMP session, waits for `agent_end`, and then continues to the next configured prompt if no new notify/halt occurs.

## Local Files

Keep local files minimal:

```text
.agentflow/
  state.json
  runs/
    M14/
      rendered-prompts.jsonl
      outputs.jsonl
      sessions.json
```

`state.json` is for crash visibility/recovery only. It is not a workflow database.

When paused, `state.json` must include the active `sessionFile` so `agentflow open` can resume the exact OMP session that needs input.

Output recording recommendation:

- `rendered-prompts.jsonl`: one record per prompt with `run_id`, `item_id`, `counter`, `iteration`, `prompt_index`, `prompt_id`, `session_id`, `session_file`, and rendered `text`.
- `outputs.jsonl`: append raw OMP frames, control events, state transitions, and command errors in observation order.
- `sessions.json`: record every observed session with `prompt_id`, `session_id`, `session_file`, `started_at`, and why it was created (`initial`, `new_session`, `resume`).

Recommended `state.json` shape:

```json
{
  "schema_version": 1,
  "run_id": "2026-05-21T15-49-43Z-M14",
  "status": "running|paused|halted|failed|completed",
  "repo_root": "/path/to/repo",
  "config_path": "/path/to/repo/.agentflow.yml",
  "current": {
    "counter": 14,
    "item_id": "M14",
    "iteration": 1,
    "prompt_index": 2,
    "prompt_id": "summarize-open-questions",
    "session_id": "...",
    "session_file": "..."
  },
  "pending_control": {
    "type": "notify|halt",
    "message": "..."
  },
  "last_error": null
}
```

State machine recommendation:

- `running`: an OMP turn may be active, or the orchestrator is between prompts.
- `paused`: no OMP RPC process is owned by AgentFlow; `agentflow open` and `agentflow resume` are allowed.
- `halted`: the agent requested a deliberate stop; no further prompts or counter increments are allowed.
- `failed`: an infrastructure/config/RPC error stopped the flow; `agentflow resume` may retry from the recorded prompt when a session file was saved in state or item session records.
- `completed`: every configured counter item and prompt completed.

Failure behavior recommendation:

- Invalid config fails before starting OMP.
- Failed prompt ack fails the run, except a busy-agent rejection is retried until the prompt idle timeout expires.
- OMP process exit before expected `agent_end` fails the run.
- Malformed JSON from OMP fails the run.
- Timeout waiting for `agent_end` fails the run unless the user explicitly resumes/retries in a later command.
- A failed RPC response with `Agent is already processing` is recoverable: preserve intervening frames, back off, and retry the same command with a fresh id for internally generated commands.
- Failed state write fails the run before sending the next prompt.
- Failed external `notify.command` should be recorded in `last_error` but should not prevent pausing or halting.

## Smoke Test

Before real work, AgentFlow must prove session reuse and `new_session` behavior:

1. Start OMP RPC and wait for the `ready` frame.
2. Call `get_state` and record `sessionId` / `sessionFile`.
3. Send: `Remember AGENTFLOW_SMOKE_TEST.`
4. Wait for prompt ack, then wait for `agent_end`.
5. Send in the same session: `What token did I ask you to remember?`
6. Wait for `agent_end`, then call `get_last_assistant_text`.
7. Pass only if the response contains `AGENTFLOW_SMOKE_TEST`.
8. Send `new_session`.
9. Call `get_state` and pass only if `sessionId` changed.
10. Ask again: `What token did I ask you to remember?`
11. Wait for `agent_end`, then call `get_last_assistant_text`.
12. Pass only if the response does not rely on the previous session.

## CLI

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
```

## Acceptance Criteria

MVP is done when:

- `.agentflow.yml` contains the full loop config, including inline prompts.
- Invalid config fails before OMP starts.
- The production implementation is a Rust CLI using `clap`, `serde`, `serde_yaml`, `serde_json`, `tokio`, `anyhow`, `thiserror`, and `handlebars`.
- `agentflow run --start 14 --end 35` increments `{{counter}}` and renders `{{item_id}}` for each item.
- AgentFlow runs one OMP RPC process for the active run, and closes it at pause boundaries.
- AgentFlow waits for the RPC `ready` frame before sending commands.
- AgentFlow sends prompts sequentially, waits for the prompt ack, then waits for `agent_end` after each prompt.
- AgentFlow records OMP `sessionId` and `sessionFile` from `get_state`.
- A prompt step with `new_session: true` starts a new OMP session before sending that prompt.
- `pause_after: true` pauses after the step completes.
- `agentflow notify --message ...` pauses the flow through the control socket.
- `agentflow halt --message ...` halts the flow through the control socket.
- `agentflow open` opens the saved active OMP session interactively with `omp --resume <sessionFile>`.
- After the user exits OMP, `agentflow resume` continues with the next configured prompt.
- `agentflow resume` starts fresh RPC with `omp --mode rpc --resume <sessionFile>` before continuing.
- The loop never continues past a paused, halted, or failed item.
