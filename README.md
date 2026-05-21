# AgentFlow

AgentFlow is a tiny local loop runner for [omp](https://omp.sh) sessions. It reads one `.agentflow.yml`, renders inline prompt templates, sends prompts to `omp --mode rpc` in order, and lets the agent pause or halt the flow from inside the omp turn.

It is intentionally small:

- one YAML config file;
- inline prompts only;
- one counter-driven loop;
- one OMP RPC process for the active run;
- no database, workflow engine, DAG, milestone model, or Codex provider.

## Install

### From this repository

```bash
git clone <this-repo-url>
cd agentflow
cargo install --path . --locked
```

Make sure Cargo's bin directory is on your `PATH`:

```bash
export PATH="$HOME/.cargo/bin:$PATH"
```

Verify:

```bash
agentflow --version
agentflow smoke-test
```

`agentflow smoke-test` uses real OMP. It verifies that AgentFlow can reuse an OMP session, start a new session, and inspect the last assistant response.

## Requirements

- Rust toolchain with Cargo.
- `omp` installed and available on `PATH`.
- OMP configured for normal interactive use.
- A project-local `.agentflow.yml`.


## Quick start

Create `.agentflow.yml` in the repository you want OMP to work in:

```yaml
provider: omp

loop:
  start: 14
  end: 35          # inclusive; use either end or count
  count: null
  step: 1
  item_id: "M{{counter}}"

notify:
  command:
    - curl
    - -fsS
    - -X
    - POST
    - https://example.com/agentflow-notify
    - --data-urlencode
    - "message={{message}}"
    - --data-urlencode
    - "item_id={{item_id}}"

prompts:
  - id: plan
    text: |
      Create an implementation plan for {{item_id}}.

      If you need my input before safely continuing, call:
      {{notify_command}}

      If {{item_id}} is already complete, say so and finish this step without calling halt.

  - id: review-plan
    text: |
      Review the plan for correctness and missing work.

  - id: summarize-open-questions
    pause_after: true
    message: "Review open questions for {{item_id}}."
    text: |
      Briefly describe the open questions and your recommendation.

  - id: implement
    new_session: true
    text: |
      /goal Implement the plan for {{item_id}}. Don't cut corners. Don't defer work.

  - id: review-and-fix
    text: |
      Use a sub-agent to review the implementation. Fix any surfaced gaps.
```

Run the flow:

```bash
agentflow run
```

Run a smaller slice:

```bash
agentflow run --start 14 --count 1
agentflow run --start 14 --end 16
```

## The pause/resume loop

AgentFlow pauses when either:

- a prompt step has `pause_after: true`; or
- the agent calls `agentflow notify --message ...` from inside OMP.

When paused:

```bash
agentflow status
agentflow open
```

`agentflow open` resumes the exact OMP session interactively with `omp --resume <sessionFile>`. Answer questions, ask the agent to revise files, then exit OMP with `/exit`.

Continue the configured flow:

```bash
agentflow resume
```

Optionally send one more message before continuing:

```bash
agentflow resume "Use these answers to revise the plan, remove resolved questions, and continue: ..."
```

## Halt behavior

The agent can stop the whole flow when continuing would be wrong or unnecessary:

```bash
agentflow halt --message "M14 is already complete. No further work needed."
```

AgentFlow waits for the current OMP turn to reach `agent_end`, records the halt, and does not advance to the next prompt or counter value.

## HTTP notification example

AgentFlow runs `notify.command` directly as argv. It is not a shell string.

This example posts a simple form-encoded notification with `curl`:

```yaml
notify:
  command:
    - curl
    - -fsS
    - -X
    - POST
    - https://example.com/agentflow-notify
    - --data-urlencode
    - "message={{message}}"
    - --data-urlencode
    - "item_id={{item_id}}"
```

Replace `https://example.com/agentflow-notify` with your own webhook endpoint.

## Template variables

Prompt text, `loop.item_id`, prompt pause messages, and `notify.command` argv elements are rendered with strict Handlebars templates.

Available variables:

| Variable | Example | Notes |
| --- | --- | --- |
| `{{counter}}` | `14` | Current loop counter. |
| `{{counter_padded}}` | `014` | Width-3 padded counter. |
| `{{item_id}}` | `M14` | Rendered from `loop.item_id`. |
| `{{iteration}}` | `1` | 1-based loop iteration. |
| `{{iteration_index}}` | `0` | 0-based loop iteration. |
| `{{repo_root}}` | `/path/to/repo` | Repository root for the run. |
| `{{notify_command}}` | `agentflow notify --message ...` | Shell text for the agent to call inside OMP. |
| `{{halt_command}}` | `agentflow halt --message ...` | Shell text for the agent to call inside OMP. |
| `{{message}}` | `Review open questions...` | Only for `notify.command` rendering. |

Unknown variables fail validation before OMP starts.

## CLI reference

```bash
agentflow run
agentflow run --start 14 --count 1
agentflow run --start 14 --end 35
agentflow status
agentflow open
agentflow resume
agentflow resume "..."
agentflow notify --message "..."
agentflow halt --message "..."
agentflow smoke-test
```

Global options:

```bash
agentflow --config path/to/.agentflow.yml --repo-root path/to/repo --state path/to/state.json <command>
```

## Local files

AgentFlow writes local run state under `.agentflow/` in the target repository:

```text
.agentflow/
  state.json
  runs/
    M14/
      rendered-prompts.jsonl
      outputs.jsonl
      sessions.json
```

These files are for visibility and crash recovery. They are not a database.

## Development

Run checks:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --all-features --locked
```

Run the real OMP smoke test:

```bash
cargo run -- smoke-test
```

Install the current checkout:

```bash
cargo install --path . --locked --force
```
