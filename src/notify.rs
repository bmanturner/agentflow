use anyhow::{anyhow, Context, Result};
use handlebars::Handlebars;
use serde_json::Value;
use tokio::process::Command;
use tokio::time::{timeout, Duration};

const NOTIFY_TIMEOUT: Duration = Duration::from_secs(30);

pub async fn run_notify_command(
    config: Option<&crate::config::NotifyConfig>,
    ctx: &crate::template::TemplateContext,
    message: &str,
) -> Result<Option<String>> {
    let Some(command) = config.and_then(|config| config.command.as_deref()) else {
        return Ok(None);
    };

    if command.is_empty() {
        return Ok(Some(String::from("notify.command is empty")));
    }

    let argv = render_argv(command, ctx, message)?;
    let Some((program, args)) = argv.split_first() else {
        return Ok(Some(String::from("notify.command rendered to empty argv")));
    };

    let mut child = Command::new(program);
    child.args(args).kill_on_drop(true);

    let output = match timeout(NOTIFY_TIMEOUT, child.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => return Ok(Some(format!("notify.command failed to start: {error}"))),
        Err(_) => {
            return Ok(Some(format!(
                "notify.command timed out after {} seconds",
                NOTIFY_TIMEOUT.as_secs()
            )))
        }
    };

    if output.status.success() {
        return Ok(None);
    }

    Ok(Some(format_command_failure(&output)))
}

fn render_argv(
    command: &[String],
    ctx: &crate::template::TemplateContext,
    message: &str,
) -> Result<Vec<String>> {
    let mut data = serde_json::to_value(ctx).context("failed to serialize template context")?;
    let object = data
        .as_object_mut()
        .ok_or_else(|| anyhow!("template context must serialize to a JSON object"))?;
    object.insert(String::from("message"), Value::String(message.to_owned()));

    let mut handlebars = Handlebars::new();
    handlebars.set_strict_mode(true);
    handlebars.register_escape_fn(handlebars::no_escape);

    command
        .iter()
        .map(|arg| {
            handlebars
                .render_template(arg, &data)
                .with_context(|| format!("failed to render notify.command argv element {arg:?}"))
        })
        .collect()
}

fn format_command_failure(output: &std::process::Output) -> String {
    let mut message = format!("notify.command exited with status {}", output.status);
    append_output_text(&mut message, "stdout", &output.stdout);
    append_output_text(&mut message, "stderr", &output.stderr);
    message
}

fn append_output_text(message: &mut String, label: &str, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }

    message.push_str("; ");
    message.push_str(label);
    message.push_str(": ");
    message.push_str(&String::from_utf8_lossy(bytes));
}
