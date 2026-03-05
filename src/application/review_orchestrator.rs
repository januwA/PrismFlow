use std::sync::Arc;

use anyhow::{Result, anyhow};

use crate::application::context::TaskContext;
use crate::domain::{
    errors::DomainError,
    ports::{ConfigRepository, FileSystem, GitHubRepository, GitService, ShellAdapter},
};

use super::{
    agent_preflight::{panic_if_cli_agents_missing, panic_if_required_agents_missing},
    agent_service::FileSystemAgentPromptService,
    cache_service::CacheCleanupService,
    review_workflow::{RepoReviewStats, ReviewWorkflow, ReviewWorkflowOptions},
};

/// 审查编排器 - 负责高层流程编排
pub struct ReviewOrchestrator<'a> {
    config_repo: &'a dyn ConfigRepository,
    fs: &'a dyn FileSystem,
    cache_service: CacheCleanupService<'a>,
}

impl<'a> ReviewOrchestrator<'a> {
    pub fn new(config_repo: &'a dyn ConfigRepository, fs: &'a dyn FileSystem) -> Self {
        Self {
            config_repo,
            fs,
            cache_service: CacheCleanupService::new(fs),
        }
    }

    /// 执行单次审查
    pub async fn run_once(
        &self,
        github: &dyn GitHubRepository,
        shell: Option<&dyn ShellAdapter>,
        git: &dyn GitService,
        mut options: ReviewWorkflowOptions,
        ctx: Arc<TaskContext>,
        cache_cleanup_hours: u64,
        _archive_report_on_failure_only: bool,
        status_tx: Option<&tokio::sync::broadcast::Sender<String>>,
    ) -> Result<Vec<RepoReviewStats>> {
        // 1. 检查取消
        if ctx.is_cancelled() {
            return Err(anyhow!(DomainError::CancelledBySignal));
        }

        // 2. 缓存清理
        self.cache_service.cleanup_if_needed(cache_cleanup_hours);

        // 3. 配置加载与验证
        let config = self.config_repo.load_config()?;
        if config.repos.is_empty() {
            println!("no repositories configured");
            return Ok(Vec::new());
        }

        // 4. 创建 Agent 服务并执行预检
        let agent_service =
            FileSystemAgentPromptService::new(self.fs, options.agent_prompt_dirs.clone());
        let required_agents = if options.cli_agents.is_empty() {
            config.repos.iter().flat_map(|r| r.agents.clone()).collect()
        } else {
            options.cli_agents.clone()
        };
        panic_if_required_agents_missing(&agent_service, &config, &required_agents, "review-once");
        options.task_context = Some(ctx.clone());

        // 5. 执行工作流
        let workflow = ReviewWorkflow::new(self.config_repo, github, shell, self.fs, git, options);
        let stats = run_with_heartbeat("review-once", workflow.review_once(), &ctx).await?;

        // 6. 输出结果
        self.print_review_results(&stats, status_tx, &ctx);

        Ok(stats)
    }

    /// 执行 ad-hoc 单 PR 审查
    pub async fn run_ad_hoc(
        &self,
        github: &dyn GitHubRepository,
        shell: Option<&dyn ShellAdapter>,
        git: &dyn GitService,
        owner: &str,
        repo: &str,
        pr_number: u64,
        mut options: ReviewWorkflowOptions,
        ctx: Arc<TaskContext>,
        cache_cleanup_hours: u64,
    ) -> Result<RepoReviewStats> {
        if ctx.is_cancelled() {
            return Err(anyhow!(DomainError::CancelledBySignal));
        }

        self.cache_service.cleanup_if_needed(cache_cleanup_hours);

        let agent_service =
            FileSystemAgentPromptService::new(self.fs, options.agent_prompt_dirs.clone());
        panic_if_cli_agents_missing(&agent_service, &options.cli_agents, "review-ad-hoc");

        options.task_context = Some(ctx.clone());
        let workflow = ReviewWorkflow::new(self.config_repo, github, shell, self.fs, git, options);
        let stats = run_with_heartbeat(
            "review-ad-hoc",
            workflow.review_ad_hoc(owner, repo, pr_number),
            &ctx,
        )
        .await?;

        self.print_single_review_result(&stats);
        Ok(stats)
    }

    fn print_review_results(
        &self,
        stats: &[RepoReviewStats],
        status_tx: Option<&tokio::sync::broadcast::Sender<String>>,
        ctx: &TaskContext,
    ) {
        if stats.is_empty() {
            println!("no repositories configured");
            return;
        }

        for item in stats {
            if let Some(tx) = status_tx {
                let _ = tx.send(format!(
                    "run_id={} repo={} processed={} skip_completed={} skip_processing={} skip_filtered={} skip_by_author={} skip_by_operator={} retryable_fail={} fatal_fail={} retryable_error={:?} fatal_error={:?}",
                    ctx.run_id(),
                    item.repo,
                    item.processed,
                    item.skipped_completed,
                    item.skipped_processing,
                    item.skipped_filtered,
                    item.skipped_by_author,
                    item.skipped_by_operator,
                    item.failed_retryable,
                    item.failed_fatal,
                    item.last_retryable_error,
                    item.last_fatal_error
                ));
            }

            log_repo_review_stats(item);
        }
    }

    fn print_single_review_result(&self, item: &RepoReviewStats) {
        log_repo_review_stats(item);
    }
}

fn log_repo_review_stats(item: &RepoReviewStats) {
    if item.failed_fatal > 0 || item.failed_retryable > 0 {
        tracing::error!(
            repo = %item.repo,
            processed = item.processed,
            skipped_completed = item.skipped_completed,
            skipped_processing = item.skipped_processing,
            skipped_filtered = item.skipped_filtered,
            skipped_by_author = item.skipped_by_author,
            skipped_by_operator = item.skipped_by_operator,
            recovered_stale_processing = item.recovered_stale_processing,
            failed_retryable = item.failed_retryable,
            failed_fatal = item.failed_fatal,
            last_retryable_error = ?item.last_retryable_error,
            last_fatal_error = ?item.last_fatal_error,
            "review repo failed summary"
        );
    }
}

/// 执行带心跳监控的异步任务
async fn run_with_heartbeat<T, F>(tag: &str, fut: F, ctx: &TaskContext) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    use tokio::time::{Duration, interval};

    let mut interval = interval(Duration::from_secs(5));
    let started = std::time::Instant::now();
    tokio::pin!(fut);

    loop {
        tokio::select! {
            res = &mut fut => {
                return res;
            }
            _ = interval.tick() => {
                let secs = started.elapsed().as_secs();
                tracing::info!(tag = tag, elapsed_secs = secs, "task is still running");
            }
            _ = ctx.cancelled() => {
                return Err(anyhow!(DomainError::CancelledBySignal).context(format!("{tag} cancelled")));
            }
        }
    }
}
