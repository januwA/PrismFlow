use std::collections::BTreeSet;

use crate::{
    application::review_workflow::{ReviewWorkflowOptions, ensure_agent_prompts_available},
    domain::{entities::AppConfig, ports::FileSystem},
};

pub fn panic_if_required_agents_missing(
    fs: &dyn FileSystem,
    config: &AppConfig,
    options: &ReviewWorkflowOptions,
    context: &str,
) {
    let required = required_agents_for_repo_review(config, &options.cli_agents);
    if required.is_empty() {
        return;
    }
    if let Err(err) = ensure_agent_prompts_available(fs, &required, &options.agent_prompt_dirs) {
        panic!(
            "{context}: missing required agent prompts for [{}]: {err:#}",
            required.join(", ")
        );
    }
}

pub fn panic_if_cli_agents_missing(
    fs: &dyn FileSystem,
    options: &ReviewWorkflowOptions,
    context: &str,
) {
    let required = dedup_agents(&options.cli_agents);
    if required.is_empty() {
        return;
    }
    if let Err(err) = ensure_agent_prompts_available(fs, &required, &options.agent_prompt_dirs) {
        panic!(
            "{context}: missing required agent prompts for [{}]: {err:#}",
            required.join(", ")
        );
    }
}

fn required_agents_for_repo_review(config: &AppConfig, cli_agents: &[String]) -> Vec<String> {
    if !cli_agents.is_empty() {
        return dedup_agents(cli_agents);
    }

    let mut all = Vec::new();
    for repo in &config.repos {
        all.extend(repo.agents.clone());
    }
    dedup_agents(&all)
}

fn dedup_agents(input: &[String]) -> Vec<String> {
    let mut set = BTreeSet::<String>::new();
    for item in input {
        let trimmed = item.trim();
        if !trimmed.is_empty() {
            set.insert(trimmed.to_string());
        }
    }
    set.into_iter().collect()
}
