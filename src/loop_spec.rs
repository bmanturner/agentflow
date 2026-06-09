use crate::config::Config;
use crate::error::{AgentFlowError, Result};
use crate::template::render_item_id;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopItem {
    pub counter: i64,
    pub item_id: String,
    pub iteration: u64,
    pub iteration_index: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopPlan {
    pub items: Vec<LoopItem>,
}

pub fn build_loop_plan(config: &Config, repo_root: &std::path::Path) -> Result<LoopPlan> {
    let mut items = Vec::new();
    let mut counter = config.loop_.start;
    let step = config.loop_.step;

    if step <= 0 {
        return Err(AgentFlowError::Loop(
            "loop.step must be greater than 0".to_owned(),
        ));
    }

    match (config.loop_.end, config.loop_.count) {
        (Some(_), Some(_)) | (None, None) => {
            return Err(AgentFlowError::Loop(
                "exactly one of loop.end or loop.count must be set".to_owned(),
            ));
        }
        _ => {}
    }

    if let Some(count) = config.loop_.count {
        if count == 0 {
            return Err(AgentFlowError::Loop(
                "loop.count must be greater than 0".to_owned(),
            ));
        }
        for iteration_index in 0..count {
            items.push(build_item(config, repo_root, counter, iteration_index)?);
            if iteration_index + 1 < count {
                counter = counter.checked_add(step).ok_or_else(|| {
                    AgentFlowError::Loop("counter overflow while expanding count loop".to_owned())
                })?;
            }
        }
        return Ok(LoopPlan { items });
    }

    let end = config.loop_.end.ok_or_else(|| {
        AgentFlowError::Loop("exactly one of loop.end or loop.count must be set".to_owned())
    })?;
    if end < counter {
        return Err(AgentFlowError::Loop(
            "loop.end must be greater than or equal to loop.start".to_owned(),
        ));
    }
    let mut iteration_index = 0_u64;
    while counter <= end {
        items.push(build_item(config, repo_root, counter, iteration_index)?);
        iteration_index = iteration_index.checked_add(1).ok_or_else(|| {
            AgentFlowError::Loop("iteration overflow while expanding end loop".to_owned())
        })?;
        if counter == end {
            break;
        }
        counter = counter.checked_add(step).ok_or_else(|| {
            AgentFlowError::Loop("counter overflow while expanding end loop".to_owned())
        })?;
    }

    Ok(LoopPlan { items })
}

fn build_item(
    config: &Config,
    repo_root: &std::path::Path,
    counter: i64,
    iteration_index: u64,
) -> Result<LoopItem> {
    let iteration = iteration_index.checked_add(1).ok_or_else(|| {
        AgentFlowError::Loop("iteration overflow while expanding loop".to_owned())
    })?;
    let item_id = render_item_id(
        &config.loop_.item_id,
        counter,
        iteration,
        iteration_index,
        repo_root,
    )?;
    if item_id.trim().is_empty() {
        return Err(AgentFlowError::Loop(
            "loop.item_id rendered an empty item id".to_owned(),
        ));
    }
    Ok(LoopItem {
        counter,
        item_id,
        iteration,
        iteration_index,
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::config::{Config, LogConfig, LoopConfig, Provider};

    use super::*;

    fn config(loop_: LoopConfig) -> Config {
        Config {
            provider: Provider::Omp,
            loop_,
            notify: None,
            logs: LogConfig::default(),
            prompts: vec![],
        }
    }

    #[test]
    fn build_loop_plan_should_include_end_counter() {
        let plan = build_loop_plan(
            &config(LoopConfig {
                start: 1,
                end: Some(5),
                count: None,
                step: 2,
                item_id: "M{{counter}}".to_owned(),
            }),
            Path::new("/repo"),
        );
        let items = match plan {
            Ok(plan) => plan.items,
            Err(err) => panic!("{err}"),
        };
        assert_eq!(
            items,
            vec![
                LoopItem {
                    counter: 1,
                    item_id: "M1".to_owned(),
                    iteration: 1,
                    iteration_index: 0,
                },
                LoopItem {
                    counter: 3,
                    item_id: "M3".to_owned(),
                    iteration: 2,
                    iteration_index: 1,
                },
                LoopItem {
                    counter: 5,
                    item_id: "M5".to_owned(),
                    iteration: 3,
                    iteration_index: 2,
                },
            ]
        );
    }

    #[test]
    fn build_loop_plan_should_yield_exact_count() {
        let plan = build_loop_plan(
            &config(LoopConfig {
                start: 4,
                end: None,
                count: Some(2),
                step: 3,
                item_id: "{{counter_padded}}".to_owned(),
            }),
            Path::new("/repo"),
        );
        let item_ids = match plan {
            Ok(plan) => plan
                .items
                .into_iter()
                .map(|item| item.item_id)
                .collect::<Vec<_>>(),
            Err(err) => panic!("{err}"),
        };
        assert_eq!(item_ids, vec!["004".to_owned(), "007".to_owned()]);
    }

    #[test]
    fn build_loop_plan_should_fail_before_counter_overflow() {
        let err = build_loop_plan(
            &config(LoopConfig {
                start: i64::MAX,
                end: None,
                count: Some(2),
                step: 1,
                item_id: "{{counter}}".to_owned(),
            }),
            Path::new("/repo"),
        );
        assert!(err.is_err());
    }
}
