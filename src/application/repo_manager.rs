use chrono::Utc;

use anyhow::{Result, bail};

use crate::domain::{
    entities::{MonitoredRepo, ReviewFilterConfig},
    errors::DomainError,
    ports::ConfigRepository,
};

pub struct RepoManager<'a> {
    config_repo: &'a dyn ConfigRepository,
}

impl<'a> RepoManager<'a> {
    pub fn new(config_repo: &'a dyn ConfigRepository) -> Self {
        Self { config_repo }
    }

    pub fn add_repo(&self, full_name: &str) -> Result<String> {
        let full_name = normalize_repo_input(full_name)?;
        let mut cfg = self.config_repo.load_config()?;

        if cfg.repos.iter().any(|r| r.full_name == full_name) {
            bail!("repository already exists: {full_name}");
        }

        cfg.repos.push(MonitoredRepo {
            full_name: full_name.clone(),
            added_at: Utc::now().to_rfc3339(),
            last_sha: None,
            review_filter: ReviewFilterConfig::default(),
            agents: vec![],
        });

        self.config_repo.save_config(&cfg)?;
        Ok(full_name)
    }

    pub fn remove_repo(&self, full_name: &str) -> Result<()> {
        let mut cfg = self.config_repo.load_config()?;
        let before = cfg.repos.len();
        cfg.repos.retain(|r| r.full_name != full_name);

        if cfg.repos.len() == before {
            bail!("repository not found: {full_name}");
        }

        self.config_repo.save_config(&cfg)
    }

    pub fn list_repos(&self) -> Result<Vec<MonitoredRepo>> {
        let cfg = self.config_repo.load_config()?;
        Ok(cfg.repos)
    }

    pub fn add_agent(&self, full_name: &str, agent: &str) -> Result<()> {
        let mut cfg = self.config_repo.load_config()?;
        let repo = cfg
            .repos
            .iter_mut()
            .find(|r| r.full_name == full_name)
            .ok_or_else(|| anyhow::anyhow!("repository not found: {full_name}"))?;
        if !repo.agents.iter().any(|a| a == agent) {
            repo.agents.push(agent.to_string());
        }
        self.config_repo.save_config(&cfg)
    }

    pub fn remove_agent(&self, full_name: &str, agent: &str) -> Result<()> {
        let mut cfg = self.config_repo.load_config()?;
        let repo = cfg
            .repos
            .iter_mut()
            .find(|r| r.full_name == full_name)
            .ok_or_else(|| anyhow::anyhow!("repository not found: {full_name}"))?;
        let before = repo.agents.len();
        repo.agents.retain(|a| a != agent);
        if before == repo.agents.len() {
            anyhow::bail!("agent not found on repo: {agent}");
        }
        self.config_repo.save_config(&cfg)
    }

    pub fn list_agents(&self, full_name: &str) -> Result<Vec<String>> {
        let cfg = self.config_repo.load_config()?;
        let repo = cfg
            .repos
            .iter()
            .find(|r| r.full_name == full_name)
            .ok_or_else(|| anyhow::anyhow!("repository not found: {full_name}"))?;
        Ok(repo.agents.clone())
    }

    pub fn add_agent_prompt_dir(&self, dir: &str) -> Result<()> {
        let dir = dir.trim();
        if dir.is_empty() {
            anyhow::bail!("agent prompt dir cannot be empty");
        }
        let mut cfg = self.config_repo.load_config()?;
        if !cfg.agent_prompt_dirs.iter().any(|d| d == dir) {
            cfg.agent_prompt_dirs.push(dir.to_string());
        }
        self.config_repo.save_config(&cfg)
    }

    pub fn remove_agent_prompt_dir(&self, dir: &str) -> Result<()> {
        let mut cfg = self.config_repo.load_config()?;
        let before = cfg.agent_prompt_dirs.len();
        cfg.agent_prompt_dirs.retain(|d| d != dir);
        if before == cfg.agent_prompt_dirs.len() {
            anyhow::bail!("global agent prompt dir not found: {dir}");
        }
        self.config_repo.save_config(&cfg)
    }

    pub fn list_agent_prompt_dirs(&self) -> Result<Vec<String>> {
        let cfg = self.config_repo.load_config()?;
        Ok(cfg.agent_prompt_dirs)
    }
}

fn validate_repo_full_name(full_name: &str) -> Result<()> {
    let mut parts = full_name.split('/');
    let owner = parts.next().unwrap_or_default();
    let repo = parts.next().unwrap_or_default();
    let extra = parts.next();

    let valid = !owner.is_empty() && !repo.is_empty() && extra.is_none();
    if valid {
        Ok(())
    } else {
        Err(DomainError::InvalidRepoFormat(full_name.to_string()).into())
    }
}

fn normalize_repo_input(input: &str) -> Result<String> {
    let trimmed = input.trim().trim_end_matches('/');
    if let Some(full_name) = parse_github_repo_url(trimmed) {
        validate_repo_full_name(&full_name)?;
        return Ok(full_name);
    }

    validate_repo_full_name(trimmed)?;
    Ok(trimmed.to_string())
}

fn parse_github_repo_url(input: &str) -> Option<String> {
    let tail = if let Some(v) = input.strip_prefix("https://github.com/") {
        v
    } else if let Some(v) = input.strip_prefix("http://github.com/") {
        v
    } else if let Some(v) = input.strip_prefix("github.com/") {
        v
    } else {
        return None;
    };

    let parts = tail
        .split('/')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();
    if parts.len() < 2 {
        return None;
    }

    let owner = parts[0].trim();
    let repo = parts[1].trim().trim_end_matches(".git");
    if owner.is_empty() || repo.is_empty() {
        return None;
    }

    Some(format!("{owner}/{repo}"))
}

#[cfg(test)]
mod tests {
    use super::normalize_repo_input;

    #[test]
    fn normalizes_owner_repo() {
        let out = normalize_repo_input("kevinanew/game_portal_react").expect("normalized");
        assert_eq!(out, "kevinanew/game_portal_react");
    }

    #[test]
    fn normalizes_repo_url() {
        let out = normalize_repo_input("https://github.com/kevinanew/game_portal_react")
            .expect("normalized");
        assert_eq!(out, "kevinanew/game_portal_react");
    }

    #[test]
    fn normalizes_pr_url() {
        let out = normalize_repo_input(
            "https://github.com/kevinanew/shell_game_portal_admin_react/pull/294",
        )
        .expect("normalized");
        assert_eq!(out, "kevinanew/shell_game_portal_admin_react");
    }
}
