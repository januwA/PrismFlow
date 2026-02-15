use std::{
    collections::HashSet,
    fs,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};

use anyhow::{Result, anyhow};
use futures::stream::{self, StreamExt};
use serde::Serialize;

use crate::{
    domain::{
        entities::{AppConfig, CiFailure, MonitoredRepo},
        ports::{ConfigRepository, GitHubRepository},
    },
    infrastructure::shell_adapter::ShellAdapter,
};

use super::review_workflow::EngineSpec;

#[derive(Debug, Clone, Default, Serialize)]
pub struct RepoCiStats {
    pub repo: String,
    pub analyzed: usize,
    pub skipped_no_failures: usize,
    pub skipped_completed: usize,
    pub failed: usize,
}

#[derive(Debug, Clone)]
pub struct CiWorkflowOptions {
    pub max_concurrent_repos: usize,
    pub engine_specs: Vec<EngineSpec>,
    pub engine_prompt: Option<String>,
    pub clone_repo_enabled: bool,
    pub clone_workspace_dir: String,
    pub clone_depth: usize,
    pub include_repos: Vec<String>,
    pub exclude_repos: Vec<String>,
}

impl Default for CiWorkflowOptions {
    fn default() -> Self {
        Self {
            max_concurrent_repos: 2,
            engine_specs: vec![],
            engine_prompt: None,
            clone_repo_enabled: false,
            clone_workspace_dir: ".prismflow/ci-repo-cache".to_string(),
            clone_depth: 1,
            include_repos: vec![],
            exclude_repos: vec![],
        }
    }
}

pub struct CiWorkflow<'a> {
    config_repo: &'a dyn ConfigRepository,
    github: &'a dyn GitHubRepository,
    shell: &'a dyn ShellAdapter,
    engine_fingerprint: String,
    options: CiWorkflowOptions,
    next_engine_idx: AtomicUsize,
}

impl<'a> CiWorkflow<'a> {
    pub fn new(
        config_repo: &'a dyn ConfigRepository,
        github: &'a dyn GitHubRepository,
        shell: &'a dyn ShellAdapter,
        options: CiWorkflowOptions,
    ) -> Self {
        let workflow_fp = workflow_engine_fingerprint(&options.engine_specs);
        Self {
            config_repo,
            github,
            shell,
            engine_fingerprint: workflow_fp,
            options,
            next_engine_idx: AtomicUsize::new(0),
        }
    }

    pub async fn run_once(&self) -> Result<Vec<RepoCiStats>> {
        let cfg: AppConfig = self.config_repo.load_config()?;
        let selector =
            RepoSelector::from_options(&self.options.include_repos, &self.options.exclude_repos);
        let repos = cfg
            .repos
            .into_iter()
            .filter(|r| selector.matches(&r.full_name))
            .collect::<Vec<_>>();

        let repo_concurrency = self.options.max_concurrent_repos.max(1);
        let mut results = stream::iter(repos.into_iter().enumerate())
            .map(|(idx, monitored)| async move { (idx, self.run_repo(monitored).await) })
            .buffer_unordered(repo_concurrency)
            .collect::<Vec<_>>()
            .await;
        results.sort_by_key(|(idx, _)| *idx);
        Ok(results.into_iter().map(|(_, stats)| stats).collect())
    }

    async fn run_repo(&self, monitored: MonitoredRepo) -> RepoCiStats {
        let mut stats = RepoCiStats {
            repo: monitored.full_name.clone(),
            ..RepoCiStats::default()
        };
        let (owner, repo) = match split_repo(&monitored.full_name) {
            Ok(v) => v,
            Err(_) => {
                stats.failed += 1;
                return stats;
            }
        };

        let prs = match self.github.list_open_pull_requests(owner, repo).await {
            Ok(v) => v,
            Err(_) => {
                stats.failed += 1;
                return stats;
            }
        };

        for pr in prs {
            let ci = match self
                .github
                .get_pull_request_ci_snapshot(owner, repo, pr.number)
                .await
            {
                Ok(v) => v,
                Err(_) => {
                    stats.failed += 1;
                    continue;
                }
            };

            if ci.failures.is_empty() {
                stats.skipped_no_failures += 1;
                continue;
            }

            let dedupe_key = ci_dedupe_key(
                &monitored.full_name,
                pr.number,
                &ci.head_sha,
                &self.engine_fingerprint,
            );
            let completed_anchor = ci_completed_anchor(&dedupe_key);
            let comments = match self
                .github
                .list_issue_comment_bodies(owner, repo, pr.number)
                .await
            {
                Ok(v) => v,
                Err(_) => {
                    stats.failed += 1;
                    continue;
                }
            };
            if comments.iter().any(|body| body.contains(&completed_anchor)) {
                stats.skipped_completed += 1;
                continue;
            }

            let selected_engine = match self.pick_engine() {
                Ok(v) => v,
                Err(_) => {
                    stats.failed += 1;
                    continue;
                }
            };

            let mut repo_dir_for_shell: Option<String> = None;
            let mut repo_head_ref_for_shell = String::new();
            if self.options.clone_repo_enabled {
                if let Ok(ctx) = self
                    .github
                    .get_pull_request_git_context(owner, repo, pr.number)
                    .await
                {
                    if let Ok(repo_dir) = prepare_repo_checkout(
                        &ctx.head_clone_url,
                        &ctx.head_sha,
                        &ctx.head_ref,
                        owner,
                        repo,
                        pr.number,
                        &self.options.clone_workspace_dir,
                        self.options.clone_depth,
                    ) {
                        repo_dir_for_shell = Some(repo_dir.to_string_lossy().to_string());
                        repo_head_ref_for_shell = ctx.head_ref;
                    }
                }
            }

            let payload = build_ci_payload(
                owner,
                repo,
                pr.number,
                &ci.head_sha,
                &ci.failures,
                self.options.engine_prompt.as_deref(),
            );
            let payload_file =
                match write_temp_ci_payload(&monitored.full_name, pr.number, &payload) {
                    Ok(v) => v,
                    Err(_) => {
                        stats.failed += 1;
                        continue;
                    }
                };

            let mut command = selected_engine.command.clone();
            let ci_file = payload_file.to_string_lossy().to_string();
            command = command.replace("{ci_file}", &ci_file);
            command = command.replace("{repo_dir}", repo_dir_for_shell.as_deref().unwrap_or(""));
            command = command.replace("{repo_head_sha}", &ci.head_sha);
            command = command.replace("{repo_head_ref}", &repo_head_ref_for_shell);
            let output = self
                .shell
                .run_command_line_in_dir(&command, repo_dir_for_shell.as_deref());
            let _ = fs::remove_file(&payload_file);

            let analysis = match output {
                Ok(v) => v,
                Err(_) => {
                    stats.failed += 1;
                    continue;
                }
            };

            let comment_body = format!(
                "{}\nPrismFlow CI analysis for `{}` (engine `{}`)\n\n{}",
                completed_anchor, ci.head_sha, selected_engine.fingerprint, analysis
            );
            if self
                .github
                .create_issue_comment(owner, repo, pr.number, &comment_body)
                .await
                .is_ok()
            {
                stats.analyzed += 1;
            } else {
                stats.failed += 1;
            }
        }
        stats
    }

    fn pick_engine(&self) -> Result<EngineSpec> {
        if self.options.engine_specs.is_empty() {
            return Err(anyhow!("missing engine specs"));
        }
        let idx = self.next_engine_idx.fetch_add(1, Ordering::Relaxed);
        let picked = self.options.engine_specs[idx % self.options.engine_specs.len()].clone();
        Ok(picked)
    }
}

#[derive(Debug, Clone)]
struct RepoSelector {
    includes: HashSet<String>,
    excludes: HashSet<String>,
}

impl RepoSelector {
    fn from_options(include_repos: &[String], exclude_repos: &[String]) -> Self {
        Self {
            includes: include_repos
                .iter()
                .map(|v| normalize_repo_selector(v))
                .filter(|v| !v.is_empty())
                .collect(),
            excludes: exclude_repos
                .iter()
                .map(|v| normalize_repo_selector(v))
                .filter(|v| !v.is_empty())
                .collect(),
        }
    }

    fn matches(&self, full_name: &str) -> bool {
        let key = normalize_repo_selector(full_name);
        if !self.includes.is_empty() && !self.includes.contains(&key) {
            return false;
        }
        !self.excludes.contains(&key)
    }
}

fn normalize_repo_selector(input: &str) -> String {
    input.trim().trim_end_matches('/').to_ascii_lowercase()
}

fn split_repo(full_name: &str) -> Result<(&str, &str)> {
    let mut parts = full_name.split('/');
    let owner = parts.next().unwrap_or_default();
    let repo = parts.next().unwrap_or_default();
    if owner.is_empty() || repo.is_empty() || parts.next().is_some() {
        return Err(anyhow!("invalid repo format: {full_name}"));
    }
    Ok((owner, repo))
}

fn ci_dedupe_key(repo: &str, pr: u64, head_sha: &str, engine_fingerprint: &str) -> String {
    format!("{repo}:{pr}:{head_sha}:{engine_fingerprint}")
}

fn workflow_engine_fingerprint(engine_specs: &[EngineSpec]) -> String {
    if engine_specs.is_empty() {
        return "default-engine".to_string();
    }
    if engine_specs.len() == 1 {
        return engine_specs[0].fingerprint.clone();
    }
    let mut out = String::new();
    for (idx, spec) in engine_specs.iter().enumerate() {
        if idx > 0 {
            out.push('+');
        }
        out.push_str(&spec.fingerprint);
    }
    out
}

fn ci_completed_anchor(key: &str) -> String {
    format!("<!-- prismflow:ci-completed:{} -->", key)
}

fn build_ci_payload(
    owner: &str,
    repo: &str,
    pr_number: u64,
    head_sha: &str,
    failures: &[CiFailure],
    engine_prompt: Option<&str>,
) -> String {
    let mut out = String::new();
    if let Some(prompt) = engine_prompt {
        out.push_str(prompt);
        out.push_str("\n\n");
    }
    out.push_str("# PrismFlow CI Failures\n");
    out.push_str(&format!(
        "Repo: {owner}/{repo}\nPR: #{pr_number}\nSHA: {head_sha}\n\n"
    ));
    for (idx, f) in failures.iter().enumerate() {
        out.push_str(&format!(
            "## Failure {}\nsource: {}\nname: {}\nconclusion: {}\n",
            idx + 1,
            f.source,
            f.name,
            f.conclusion
        ));
        if let Some(url) = &f.details_url {
            out.push_str(&format!("details_url: {url}\n"));
        }
        if let Some(summary) = &f.summary {
            out.push_str(&format!("summary:\n{}\n", summary));
        }
        if let Some(text) = &f.text {
            out.push_str(&format!("text:\n{}\n", text));
        }
        out.push('\n');
    }
    out
}

fn write_temp_ci_payload(repo: &str, pr_number: u64, content: &str) -> Result<PathBuf> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let root = cwd.join(".prismflow").join("tmp-ci");
    fs::create_dir_all(&root)?;
    let sanitized = repo.replace('/', "_");
    let path = root.join(format!("prismflow_{}_{}_ci.txt", sanitized, pr_number));
    fs::write(&path, content)?;
    Ok(path)
}

fn prepare_repo_checkout(
    clone_url: &str,
    head_sha: &str,
    head_ref: &str,
    owner: &str,
    repo: &str,
    pr_number: u64,
    workspace_dir: &str,
    clone_depth: usize,
) -> Result<PathBuf> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let root = cwd.join(workspace_dir);
    fs::create_dir_all(&root)?;
    let dir_name = format!(
        "{}_{}_pr{}_{}",
        owner.replace('/', "_"),
        repo.replace('/', "_"),
        pr_number,
        &head_sha.chars().take(12).collect::<String>()
    );
    let target = root.join(dir_name);
    let target_str = target.to_string_lossy().to_string();
    let depth_str = clone_depth.max(1).to_string();
    if !target.join(".git").exists() {
        let status = Command::new("git")
            .args(["clone", "--no-checkout", clone_url, &target_str])
            .status()?;
        if !status.success() {
            anyhow::bail!("git clone failed for {}", clone_url);
        }
    }
    let fetch_status = Command::new("git")
        .args([
            "-C",
            &target_str,
            "fetch",
            "--depth",
            &depth_str,
            "origin",
            head_sha,
        ])
        .status()?;
    if !fetch_status.success() {
        let refspec = format!("refs/heads/{head_ref}");
        let fetch_ref_status = Command::new("git")
            .args([
                "-C",
                &target_str,
                "fetch",
                "--depth",
                &depth_str,
                "origin",
                &refspec,
            ])
            .status()?;
        if !fetch_ref_status.success() {
            anyhow::bail!("git fetch failed for sha={} ref={}", head_sha, head_ref);
        }
    }
    let checkout_status = Command::new("git")
        .args(["-C", &target_str, "checkout", "--force", head_sha])
        .status()?;
    if !checkout_status.success() {
        anyhow::bail!("git checkout failed for sha={}", head_sha);
    }
    Ok(target)
}
