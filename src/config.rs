use std::collections::HashSet;
use std::path::Path;

use serde::Deserialize;

use crate::error::{AgentFlowError, Result};
use crate::loop_spec::build_loop_plan;
use crate::template::{build_context, render_message, render_notify_argv, render_prompt};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub provider: Provider,
    #[serde(rename = "loop")]
    pub loop_: LoopConfig,
    pub notify: Option<NotifyConfig>,
    #[serde(default)]
    pub logs: LogConfig,
    #[serde(default)]
    pub sessions: SessionConfig,
    pub prompts: Vec<PromptStep>,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Omp,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LoopConfig {
    pub start: i64,
    pub end: Option<i64>,
    pub count: Option<u64>,
    pub step: i64,
    pub item_id: String,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NotifyConfig {
    pub command: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LogConfig {
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Default, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SessionConfig {
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PromptStep {
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub new_session: bool,
    #[serde(default)]
    pub pause_after: bool,
    pub message: Option<String>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LoopOverrides {
    pub start: Option<i64>,
    pub end: Option<i64>,
    pub count: Option<u64>,
}

pub async fn load_config(path: &Path) -> Result<Config> {
    let contents = tokio::fs::read_to_string(path).await?;
    Ok(serde_yaml::from_str(&contents)?)
}

pub fn apply_overrides(config: &mut Config, overrides: &LoopOverrides) {
    if let Some(start) = overrides.start {
        config.loop_.start = start;
    }
    if let Some(end) = overrides.end {
        config.loop_.end = Some(end);
        config.loop_.count = None;
    }
    if let Some(count) = overrides.count {
        config.loop_.count = Some(count);
        config.loop_.end = None;
    }
}

pub fn validate_config(config: &Config, repo_root: &Path) -> Result<()> {
    let mut errors = Vec::new();

    match config.provider {
        Provider::Omp => {}
    }
    validate_loop_config(&config.loop_, &mut errors);
    validate_prompts(&config.prompts, &mut errors);
    validate_notify(config.notify.as_ref(), &mut errors);
    validate_session_config(config, &mut errors);
    if errors.is_empty() {
        validate_templates(config, repo_root, &mut errors);
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(AgentFlowError::config_validation(errors))
    }
}

fn validate_loop_config(loop_: &LoopConfig, errors: &mut Vec<String>) {
    match (loop_.end, loop_.count) {
        (Some(_), Some(_)) => {
            errors.push("exactly one of loop.end or loop.count must be set".to_owned())
        }
        (None, None) => errors.push("exactly one of loop.end or loop.count must be set".to_owned()),
        _ => {}
    }

    if matches!(loop_.count, Some(0)) {
        errors.push("loop.count must be greater than 0".to_owned());
    }
    if loop_.step <= 0 {
        errors.push("loop.step must be greater than 0".to_owned());
    }
    if let Some(end) = loop_.end {
        if loop_.step > 0 && end < loop_.start {
            errors.push("loop.end must be greater than or equal to loop.start".to_owned());
        }
    }
    if loop_.item_id.trim().is_empty() {
        errors.push("loop.item_id must not be empty".to_owned());
    }
}

fn validate_prompts(prompts: &[PromptStep], errors: &mut Vec<String>) {
    if prompts.is_empty() {
        errors.push("prompts must not be empty".to_owned());
    }

    let mut ids = HashSet::new();
    for (index, prompt) in prompts.iter().enumerate() {
        if prompt.id.trim().is_empty() {
            errors.push(format!("prompts[{index}].id must not be empty"));
        } else if !ids.insert(prompt.id.as_str()) {
            errors.push(format!("duplicate prompt id '{}'", prompt.id));
        }

        if prompt.text.trim().is_empty() {
            errors.push(format!("prompts[{index}].text must not be empty"));
        }
        if prompt.pause_after {
            match prompt.message.as_deref() {
                Some(message) if !message.trim().is_empty() => {}
                _ => errors.push(format!(
                    "prompts[{index}].message must be non-empty when pause_after is true"
                )),
            }
        }
    }
}

fn validate_notify(notify: Option<&NotifyConfig>, errors: &mut Vec<String>) {
    let Some(command) = notify.and_then(|notify| notify.command.as_ref()) else {
        return;
    };

    if command.is_empty() {
        errors.push("notify.command must not be empty when set".to_owned());
    }
    for (index, arg) in command.iter().enumerate() {
        if arg.is_empty() {
            errors.push(format!("notify.command[{index}] must not be empty"));
        }
    }
}
fn validate_session_config(config: &Config, errors: &mut Vec<String>) {
    if config.sessions.enabled {
        return;
    }
    if let Some((index, _)) = config
        .prompts
        .iter()
        .enumerate()
        .find(|(_, prompt)| prompt.pause_after)
    {
        errors.push(format!(
            "sessions.enabled must be true when prompts[{index}].pause_after is true"
        ));
    }
}

fn validate_templates(config: &Config, repo_root: &Path, errors: &mut Vec<String>) {
    let plan = match build_loop_plan(config, repo_root) {
        Ok(plan) => plan,
        Err(err) => {
            errors.push(err.to_string());
            return;
        }
    };

    for item in &plan.items {
        let context = build_context(item, repo_root, None);
        for prompt in &config.prompts {
            if let Err(err) = render_prompt(prompt, &context) {
                errors.push(format!(
                    "prompt '{}' failed to render for item '{}': {err}",
                    prompt.id, item.item_id
                ));
            }
            if let Some(message) = prompt.message.as_deref() {
                if let Err(err) = render_message(message, &context) {
                    errors.push(format!(
                        "prompt '{}' message failed to render for item '{}': {err}",
                        prompt.id, item.item_id
                    ));
                }
            }
        }

        if let Some(notify) = config.notify.as_ref() {
            if let Err(err) = render_notify_argv(notify, &context, "validation message") {
                errors.push(format!(
                    "notify.command failed to render for item '{}': {err}",
                    item.item_id
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn valid_config() -> Config {
        Config {
            provider: Provider::Omp,
            loop_: LoopConfig {
                start: 1,
                end: None,
                count: Some(2),
                step: 1,
                item_id: "M{{counter}}".to_owned(),
            },
            notify: Some(NotifyConfig {
                command: Some(vec![
                    "printf".to_owned(),
                    "{{message}} {{item_id}}".to_owned(),
                ]),
            }),
            sessions: SessionConfig::default(),
            logs: LogConfig::default(),
            prompts: vec![PromptStep {
                id: "plan".to_owned(),
                text: "Do {{item_id}} from {{repo_root}}".to_owned(),
                new_session: false,
                pause_after: true,
                message: Some("Pause {{item_id}}".to_owned()),
            }],
        }
    }

    #[test]
    fn load_config_should_disable_logs_and_sessions_by_default() {
        let config: Config = serde_yaml::from_str(
            r#"
provider: omp
loop:
  start: 1
  count: 1
  step: 1
  item_id: "M{{counter}}"
prompts:
  - id: plan
    text: plan
"#,
        )
        .expect("config should parse");

        assert!(!config.logs.enabled);
        assert!(!config.sessions.enabled);
    }

    #[test]
    fn load_config_should_accept_enabled_verbose_logs() {
        let config: Config = serde_yaml::from_str(
            r#"
provider: omp
loop:
  start: 1
  count: 1
  step: 1
  item_id: "M{{counter}}"
logs:
  enabled: true
prompts:
  - id: plan
    text: plan
"#,
        )
        .expect("config should parse");

        assert!(config.logs.enabled);
    }

    #[test]
    fn load_config_should_accept_enabled_sessions() {
        let config: Config = serde_yaml::from_str(
            r#"
provider: omp
loop:
  start: 1
  count: 1
  step: 1
  item_id: "M{{counter}}"
sessions:
  enabled: true
prompts:
  - id: plan
    text: plan
"#,
        )
        .expect("config should parse");

        assert!(config.sessions.enabled);
    }

    #[test]
    fn validate_config_should_accept_valid_config() {
        let mut config = valid_config();
        config.sessions.enabled = true;
        let result = validate_config(&config, Path::new("/repo"));
        assert!(result.is_ok());
    }

    #[test]
    fn validate_config_should_collect_basic_errors() {
        let mut config = valid_config();
        config.loop_.end = Some(2);
        config.loop_.count = Some(0);
        config.loop_.step = 0;
        config.prompts.push(PromptStep {
            id: "plan".to_owned(),
            text: " ".to_owned(),
            new_session: false,
            pause_after: false,
            message: None,
        });
        let err = validate_config(&config, Path::new("/repo"))
            .err()
            .map(|err| err.to_string());
        let err = err.unwrap_or_default();
        assert!(err.contains("exactly one of loop.end or loop.count"));
        assert!(err.contains("loop.count must be greater than 0"));
        assert!(err.contains("loop.step must be greater than 0"));
        assert!(err.contains("duplicate prompt id"));
        assert!(err.contains("text must not be empty"));
        assert!(err.contains("sessions.enabled must be true"));
    }

    #[test]
    fn validate_config_should_require_sessions_for_pause_after() {
        let config = valid_config();
        let err = validate_config(&config, Path::new("/repo"))
            .err()
            .map(|err| err.to_string())
            .unwrap_or_default();

        assert!(err.contains("sessions.enabled must be true"));
    }

    #[test]
    fn validate_config_should_fail_unknown_template_variables() {
        let mut config = valid_config();
        config.sessions.enabled = true;
        config.prompts[0].text = "{{missing}}".to_owned();
        let result = validate_config(&config, Path::new("/repo"));
        assert!(result.is_err());
    }

    #[test]
    fn apply_overrides_should_select_one_loop_bound() {
        let mut config = valid_config();
        apply_overrides(
            &mut config,
            &LoopOverrides {
                start: Some(10),
                end: Some(12),
                count: None,
            },
        );
        assert_eq!(
            (config.loop_.start, config.loop_.end, config.loop_.count),
            (10, Some(12), None)
        );
    }
}
