use std::path::Path;

use handlebars::Handlebars;
use serde::Serialize;

use crate::config::{NotifyConfig, PromptStep};
use crate::error::Result;
use crate::loop_spec::LoopItem;

#[derive(Debug, Clone, Serialize)]
pub struct TemplateContext {
    pub counter: i64,
    pub counter_padded: String,
    pub item_id: String,
    pub iteration: u64,
    pub iteration_index: u64,
    pub repo_root: String,
    pub notify_command: String,
    pub halt_command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Serialize)]
struct ItemIdContext<'a> {
    counter: i64,
    counter_padded: String,
    iteration: u64,
    iteration_index: u64,
    repo_root: &'a str,
}

#[derive(Debug, Serialize)]
struct NotifyTemplateContext<'a> {
    counter: i64,
    counter_padded: &'a str,
    item_id: &'a str,
    iteration: u64,
    iteration_index: u64,
    repo_root: &'a str,
    notify_command: &'a str,
    halt_command: &'a str,
    message: &'a str,
}

fn registry() -> Handlebars<'static> {
    let mut handlebars = Handlebars::new();
    handlebars.set_strict_mode(true);
    handlebars.register_escape_fn(handlebars::no_escape);
    handlebars
}

fn render_strict<T>(template: &str, context: &T) -> Result<String>
where
    T: Serialize,
{
    Ok(registry().render_template(template, context)?)
}

fn shell_double_quoted(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for ch in value.chars() {
        match ch {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            _ => quoted.push(ch),
        }
    }
    quoted.push('"');
    quoted
}

fn default_notify_message(item_id: &str) -> String {
    let mut message = String::with_capacity("Need input for ".len() + item_id.len());
    message.push_str("Need input for ");
    message.push_str(item_id);
    message
}

fn default_halt_message(item_id: &str) -> String {
    let suffix = " is already complete; stopping flow.";
    let mut message = String::with_capacity(item_id.len() + suffix.len());
    message.push_str(item_id);
    message.push_str(suffix);
    message
}

pub fn render_item_id(
    template: &str,
    counter: i64,
    iteration: u64,
    iteration_index: u64,
    repo_root: &Path,
) -> Result<String> {
    let repo_root = repo_root.to_string_lossy();
    let context = ItemIdContext {
        counter,
        counter_padded: format!("{counter:03}"),
        iteration,
        iteration_index,
        repo_root: repo_root.as_ref(),
    };
    render_strict(template, &context)
}

pub fn build_context(item: &LoopItem, repo_root: &Path, message: Option<&str>) -> TemplateContext {
    let notify_message = default_notify_message(&item.item_id);
    let halt_message = default_halt_message(&item.item_id);
    let notify_quoted = shell_double_quoted(&notify_message);
    let halt_quoted = shell_double_quoted(&halt_message);

    TemplateContext {
        counter: item.counter,
        counter_padded: format!("{:03}", item.counter),
        item_id: item.item_id.clone(),
        iteration: item.iteration,
        iteration_index: item.iteration_index,
        repo_root: repo_root.to_string_lossy().into_owned(),
        notify_command: format!("agentflow notify --message {notify_quoted}"),
        halt_command: format!("agentflow halt --message {halt_quoted}"),
        message: message.map(str::to_owned),
    }
}

pub fn render_prompt(step: &PromptStep, context: &TemplateContext) -> Result<String> {
    render_strict(&step.text, context)
}

pub fn render_message(template: &str, context: &TemplateContext) -> Result<String> {
    render_strict(template, context)
}

pub fn render_notify_argv(
    notify: &NotifyConfig,
    context: &TemplateContext,
    message: &str,
) -> Result<Option<Vec<String>>> {
    let Some(command) = notify.command.as_ref() else {
        return Ok(None);
    };

    let context = NotifyTemplateContext {
        counter: context.counter,
        counter_padded: &context.counter_padded,
        item_id: &context.item_id,
        iteration: context.iteration,
        iteration_index: context.iteration_index,
        repo_root: &context.repo_root,
        notify_command: &context.notify_command,
        halt_command: &context.halt_command,
        message,
    };

    let mut argv = Vec::with_capacity(command.len());
    for arg in command {
        argv.push(render_strict(arg, &context)?);
    }
    Ok(Some(argv))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::loop_spec::LoopItem;

    use super::*;

    fn item() -> LoopItem {
        LoopItem {
            counter: 14,
            item_id: "M14".to_owned(),
            iteration: 1,
            iteration_index: 0,
        }
    }

    #[test]
    fn render_item_id_should_fail_for_unknown_variable() {
        let err = render_item_id("M{{missing}}", 14, 1, 0, Path::new("/repo"));
        assert!(err.is_err());
    }

    #[test]
    fn build_context_should_render_shell_commands_with_escaped_message() {
        let item = LoopItem {
            counter: 7,
            item_id: "M\\\"7".to_owned(),
            iteration: 1,
            iteration_index: 0,
        };
        let context = build_context(&item, Path::new("/repo"), None);
        assert_eq!(
            context.notify_command,
            "agentflow notify --message \"Need input for M\\\\\\\"7\""
        );
    }

    #[test]
    fn render_prompt_should_include_counter_padded_and_item_id() {
        let context = build_context(&item(), Path::new("/repo"), None);
        let step = PromptStep {
            id: "p".to_owned(),
            text: "{{item_id}} {{counter_padded}}".to_owned(),
            new_session: false,
            pause_after: false,
            message: None,
        };
        let rendered = match render_prompt(&step, &context) {
            Ok(rendered) => rendered,
            Err(err) => panic!("{err}"),
        };
        assert_eq!(rendered, "M14 014");
    }

    #[test]
    fn render_notify_argv_should_render_message_without_shell() {
        let context = build_context(&item(), Path::new("/repo"), None);
        let notify = NotifyConfig {
            command: Some(vec![
                "notify".to_owned(),
                "{{message}} {{item_id}}".to_owned(),
            ]),
        };
        let rendered = match render_notify_argv(&notify, &context, "hello \"there\"") {
            Ok(Some(argv)) => argv,
            Ok(None) => panic!("expected argv"),
            Err(err) => panic!("{err}"),
        };
        assert_eq!(
            rendered,
            vec!["notify".to_owned(), "hello \"there\" M14".to_owned()]
        );
    }
}
