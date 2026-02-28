use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use anyhow::{Context, Result, anyhow};
use futures::stream::{self, StreamExt};
use serde::Serialize;

use crate::{
    application::context::TaskContext,
    domain::{
        entities::{AppConfig, CiFailure, MonitoredRepo},
        ports::{
            CommandContext, ConfigRepository, FileSystem, GitHubRepository, GitService,
            ShellAdapter,
        },
    },
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
    pub task_context: Option<Arc<TaskContext>>,
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
            task_context: None,
        }
    }
}

pub struct CiWorkflow<'a> {
    config_repo: &'a dyn ConfigRepository,
    github: &'a dyn GitHubRepository,
    shell: &'a dyn ShellAdapter,
    fs: &'a dyn FileSystem,
    git: &'a dyn GitService,
    engine_fingerprint: String,
    options: CiWorkflowOptions,
    next_engine_idx: AtomicUsize,
}

impl<'a> CiWorkflow<'a> {
    pub fn new(
        config_repo: &'a dyn ConfigRepository,
        github: &'a dyn GitHubRepository,
        shell: &'a dyn ShellAdapter,
        fs: &'a dyn FileSystem,
        git: &'a dyn GitService,
        options: CiWorkflowOptions,
    ) -> Self {
        let workflow_fp = workflow_engine_fingerprint(&options.engine_specs);
        Self {
            config_repo,
            github,
            shell,
            fs,
            git,
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
            if self
                .options
                .task_context
                .as_ref()
                .map(|ctx| ctx.is_cancelled())
                .unwrap_or(false)
            {
                break;
            }
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
                    if let Ok(repo_dir) = self
                        .prepare_repo_checkout(
                            &ctx.head_clone_url,
                            &ctx.head_sha,
                            &ctx.head_ref,
                            owner,
                            repo,
                            pr.number,
                        )
                        .await
                    {
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
                match self.write_temp_ci_payload(&monitored.full_name, pr.number, &payload) {
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
            println!(
                "[ENGINE] repo={}/{} pr={} engine={} command_line={}",
                owner, repo, pr.number, selected_engine.fingerprint, command
            );
            let output = self
                .shell
                .run_command_line_in_dir(
                    &command,
                    repo_dir_for_shell.as_deref(),
                    self.options
                        .task_context
                        .as_deref()
                        .map(|ctx| ctx as &dyn CommandContext),
                )
                .await;

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

    fn write_temp_ci_payload(&self, repo: &str, pr_number: u64, content: &str) -> Result<PathBuf> {
        let cwd = self.fs.current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let root = cwd.join(".prismflow").join("tmp-ci");
        self.fs.create_dir_all(&root)?;
        let sanitized = repo.replace('/', "_");
        let path = root.join(format!("prismflow_{}_{}_ci.txt", sanitized, pr_number));
        self.fs.write(&path, content.as_bytes())?;
        Ok(path)
    }

    async fn prepare_repo_checkout(
        &self,
        clone_url: &str,
        head_sha: &str,
        head_ref: &str,
        owner: &str,
        repo: &str,
        pr_number: u64,
    ) -> Result<PathBuf> {
        let cwd = self.fs.current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let root = cwd.join(&self.options.clone_workspace_dir);
        self.fs.create_dir_all(&root)?;
        let dir_name = format!(
            "{}_{}_pr{}_{}",
            owner.replace('/', "_"),
            repo.replace('/', "_"),
            pr_number,
            &head_sha.chars().take(12).collect::<String>()
        );
        let target = root.join(dir_name);

        let ctx = self
            .options
            .task_context
            .as_deref()
            .map(|c| c as &dyn CommandContext);

        if !self.fs.exists(&target.join(".git")) {
            self.git
                .clone_repo(clone_url, &target, ctx)
                .await
                .context("git clone failed")?;
        }

        // Fetch by sha
        if let Err(_) = self
            .git
            .fetch(&target, "origin", head_sha, self.options.clone_depth, ctx)
            .await
        {
            // fallback fetch by ref
            let refspec = format!("refs/heads/{head_ref}");
            self.git
                .fetch(&target, "origin", &refspec, self.options.clone_depth, ctx)
                .await
                .context("git fetch by ref failed")?;
        }

        self.git
            .checkout(&target, head_sha, ctx)
            .await
            .context("git checkout failed")?;

        Ok(target)
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

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use anyhow::{Result, anyhow};
    use async_trait::async_trait;

    use super::{CiWorkflow, CiWorkflowOptions, EngineSpec};
    use crate::application::context::TaskContext;
    use crate::domain::entities::{
        AppConfig, MonitoredRepo, PullRequestCiSnapshot, PullRequestFilePatch,
        PullRequestGitContext, PullRequestMetrics, PullRequestSummary, ReviewFilterConfig,
    };
    use crate::domain::ports::{
        CommandContext, ConfigRepository, FileSystem, GitHubRepository, GitService, ShellAdapter,
    };
    use std::sync::Arc;

    struct InMemoryConfigRepo {
        inner: Mutex<AppConfig>,
    }

    impl InMemoryConfigRepo {
        fn new(cfg: AppConfig) -> Self {
            Self {
                inner: Mutex::new(cfg),
            }
        }
    }

    impl ConfigRepository for InMemoryConfigRepo {
        fn load_config(&self) -> Result<AppConfig> {
            Ok(self.inner.lock().expect("lock").clone())
        }

        fn save_config(&self, config: &AppConfig) -> Result<()> {
            *self.inner.lock().expect("lock") = config.clone();
            Ok(())
        }

        fn config_path(&self) -> &std::path::Path {
            std::path::Path::new("memory")
        }
    }

    struct MinimalGitHub {
        prs: Vec<PullRequestSummary>,
    }

    #[async_trait]
    impl GitHubRepository for MinimalGitHub {
        async fn current_user_login(&self) -> Result<String> {
            panic!("unexpected call")
        }

        async fn list_open_pull_requests(
            &self,
            _owner: &str,
            _repo: &str,
        ) -> Result<Vec<PullRequestSummary>> {
            Ok(self.prs.clone())
        }

        async fn get_pull_request(
            &self,
            _owner: &str,
            _repo: &str,
            _pull_number: u64,
        ) -> Result<PullRequestSummary> {
            panic!("unexpected call")
        }

        async fn get_pull_request_git_context(
            &self,
            _owner: &str,
            _repo: &str,
            _pull_number: u64,
        ) -> Result<PullRequestGitContext> {
            panic!("unexpected call")
        }

        async fn get_pull_request_metrics(
            &self,
            _owner: &str,
            _repo: &str,
            _pull_number: u64,
        ) -> Result<PullRequestMetrics> {
            panic!("unexpected call")
        }

        async fn get_pull_request_ci_snapshot(
            &self,
            _owner: &str,
            _repo: &str,
            _pull_number: u64,
        ) -> Result<PullRequestCiSnapshot> {
            panic!("unexpected call")
        }

        async fn list_pull_request_files(
            &self,
            _owner: &str,
            _repo: &str,
            _pull_number: u64,
        ) -> Result<Vec<PullRequestFilePatch>> {
            panic!("unexpected call")
        }

        async fn list_issue_comment_bodies(
            &self,
            _owner: &str,
            _repo: &str,
            _issue_number: u64,
        ) -> Result<Vec<String>> {
            panic!("unexpected call")
        }

        async fn list_issue_comments(
            &self,
            _owner: &str,
            _repo: &str,
            _issue_number: u64,
        ) -> Result<Vec<crate::domain::entities::SimpleComment>> {
            panic!("unexpected call")
        }

        async fn create_issue_comment(
            &self,
            _owner: &str,
            _repo: &str,
            _issue_number: u64,
            _body: &str,
        ) -> Result<()> {
            panic!("unexpected call")
        }

        async fn submit_inline_review(
            &self,
            _owner: &str,
            _repo: &str,
            _pull_number: u64,
            _body: &str,
            _comments: &[crate::domain::entities::ReviewComment],
        ) -> Result<()> {
            panic!("unexpected call")
        }

        async fn list_issue_labels(
            &self,
            _owner: &str,
            _repo: &str,
            _issue_number: u64,
        ) -> Result<Vec<String>> {
            panic!("unexpected call")
        }

        async fn add_issue_labels(
            &self,
            _owner: &str,
            _repo: &str,
            _issue_number: u64,
            _labels: &[String],
        ) -> Result<()> {
            panic!("unexpected call")
        }

        async fn remove_issue_label(
            &self,
            _owner: &str,
            _repo: &str,
            _issue_number: u64,
            _label: &str,
        ) -> Result<()> {
            panic!("unexpected call")
        }

        async fn list_pull_review_comments(
            &self,
            _owner: &str,
            _repo: &str,
            _pull_number: u64,
        ) -> Result<Vec<crate::domain::entities::SimpleComment>> {
            panic!("unexpected call")
        }

        async fn list_pull_reviews(
            &self,
            _owner: &str,
            _repo: &str,
            _pull_number: u64,
        ) -> Result<Vec<crate::domain::entities::SimplePullReview>> {
            panic!("unexpected call")
        }

        async fn delete_issue_comment(
            &self,
            _owner: &str,
            _repo: &str,
            _comment_id: u64,
        ) -> Result<()> {
            panic!("unexpected call")
        }

        async fn delete_pull_review_comment(
            &self,
            _owner: &str,
            _repo: &str,
            _comment_id: u64,
        ) -> Result<()> {
            panic!("unexpected call")
        }

        async fn delete_pending_pull_review(
            &self,
            _owner: &str,
            _repo: &str,
            _pull_number: u64,
            _review_id: u64,
        ) -> Result<()> {
            panic!("unexpected call")
        }

        async fn dismiss_pull_review(
            &self,
            _owner: &str,
            _repo: &str,
            _pull_number: u64,
            _review_id: u64,
            _message: &str,
        ) -> Result<()> {
            panic!("unexpected call")
        }
    }

    #[derive(Default)]
    struct MockShell;

    #[async_trait::async_trait]
    impl ShellAdapter for MockShell {
        fn run_capture(&self, _program: &str, _args: &[&str]) -> Result<String> {
            Ok(String::new())
        }

        async fn run_command_line(
            &self,
            _command_line: &str,
            _ctx: Option<&dyn CommandContext>,
        ) -> Result<String> {
            Ok(String::new())
        }

        async fn run_command_line_in_dir(
            &self,
            _command_line: &str,
            _workdir: Option<&str>,
            _ctx: Option<&dyn CommandContext>,
        ) -> Result<String> {
            Ok(String::new())
        }
    }

    struct MockFileSystem;
    impl FileSystem for MockFileSystem {
        fn create_dir_all(&self, _path: &std::path::Path) -> Result<()> {
            Ok(())
        }
        fn write(&self, _path: &std::path::Path, _content: &[u8]) -> Result<()> {
            Ok(())
        }
        fn read_to_string(&self, _path: &std::path::Path) -> Result<String> {
            Ok(String::new())
        }
        fn remove_file(&self, _path: &std::path::Path) -> Result<()> {
            Ok(())
        }
        fn current_dir(&self) -> Result<std::path::PathBuf> {
            Ok(std::path::PathBuf::from("."))
        }
        fn config_dir(&self) -> Option<std::path::PathBuf> {
            None
        }
        fn exists(&self, _path: &std::path::Path) -> bool {
            false
        }
    }

    struct MockGit;
    #[async_trait::async_trait]
    impl GitService for MockGit {
        async fn clone_repo(
            &self,
            _url: &str,
            _target_dir: &std::path::Path,
            _ctx: Option<&dyn CommandContext>,
        ) -> Result<()> {
            Ok(())
        }
        async fn fetch(
            &self,
            _target_dir: &std::path::Path,
            _remote: &str,
            _refspec: &str,
            _depth: usize,
            _ctx: Option<&dyn CommandContext>,
        ) -> Result<()> {
            Ok(())
        }
        async fn checkout(
            &self,
            _target_dir: &std::path::Path,
            _rev: &str,
            _ctx: Option<&dyn CommandContext>,
        ) -> Result<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct RecordingGit {
        fetch_calls: Mutex<Vec<String>>,
        checkout_calls: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl GitService for RecordingGit {
        async fn clone_repo(
            &self,
            _url: &str,
            _target_dir: &std::path::Path,
            _ctx: Option<&dyn CommandContext>,
        ) -> Result<()> {
            Ok(())
        }

        async fn fetch(
            &self,
            _target_dir: &std::path::Path,
            _remote: &str,
            refspec: &str,
            _depth: usize,
            _ctx: Option<&dyn CommandContext>,
        ) -> Result<()> {
            let mut calls = self.fetch_calls.lock().expect("lock fetch_calls");
            calls.push(refspec.to_string());
            if calls.len() == 1 {
                return Err(anyhow!("fetch by sha failed"));
            }
            Ok(())
        }

        async fn checkout(
            &self,
            _target_dir: &std::path::Path,
            rev: &str,
            _ctx: Option<&dyn CommandContext>,
        ) -> Result<()> {
            self.checkout_calls
                .lock()
                .expect("lock checkout_calls")
                .push(rev.to_string());
            Ok(())
        }
    }

    fn config_with_repo() -> AppConfig {
        AppConfig {
            agent_prompt_dirs: vec![],
            repos: vec![MonitoredRepo {
                full_name: "owner/repo".to_string(),
                added_at: "2026-01-01T00:00:00Z".to_string(),
                last_sha: None,
                review_filter: ReviewFilterConfig::default(),
                agents: vec![],
            }],
        }
    }

    #[tokio::test]
    async fn run_once_respects_cancelled_task_context() {
        let github = MinimalGitHub {
            prs: vec![PullRequestSummary {
                number: 1,
                title: "t".to_string(),
                head_sha: "abc123".to_string(),
                html_url: None,
                author_login: Some("mock-user".to_string()),
            }],
        };
        let config = InMemoryConfigRepo::new(config_with_repo());
        let ctx = Arc::new(TaskContext::new("ci-cancelled-run-once"));
        ctx.cancel();
        let options = CiWorkflowOptions {
            engine_specs: vec![EngineSpec {
                fingerprint: "engine".to_string(),
                command: "echo ok".to_string(),
            }],
            task_context: Some(ctx),
            ..CiWorkflowOptions::default()
        };
        let shell = MockShell;
        let fs = MockFileSystem;
        let git = MockGit;
        let stats = CiWorkflow::new(&config, &github, &shell, &fs, &git, options)
            .run_once()
            .await
            .expect("run_once");

        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].analyzed, 0);
        assert_eq!(stats[0].failed, 0);
        assert_eq!(stats[0].skipped_no_failures, 0);
        assert_eq!(stats[0].skipped_completed, 0);
    }

    #[tokio::test]
    async fn prepare_repo_checkout_falls_back_to_ref_fetch_when_sha_fetch_fails() {
        let github = MinimalGitHub { prs: vec![] };
        let config = InMemoryConfigRepo::new(config_with_repo());
        let shell = MockShell;
        let fs = MockFileSystem;
        let git = RecordingGit::default();
        let workflow = CiWorkflow::new(
            &config,
            &github,
            &shell,
            &fs,
            &git,
            CiWorkflowOptions {
                clone_workspace_dir: ".prismflow/test-ci-cache".to_string(),
                clone_depth: 1,
                ..CiWorkflowOptions::default()
            },
        );

        let repo_dir = workflow
            .prepare_repo_checkout(
                "https://github.com/owner/repo.git",
                "abc123",
                "main",
                "owner",
                "repo",
                42,
            )
            .await
            .expect("prepare_repo_checkout should succeed with fallback fetch");

        let fetch_calls = git.fetch_calls.lock().expect("lock fetch_calls").clone();
        assert_eq!(fetch_calls.len(), 2);
        assert_eq!(fetch_calls[0], "abc123");
        assert_eq!(fetch_calls[1], "refs/heads/main");

        let checkout_calls = git
            .checkout_calls
            .lock()
            .expect("lock checkout_calls")
            .clone();
        assert_eq!(checkout_calls, vec!["abc123".to_string()]);
        assert!(
            repo_dir
                .to_string_lossy()
                .contains("owner_repo_pr42_abc123")
        );
    }
}
