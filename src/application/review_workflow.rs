use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use futures::stream::{self, StreamExt};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;
use tokio::time::sleep;

use crate::application::context::TaskContext;
use crate::domain::{
    entities::{
        AppConfig, MonitoredRepo, PullRequestFilePatch, PullRequestSummary, ReviewComment,
        ReviewFilterConfig,
    },
    ports::{
        CommandContext, ConfigRepository, FileSystem, GitHubRepository, GitService, ShellAdapter,
    },
};

const MAX_INLINE_COMMENTS: usize = 20;
const DEFAULT_RETRY_ATTEMPTS: usize = 3;
const DEFAULT_RETRY_BACKOFF_MS: u64 = 300;
const PROCESSING_TTL_SECS: i64 = 30 * 60;
const ADHOC_COOLDOWN_SECS: i64 = 10 * 60;
const DEFAULT_LARGE_PR_MAX_FILES: u64 = 120;
const DEFAULT_LARGE_PR_MAX_CHANGED_LINES: u64 = 4000;

#[derive(Debug, Clone)]
pub struct ScanPrReport {
    pub number: u64,
    pub title: String,
    pub url: Option<String>,
    pub anchor_key: String,
}

#[derive(Debug, Clone)]
pub struct ScanReport {
    pub repo: String,
    pub prs: Vec<ScanPrReport>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RepoReviewStats {
    pub repo: String,
    pub processed: usize,
    pub skipped_completed: usize,
    pub skipped_processing: usize,
    pub skipped_by_author: usize,
    pub recovered_stale_processing: usize,
    pub fallback_general: usize,
    pub failed_retryable: usize,
    pub failed_fatal: usize,
    pub skipped_filtered: usize,
    pub last_retryable_error: Option<String>,
    pub last_fatal_error: Option<String>,
    pub skipped_by_operator: usize,
}

#[derive(Debug, Clone)]
pub struct ReviewWorkflowOptions {
    pub max_concurrent_repos: usize,
    pub max_concurrent_prs: usize,
    pub retry_attempts: usize,
    pub retry_backoff_ms: u64,
    pub engine_specs: Vec<EngineSpec>,
    pub engine_start_index: usize,
    pub engine_prompt: Option<String>,
    pub prompt_template: Option<String>,
    pub agent_prompt_dirs: Vec<String>,
    pub keep_diff_files: bool,
    pub clone_repo_enabled: bool,
    pub clone_workspace_dir: String,
    pub clone_depth: usize,
    pub large_pr_max_files: u64,
    pub large_pr_max_changed_lines: u64,
    pub cli_agents: Vec<String>,
    pub include_repos: Vec<String>,
    pub exclude_repos: Vec<String>,
    pub include_authors: Vec<String>,
    pub exclude_authors: Vec<String>,
    pub status_tx: Option<broadcast::Sender<String>>,
    pub skip_flag: Option<Arc<AtomicBool>>,
    pub task_context: Option<Arc<TaskContext>>,
}

impl Default for ReviewWorkflowOptions {
    fn default() -> Self {
        Self {
            max_concurrent_repos: 2,
            max_concurrent_prs: 4,
            retry_attempts: DEFAULT_RETRY_ATTEMPTS,
            retry_backoff_ms: DEFAULT_RETRY_BACKOFF_MS,
            engine_specs: vec![],
            engine_start_index: 0,
            engine_prompt: None,
            prompt_template: None,
            agent_prompt_dirs: vec![],
            keep_diff_files: false,
            clone_repo_enabled: false,
            clone_workspace_dir: ".prismflow/repo-cache".to_string(),
            clone_depth: 1,
            large_pr_max_files: DEFAULT_LARGE_PR_MAX_FILES,
            large_pr_max_changed_lines: DEFAULT_LARGE_PR_MAX_CHANGED_LINES,
            cli_agents: vec![],
            include_repos: vec![],
            exclude_repos: vec![],
            include_authors: vec![],
            exclude_authors: vec![],
            status_tx: None,
            skip_flag: None,
            task_context: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EngineSpec {
    pub fingerprint: String,
    pub command: String,
}

pub struct ReviewWorkflow<'a> {
    config_repo: &'a dyn ConfigRepository,
    github: &'a dyn GitHubRepository,
    shell: Option<&'a dyn ShellAdapter>,
    fs: &'a dyn FileSystem,
    git: &'a dyn GitService,
    engine_fingerprint: String,
    options: ReviewWorkflowOptions,
    next_engine_idx: AtomicUsize,
}

impl<'a> ReviewWorkflow<'a> {
    pub fn new(
        config_repo: &'a dyn ConfigRepository,
        github: &'a dyn GitHubRepository,
        shell: Option<&'a dyn ShellAdapter>,
        fs: &'a dyn FileSystem,
        git: &'a dyn GitService,
        options: ReviewWorkflowOptions,
    ) -> Self {
        let workflow_fp = workflow_engine_fingerprint(&options.engine_specs);
        Self {
            config_repo,
            github,
            shell,
            fs,
            git,
            engine_fingerprint: workflow_fp,
            next_engine_idx: AtomicUsize::new(options.engine_start_index),
            options,
        }
    }

    pub async fn scan_once(&self) -> Result<Vec<ScanReport>> {
        let cfg = self.config_repo.load_config()?;
        let mut reports = Vec::new();
        let selector = RepoSelector::from_options(&self.options);

        for monitored in cfg.repos {
            if !selector.matches(&monitored.full_name) {
                continue;
            }
            let (owner, repo) = split_repo(&monitored.full_name)?;
            let prs = self.github.list_open_pull_requests(owner, repo).await?;

            let prs = prs
                .into_iter()
                .map(|pr| ScanPrReport {
                    number: pr.number,
                    title: pr.title,
                    url: pr.html_url,
                    anchor_key: dedupe_key(&monitored.full_name, pr.number, &pr.head_sha, "scan"),
                })
                .collect::<Vec<_>>();

            reports.push(ScanReport {
                repo: monitored.full_name,
                prs,
            });
        }

        Ok(reports)
    }

    pub async fn review_once(&self) -> Result<Vec<RepoReviewStats>> {
        let mut cfg: AppConfig = self.config_repo.load_config()?;
        let selector = RepoSelector::from_options(&self.options);
        let repos = cfg
            .repos
            .iter()
            .enumerate()
            .filter(|(_, repo)| selector.matches(&repo.full_name))
            .map(|(idx, repo)| (idx, repo.clone()))
            .collect::<Vec<_>>();
        let repo_concurrency = self.options.max_concurrent_repos.max(1);

        let mut results = stream::iter(repos.into_iter())
            .map(|(idx, monitored)| async move {
                let report = self.review_repo(monitored).await;
                (idx, report)
            })
            .buffer_unordered(repo_concurrency)
            .collect::<Vec<_>>()
            .await;

        results.sort_by_key(|(idx, _)| *idx);

        let mut stats = Vec::new();
        for (idx, report) in results {
            if let Some(last_sha) = report.last_processed_sha {
                if let Some(repo) = cfg.repos.get_mut(idx) {
                    repo.last_sha = Some(last_sha);
                }
            }
            stats.push(report.stats);
        }

        self.config_repo.save_config(&cfg)?;
        Ok(stats)
    }

    pub async fn review_ad_hoc(
        &self,
        owner: &str,
        repo: &str,
        pr_number: u64,
    ) -> Result<RepoReviewStats> {
        let mut stats = RepoReviewStats {
            repo: format!("{owner}/{repo}"),
            ..RepoReviewStats::default()
        };
        let pr = self.github.get_pull_request(owner, repo, pr_number).await?;
        let filter = ReviewFilterConfig::default();
        let agents = self.options.cli_agents.clone();
        let outcome = self
            .review_single_pr(owner, repo, &stats.repo, &filter, &agents, pr, true)
            .await;
        match outcome {
            PrReviewOutcome::Processed {
                used_fallback,
                recovered_stale,
                ..
            } => {
                stats.processed += 1;
                if used_fallback {
                    stats.fallback_general += 1;
                }
                if recovered_stale {
                    stats.recovered_stale_processing += 1;
                }
            }
            PrReviewOutcome::SkippedCompleted => stats.skipped_completed += 1,
            PrReviewOutcome::SkippedProcessing => stats.skipped_processing += 1,
            PrReviewOutcome::SkippedFiltered => stats.skipped_filtered += 1,
            PrReviewOutcome::SkippedByOperator => stats.skipped_by_operator += 1,
            PrReviewOutcome::FailedRetryable(msg) => {
                stats.failed_retryable += 1;
                stats.last_retryable_error = Some(msg);
            }
            PrReviewOutcome::FailedFatal(msg) => {
                stats.failed_fatal += 1;
                stats.last_fatal_error = Some(msg);
            }
        }
        Ok(stats)
    }

    async fn review_repo(&self, monitored: MonitoredRepo) -> RepoReviewReport {
        let mut stats = RepoReviewStats {
            repo: monitored.full_name.clone(),
            ..RepoReviewStats::default()
        };

        let (owner, repo) = match split_repo(&monitored.full_name) {
            Ok(v) => v,
            Err(_) => {
                stats.failed_fatal += 1;
                return RepoReviewReport {
                    stats,
                    last_processed_sha: None,
                };
            }
        };

        let prs = match self.github.list_open_pull_requests(owner, repo).await {
            Ok(v) => v,
            Err(err) => {
                match classify_error(&err) {
                    ErrorClass::Retryable => stats.failed_retryable += 1,
                    ErrorClass::Fatal => stats.failed_fatal += 1,
                }
                return RepoReviewReport {
                    stats,
                    last_processed_sha: None,
                };
            }
        };
        let author_selector = AuthorSelector::from_options(&self.options);
        let mut selected_prs = Vec::new();
        for pr in prs {
            if author_selector.matches(pr.author_login.as_deref()) {
                selected_prs.push(pr);
            } else {
                stats.skipped_by_author += 1;
                self.mark_stage(ReviewStage::Skipped, &monitored.full_name, pr.number);
            }
        }

        let pr_concurrency = self.options.max_concurrent_prs.max(1);
        let repo_name_for_tasks = monitored.full_name.clone();
        let filter_for_tasks = monitored.review_filter.clone();
        let agents_for_tasks = if self.options.cli_agents.is_empty() {
            monitored.agents.clone()
        } else {
            self.options.cli_agents.clone()
        };
        let outcomes = stream::iter(selected_prs)
            .map(|pr| {
                let repo_name_for_tasks = repo_name_for_tasks.clone();
                let filter_for_tasks = filter_for_tasks.clone();
                let agents_for_tasks = agents_for_tasks.clone();
                async move {
                    self.review_single_pr(
                        owner,
                        repo,
                        &repo_name_for_tasks,
                        &filter_for_tasks,
                        &agents_for_tasks,
                        pr,
                        false,
                    )
                    .await
                }
            })
            .buffer_unordered(pr_concurrency)
            .collect::<Vec<_>>()
            .await;

        let mut last_sha: Option<String> = None;
        for outcome in outcomes {
            match outcome {
                PrReviewOutcome::Processed {
                    sha,
                    used_fallback,
                    recovered_stale,
                } => {
                    stats.processed += 1;
                    if used_fallback {
                        stats.fallback_general += 1;
                    }
                    if recovered_stale {
                        stats.recovered_stale_processing += 1;
                    }
                    last_sha = Some(sha);
                }
                PrReviewOutcome::SkippedCompleted => stats.skipped_completed += 1,
                PrReviewOutcome::SkippedProcessing => stats.skipped_processing += 1,
                PrReviewOutcome::SkippedFiltered => stats.skipped_filtered += 1,
                PrReviewOutcome::SkippedByOperator => stats.skipped_by_operator += 1,
                PrReviewOutcome::FailedRetryable(msg) => {
                    stats.failed_retryable += 1;
                    stats.last_retryable_error = Some(msg);
                }
                PrReviewOutcome::FailedFatal(msg) => {
                    stats.failed_fatal += 1;
                    stats.last_fatal_error = Some(msg);
                }
            }
        }

        RepoReviewReport {
            stats,
            last_processed_sha: last_sha,
        }
    }

    async fn review_single_pr(
        &self,
        owner: &str,
        repo: &str,
        full_repo_name: &str,
        filter: &ReviewFilterConfig,
        agents: &[String],
        pr: PullRequestSummary,
        force_run: bool,
    ) -> PrReviewOutcome {
        if self
            .options
            .task_context
            .as_ref()
            .map(|ctx| ctx.is_cancelled())
            .unwrap_or(false)
        {
            return PrReviewOutcome::FailedRetryable("task cancelled".to_string());
        }
        if self.consume_skip_by_operator() {
            self.mark_stage(ReviewStage::Skipped, full_repo_name, pr.number);
            return PrReviewOutcome::SkippedByOperator;
        }
        self.mark_stage(ReviewStage::Queued, full_repo_name, pr.number);
        let key = dedupe_key(
            full_repo_name,
            pr.number,
            &pr.head_sha,
            &self.engine_fingerprint,
        );
        let processing_prefix = processing_anchor_prefix(&key);
        let completed_anchor = completed_anchor(&key);

        self.mark_stage(ReviewStage::Fetching, full_repo_name, pr.number);
        let comments = match self
            .github
            .list_issue_comment_bodies(owner, repo, pr.number)
            .await
        {
            Ok(v) => v,
            Err(err) => {
                return match classify_error(&err) {
                    ErrorClass::Retryable => PrReviewOutcome::FailedRetryable(err.to_string()),
                    ErrorClass::Fatal => PrReviewOutcome::FailedFatal(err.to_string()),
                };
            }
        };
        let labels = match self.github.list_issue_labels(owner, repo, pr.number).await {
            Ok(v) => v,
            Err(err) => {
                return match classify_error(&err) {
                    ErrorClass::Retryable => PrReviewOutcome::FailedRetryable(err.to_string()),
                    ErrorClass::Fatal => PrReviewOutcome::FailedFatal(err.to_string()),
                };
            }
        };
        let reviewed_label = reviewed_label_for_sha(&pr.head_sha);
        let large_skip_label = large_skipped_label_for_sha(&pr.head_sha);
        let previous_reviewed_short_sha = labels.iter().find_map(|l| {
            l.strip_prefix("pr-reviewer:reviewed:")
                .filter(|short| *short != pr.head_sha.get(0..12).unwrap_or_default())
                .map(|v| v.to_string())
        });

        if !force_run {
            if comments.iter().any(|body| body.contains(&completed_anchor))
                || labels.iter().any(|l| l == &reviewed_label)
                || labels.iter().any(|l| l == &large_skip_label)
            {
                return PrReviewOutcome::SkippedCompleted;
            }

            if let Some(latest_ts) = newest_processing_ts(&comments, &processing_prefix) {
                let age_secs = now_unix_secs() - latest_ts;
                if age_secs <= PROCESSING_TTL_SECS {
                    return PrReviewOutcome::SkippedProcessing;
                }
            }
        } else if let Some(latest_ts) = newest_processing_ts(&comments, &processing_prefix) {
            let age_secs = now_unix_secs() - latest_ts;
            if age_secs <= ADHOC_COOLDOWN_SECS {
                return PrReviewOutcome::SkippedProcessing;
            }
        }

        let metrics = self
            .github
            .get_pull_request_metrics(owner, repo, pr.number)
            .await
            .unwrap_or_default();
        if metrics.changed_files >= self.options.large_pr_max_files
            || metrics.additions.saturating_add(metrics.deletions)
                >= self.options.large_pr_max_changed_lines
        {
            if let Err(err) = self
                .sync_large_skip_labels(owner, repo, pr.number, &large_skip_label)
                .await
            {
                return match classify_error(&err) {
                    ErrorClass::Retryable => PrReviewOutcome::FailedRetryable(err.to_string()),
                    ErrorClass::Fatal => PrReviewOutcome::FailedFatal(err.to_string()),
                };
            }
            self.mark_stage(ReviewStage::Skipped, full_repo_name, pr.number);
            return PrReviewOutcome::SkippedFiltered;
        }

        let selected_engine = match self.pick_engine_for_pr() {
            Ok(v) => v,
            Err(err) => {
                return match classify_error(&err) {
                    ErrorClass::Retryable => PrReviewOutcome::FailedRetryable(err.to_string()),
                    ErrorClass::Fatal => PrReviewOutcome::FailedFatal(err.to_string()),
                };
            }
        };
        let selected_engine_fingerprint = selected_engine.fingerprint.as_str();

        let processing_body = format!(
            "{}\nPrismFlow started review for `{}` with engine `{}`.",
            processing_anchor(&key),
            pr.head_sha,
            selected_engine_fingerprint
        );

        if let Err(err) = self
            .with_retry(|| {
                self.github
                    .create_issue_comment(owner, repo, pr.number, &processing_body)
            })
            .await
        {
            return match classify_error(&err) {
                ErrorClass::Retryable => PrReviewOutcome::FailedRetryable(err.to_string()),
                ErrorClass::Fatal => PrReviewOutcome::FailedFatal(err.to_string()),
            };
        }

        let files = match self
            .github
            .list_pull_request_files(owner, repo, pr.number)
            .await
        {
            Ok(v) => v,
            Err(err) => {
                return match classify_error(&err) {
                    ErrorClass::Retryable => PrReviewOutcome::FailedRetryable(err.to_string()),
                    ErrorClass::Fatal => PrReviewOutcome::FailedFatal(err.to_string()),
                };
            }
        };
        let files = apply_repo_file_filter(&files, filter);
        self.mark_stage(ReviewStage::Analyzing, full_repo_name, pr.number);
        if files.is_empty() {
            let completed_body = format!(
                "{}\nPrismFlow completed review for `{}` (all files filtered by repo rules).",
                completed_anchor, pr.head_sha
            );
            let add_completed = self
                .with_retry(|| {
                    self.github
                        .create_issue_comment(owner, repo, pr.number, &completed_body)
                })
                .await;
            if add_completed.is_err() {
                return PrReviewOutcome::FailedRetryable(
                    "operation failed without detailed error context".to_string(),
                );
            }

            if self
                .sync_reviewed_labels(owner, repo, pr.number, &reviewed_label)
                .await
                .is_err()
            {
                return PrReviewOutcome::FailedRetryable(
                    "operation failed without detailed error context".to_string(),
                );
            }
            return PrReviewOutcome::SkippedFiltered;
        }

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

        let effective_prompt = match self.resolve_effective_prompt(agents) {
            Ok(v) => v,
            Err(err) => {
                return match classify_error(&err) {
                    ErrorClass::Retryable => PrReviewOutcome::FailedRetryable(err.to_string()),
                    ErrorClass::Fatal => PrReviewOutcome::FailedFatal(err.to_string()),
                };
            }
        };
        let analysis = match self
            .analyze_review(
                owner,
                repo,
                pr.number,
                &pr.head_sha,
                &files,
                effective_prompt.as_deref(),
                &selected_engine,
                repo_dir_for_shell.as_deref(),
                &repo_head_ref_for_shell,
            )
            .await
        {
            Ok(v) => v,
            Err(err) => {
                return match classify_error(&err) {
                    ErrorClass::Retryable => PrReviewOutcome::FailedRetryable(err.to_string()),
                    ErrorClass::Fatal => PrReviewOutcome::FailedFatal(err.to_string()),
                };
            }
        };

        let outdated_notice = previous_reviewed_short_sha
            .as_ref()
            .map(|old| format!("Previous review for `{old}` is outdated.\n\n"));
        let summary_with_outdated = match outdated_notice {
            Some(ref notice) => format!("{notice}{}", analysis.summary),
            None => analysis.summary.clone(),
        };

        let review_body = format!(
            "PrismFlow review summary for `{}`\n\n- Engine: `{}`\n- Analyzer: `{}`\n- Inline findings: {}\n\n{}",
            pr.head_sha,
            selected_engine_fingerprint,
            analysis.source,
            analysis.inline_comments.len(),
            summary_with_outdated
        );

        let mut used_fallback = false;
        self.mark_stage(ReviewStage::PostingReview, full_repo_name, pr.number);
        if analysis.inline_comments.is_empty() {
            if let Err(err) = self
                .with_retry(|| {
                    self.github
                        .create_issue_comment(owner, repo, pr.number, &review_body)
                })
                .await
            {
                return match classify_error(&err) {
                    ErrorClass::Retryable => PrReviewOutcome::FailedRetryable(err.to_string()),
                    ErrorClass::Fatal => PrReviewOutcome::FailedFatal(err.to_string()),
                };
            }
        } else {
            let inline_submit = self
                .with_retry(|| {
                    self.github.submit_inline_review(
                        owner,
                        repo,
                        pr.number,
                        &review_body,
                        &analysis.inline_comments,
                    )
                })
                .await;

            if inline_submit.is_err() {
                used_fallback = true;
                let fallback = build_fallback_summary(
                    &pr.head_sha,
                    selected_engine_fingerprint,
                    &analysis.inline_comments,
                    &summary_with_outdated,
                );
                if let Err(err) = self
                    .with_retry(|| {
                        self.github
                            .create_issue_comment(owner, repo, pr.number, &fallback)
                    })
                    .await
                {
                    return match classify_error(&err) {
                        ErrorClass::Retryable => PrReviewOutcome::FailedRetryable(err.to_string()),
                        ErrorClass::Fatal => PrReviewOutcome::FailedFatal(err.to_string()),
                    };
                }
            }
        }

        let completed_body = format!(
            "{}\nPrismFlow completed review for `{}`.",
            completed_anchor, pr.head_sha
        );

        if let Err(err) = self
            .with_retry(|| {
                self.github
                    .create_issue_comment(owner, repo, pr.number, &completed_body)
            })
            .await
        {
            return match classify_error(&err) {
                ErrorClass::Retryable => PrReviewOutcome::FailedRetryable(err.to_string()),
                ErrorClass::Fatal => PrReviewOutcome::FailedFatal(err.to_string()),
            };
        }
        self.mark_stage(ReviewStage::Labeling, full_repo_name, pr.number);
        if let Err(err) = self
            .sync_reviewed_labels(owner, repo, pr.number, &reviewed_label)
            .await
        {
            return match classify_error(&err) {
                ErrorClass::Retryable => PrReviewOutcome::FailedRetryable(err.to_string()),
                ErrorClass::Fatal => PrReviewOutcome::FailedFatal(err.to_string()),
            };
        }
        self.mark_stage(ReviewStage::Done, full_repo_name, pr.number);

        PrReviewOutcome::Processed {
            sha: pr.head_sha,
            used_fallback,
            recovered_stale: newest_processing_ts(&comments, &processing_prefix).is_some(),
        }
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
        let cwd = self.fs.current_dir()?;
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

        // We use GitService here
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

    async fn with_retry<T, F, Fut>(&self, mut op: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let attempts = self.options.retry_attempts.max(1);
        let backoff = self.options.retry_backoff_ms;
        let mut last_err: Option<anyhow::Error> = None;

        for idx in 0..attempts {
            match op().await {
                Ok(v) => return Ok(v),
                Err(err) => {
                    if matches!(classify_error(&err), ErrorClass::Fatal) {
                        return Err(err);
                    }

                    last_err = Some(err);
                    if idx + 1 < attempts {
                        let wait_ms = retry_backoff_ms(backoff, idx, &last_err);
                        sleep(Duration::from_millis(wait_ms)).await;
                    }
                }
            }
        }

        Err(last_err.unwrap_or_else(|| anyhow!("retry operation failed")))
    }

    async fn sync_reviewed_labels(
        &self,
        owner: &str,
        repo: &str,
        issue_number: u64,
        target_label: &str,
    ) -> Result<()> {
        let labels = self
            .with_retry(|| self.github.list_issue_labels(owner, repo, issue_number))
            .await?;
        let prefix = "pr-reviewer:reviewed:";
        for old in labels
            .iter()
            .filter(|l| l.starts_with(prefix) && l.as_str() != target_label)
        {
            let _ = self
                .with_retry(|| {
                    self.github
                        .remove_issue_label(owner, repo, issue_number, old)
                })
                .await;
        }

        if !labels.iter().any(|l| l == target_label) {
            let label_vec = vec![target_label.to_string()];
            self.with_retry(|| {
                self.github
                    .add_issue_labels(owner, repo, issue_number, &label_vec)
            })
            .await?;
        }
        Ok(())
    }

    async fn sync_large_skip_labels(
        &self,
        owner: &str,
        repo: &str,
        issue_number: u64,
        target_label: &str,
    ) -> Result<()> {
        let labels = self
            .with_retry(|| self.github.list_issue_labels(owner, repo, issue_number))
            .await?;
        let prefix = "pr-reviewer:skipped-large:";
        for old in labels
            .iter()
            .filter(|l| l.starts_with(prefix) && l.as_str() != target_label)
        {
            let _ = self
                .with_retry(|| {
                    self.github
                        .remove_issue_label(owner, repo, issue_number, old)
                })
                .await;
        }

        if !labels.iter().any(|l| l == target_label) {
            let label_vec = vec![target_label.to_string()];
            self.with_retry(|| {
                self.github
                    .add_issue_labels(owner, repo, issue_number, &label_vec)
            })
            .await?;
        }
        Ok(())
    }

    fn emit_status(&self, msg: String) {
        if let Some(tx) = &self.options.status_tx {
            let _ = tx.send(msg);
        }
    }

    fn consume_skip_by_operator(&self) -> bool {
        self.options
            .skip_flag
            .as_ref()
            .map(|f| f.swap(false, Ordering::Relaxed))
            .unwrap_or(false)
    }

    fn mark_stage(&self, stage: ReviewStage, repo: &str, pr_number: u64) {
        let run_id = self
            .options
            .task_context
            .as_ref()
            .map(|ctx| ctx.run_id().to_string())
            .unwrap_or_else(|| "n/a".to_string());
        self.emit_status(format!(
            "run_id={} stage={:?} repo={} pr={}",
            run_id, stage, repo, pr_number
        ));
    }

    fn resolve_effective_prompt(&self, agents: &[String]) -> Result<Option<String>> {
        let mut sections: Vec<String> = Vec::new();

        if let Some(p) = &self.options.engine_prompt {
            sections.push(p.clone());
        }

        let loaded = self.load_agent_prompts(agents, &self.options.agent_prompt_dirs)?;
        if !loaded.is_empty() {
            sections.push(loaded);
        }

        if sections.is_empty() {
            Ok(None)
        } else {
            Ok(Some(sections.join("\n\n")))
        }
    }

    fn load_agent_prompts(&self, agents: &[String], extra_dirs: &[String]) -> Result<String> {
        if agents.is_empty() {
            return Ok(String::new());
        }

        let mut bases: Vec<PathBuf> = extra_dirs
            .iter()
            .map(|d| PathBuf::from(d.trim()))
            .filter(|p| !p.as_os_str().is_empty())
            .collect();
        let cwd = self.fs.current_dir().unwrap_or_else(|_| PathBuf::from("."));
        bases.push(cwd.join(".prismflow").join("prompts"));
        if let Some(config_dir) = self.fs.config_dir() {
            bases.push(config_dir.join("pr-reviewer").join("prompts"));
        }

        let mut sections = Vec::new();
        for agent in agents {
            let file_name = format!("{agent}.md");
            let mut checked: Vec<PathBuf> = Vec::new();
            let mut loaded = None;
            for base in &bases {
                let path = base.join(&file_name);
                checked.push(path.clone());
                if self.fs.exists(&path) {
                    let content = self.fs.read_to_string(&path).map_err(|e| {
                        anyhow!("failed to read agent prompt file {}: {}", path.display(), e)
                    })?;
                    loaded = Some(content);
                    break;
                }
            }
            match loaded {
                Some(content) => sections.push(format!("# Agent: {agent}\n{content}")),
                None => {
                    let checked_str = checked
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(" ; ");
                    return Err(anyhow!(
                        "agent prompt file missing: checked {}",
                        checked_str
                    ));
                }
            }
        }

        Ok(sections.join("\n\n"))
    }

    async fn analyze_review(
        &self,
        owner: &str,
        repo: &str,
        pr_number: u64,
        head_sha: &str,
        files: &[PullRequestFilePatch],
        effective_prompt: Option<&str>,
        selected_engine: &EngineSpec,
        repo_dir: Option<&str>,
        repo_head_ref: &str,
    ) -> Result<ReviewAnalysis> {
        let shell = self
            .shell
            .ok_or_else(|| anyhow!("shell adapter is unavailable"))?;
        let mut command = selected_engine.command.clone();

        let patch = build_patch_dump(owner, repo, pr_number, head_sha, files);
        let patch_file =
            self.write_temp_patch_file(&format!("{owner}/{repo}"), pr_number, &patch)?;
        let agents_payload = effective_prompt.unwrap_or_default();
        let agents_file =
            self.write_temp_agents_file(&format!("{owner}/{repo}"), pr_number, agents_payload)?;
        let files_payload = build_changed_files_list(owner, repo, pr_number, head_sha, files);
        let changed_files_file =
            self.write_temp_files_file(&format!("{owner}/{repo}"), pr_number, &files_payload)?;
        let patch_file_str = patch_file.to_string_lossy().to_string();
        let agents_file_str = agents_file.to_string_lossy().to_string();
        let changed_files_file_str = changed_files_file.to_string_lossy().to_string();
        command = command.replace("{patch_file}", &patch_file_str);
        command = command.replace("{agents_file}", &agents_file_str);
        command = command.replace("{changed_files_file}", &changed_files_file_str);
        command = command.replace("{repo_head_sha}", head_sha);
        command = command.replace("{repo_dir}", repo_dir.unwrap_or(""));
        command = command.replace("{repo_head_ref}", repo_head_ref);
        if let Some(template) = &self.options.prompt_template {
            let rendered_prompt = render_prompt_template(
                template,
                &patch_file_str,
                &agents_file_str,
                &changed_files_file_str,
                repo_dir.unwrap_or(""),
                head_sha,
                repo_head_ref,
            );
            command = command.replace("{prompt_template}", &rendered_prompt);
            command = command.replace("{prompt}", &rendered_prompt);
        }
        let command_line = command;
        let pr_url = format!("https://github.com/{owner}/{repo}/pull/{pr_number}");
        println!(
            "[ENGINE] repo={}/{} pr={} pr_url={} engine={} command_line={}",
            owner, repo, pr_number, pr_url, selected_engine.fingerprint, command_line
        );
        self.emit_status(format!(
            "repo={}/{} pr={} pr_url={} stage=EngineCommand engine={} command_line={}",
            owner, repo, pr_number, pr_url, selected_engine.fingerprint, command_line
        ));
        let output = match shell
            .run_command_line(
                &command_line,
                self.options
                    .task_context
                    .as_deref()
                    .map(|ctx| ctx as &dyn CommandContext),
            )
            .await
        {
            Ok(v) => {
                if !self.options.keep_diff_files {
                    let _ = self.fs.remove_file(&patch_file);
                    let _ = self.fs.remove_file(&agents_file);
                    let _ = self.fs.remove_file(&changed_files_file);
                }
                v
            }
            Err(e) => {
                if !self.options.keep_diff_files {
                    let _ = self.fs.remove_file(&patch_file);
                    let _ = self.fs.remove_file(&agents_file);
                    let _ = self.fs.remove_file(&changed_files_file);
                }
                return Err(e);
            }
        };

        let mut inline_comments = parse_shell_inline_comments(&output);
        let mut summary = if output.trim().is_empty() {
            "Shell engine returned empty output.".to_string()
        } else {
            output
        };

        if inline_comments.is_empty() && likely_unusable_shell_output(&summary) {
            let builtin_comments = analyze_files_for_inline_comments(files);
            inline_comments = builtin_comments;
            summary = "Shell engine returned non-review conversational output; PrismFlow auto-fell back to builtin analyzer.".to_string();
        }

        Ok(ReviewAnalysis {
            source: format!("shell:{}", selected_engine.fingerprint),
            summary,
            inline_comments,
        })
    }

    fn write_temp_patch_file(&self, repo: &str, pr_number: u64, content: &str) -> Result<PathBuf> {
        let cwd = self.fs.current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let root = cwd.join(".prismflow").join("tmp-diffs");
        self.fs.create_dir_all(&root)?;

        let sanitized = repo.replace('/', "_");
        let path = root.join(format!("prismflow_{}_{}_patch.diff", sanitized, pr_number));
        self.fs.write(&path, content.as_bytes())?;
        Ok(path)
    }

    fn write_temp_agents_file(&self, repo: &str, pr_number: u64, content: &str) -> Result<PathBuf> {
        let cwd = self.fs.current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let root = cwd.join(".prismflow").join("tmp-diffs");
        self.fs.create_dir_all(&root)?;

        let sanitized = repo.replace('/', "_");
        let path = root.join(format!("prismflow_{}_{}_agents.txt", sanitized, pr_number));
        self.fs.write(&path, content.as_bytes())?;
        Ok(path)
    }

    fn write_temp_files_file(&self, repo: &str, pr_number: u64, content: &str) -> Result<PathBuf> {
        let cwd = self.fs.current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let root = cwd.join(".prismflow").join("tmp-diffs");
        self.fs.create_dir_all(&root)?;

        let sanitized = repo.replace('/', "_");
        let path = root.join(format!("prismflow_{}_{}_files.txt", sanitized, pr_number));
        self.fs.write(&path, content.as_bytes())?;
        Ok(path)
    }

    fn pick_engine_for_pr(&self) -> Result<EngineSpec> {
        if self.options.engine_specs.is_empty() {
            anyhow::bail!("shell engine selected but no engine command is configured");
        }
        let idx = self.next_engine_idx.fetch_add(1, Ordering::Relaxed);
        let pick = idx % self.options.engine_specs.len();
        Ok(self.options.engine_specs[pick].clone())
    }
}

#[derive(Debug, Clone)]
struct RepoSelector {
    includes: HashSet<String>,
    excludes: HashSet<String>,
}

impl RepoSelector {
    fn from_options(options: &ReviewWorkflowOptions) -> Self {
        Self {
            includes: options
                .include_repos
                .iter()
                .map(|v| normalize_repo_selector(v))
                .filter(|v| !v.is_empty())
                .collect(),
            excludes: options
                .exclude_repos
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

#[derive(Debug, Clone)]
struct AuthorSelector {
    includes: HashSet<String>,
    excludes: HashSet<String>,
}

impl AuthorSelector {
    fn from_options(options: &ReviewWorkflowOptions) -> Self {
        Self {
            includes: options
                .include_authors
                .iter()
                .map(|v| normalize_author_selector(v))
                .filter(|v| !v.is_empty())
                .collect(),
            excludes: options
                .exclude_authors
                .iter()
                .map(|v| normalize_author_selector(v))
                .filter(|v| !v.is_empty())
                .collect(),
        }
    }

    fn matches(&self, author_login: Option<&str>) -> bool {
        let key = normalize_author_selector(author_login.unwrap_or_default());
        if !key.is_empty() && self.excludes.contains(&key) {
            return false;
        }
        if self.includes.is_empty() {
            return true;
        }
        !key.is_empty() && self.includes.contains(&key)
    }
}

fn normalize_repo_selector(input: &str) -> String {
    let trimmed = input.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return String::new();
    }
    if let Some(repo) = parse_github_repo_like_url(trimmed) {
        return repo.to_ascii_lowercase();
    }
    trimmed.to_ascii_lowercase()
}

fn normalize_author_selector(input: &str) -> String {
    input.trim().to_ascii_lowercase()
}

fn parse_github_repo_like_url(input: &str) -> Option<String> {
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
        .filter(|s| !s.trim().is_empty())
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

#[derive(Debug)]
struct RepoReviewReport {
    stats: RepoReviewStats,
    last_processed_sha: Option<String>,
}

#[derive(Debug)]
enum PrReviewOutcome {
    Processed {
        sha: String,
        used_fallback: bool,
        recovered_stale: bool,
    },
    SkippedCompleted,
    SkippedProcessing,
    SkippedFiltered,
    SkippedByOperator,
    FailedRetryable(String),
    FailedFatal(String),
}

#[derive(Debug, Clone, Copy)]
enum ReviewStage {
    Queued,
    Fetching,
    Analyzing,
    PostingReview,
    Labeling,
    Done,
    Skipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ErrorClass {
    Retryable,
    Fatal,
}

fn classify_error(err: &anyhow::Error) -> ErrorClass {
    let msg = err.to_string().to_ascii_lowercase();
    let retryable_hints = [
        "rate limit",
        "too many requests",
        "timeout",
        "timed out",
        "temporar",
        "connection reset",
        "connection refused",
        "503",
        "502",
        "504",
        "429",
    ];

    if retryable_hints.iter().any(|h| msg.contains(h)) {
        ErrorClass::Retryable
    } else {
        ErrorClass::Fatal
    }
}

fn retry_backoff_ms(
    base_backoff_ms: u64,
    attempt_idx: usize,
    last_err: &Option<anyhow::Error>,
) -> u64 {
    let multiplier = (attempt_idx as u64) + 1;
    let mut backoff = base_backoff_ms.saturating_mul(multiplier);

    if let Some(err) = last_err {
        let msg = err.to_string().to_ascii_lowercase();
        if msg.contains("rate limit")
            || msg.contains("x-ratelimit")
            || msg.contains("429")
            || msg.contains("403")
        {
            let reset_secs = parse_rate_limit_reset_secs(&msg).unwrap_or(5);
            backoff = backoff.max(reset_secs.saturating_mul(1000));
        }
    }

    backoff
}

fn parse_rate_limit_reset_secs(msg: &str) -> Option<u64> {
    let marker = "x-ratelimit-reset";
    let idx = msg.find(marker)?;
    let tail = &msg[idx + marker.len()..];
    let mut digits = String::new();
    let mut started = false;
    for c in tail.chars() {
        if c.is_ascii_digit() {
            started = true;
            digits.push(c);
        } else if started {
            break;
        }
    }

    if digits.is_empty() {
        return None;
    }

    let raw = digits.parse::<u64>().ok()?;
    let now = now_unix_secs().max(0) as u64;
    if raw > now {
        Some(raw - now)
    } else {
        Some(raw.min(60))
    }
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

fn dedupe_key(repo: &str, pr: u64, head_sha: &str, engine_fingerprint: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(repo.as_bytes());
    hasher.update(b":");
    hasher.update(pr.to_string().as_bytes());
    hasher.update(b":");
    hasher.update(head_sha.as_bytes());
    hasher.update(b":");
    hasher.update(engine_fingerprint.as_bytes());
    let digest = hasher.finalize();
    hex::encode(digest)
}

fn workflow_engine_fingerprint(engine_specs: &[EngineSpec]) -> String {
    if engine_specs.is_empty() {
        return "default-engine".to_string();
    }
    if engine_specs.len() == 1 {
        return engine_specs[0].fingerprint.clone();
    }
    let mut hasher = Sha256::new();
    for spec in engine_specs {
        hasher.update(spec.fingerprint.as_bytes());
        hasher.update(b";");
    }
    let digest = hasher.finalize();
    let hex = hex::encode(digest);
    format!("multi-{}", &hex[..12])
}

fn reviewed_label_for_sha(head_sha: &str) -> String {
    let short = &head_sha[..head_sha.len().min(12)];
    format!("pr-reviewer:reviewed:{short}")
}

fn large_skipped_label_for_sha(head_sha: &str) -> String {
    let short = &head_sha[..head_sha.len().min(12)];
    format!("pr-reviewer:skipped-large:{short}")
}

fn processing_anchor_prefix(key: &str) -> String {
    format!("<!-- prismflow:processing:{}:ts=", key)
}

fn processing_anchor(key: &str) -> String {
    format!(
        "<!-- prismflow:processing:{}:ts={} -->",
        key,
        now_unix_secs()
    )
}

fn completed_anchor(key: &str) -> String {
    format!("<!-- prismflow:completed:{} -->", key)
}

fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn newest_processing_ts(comments: &[String], prefix: &str) -> Option<i64> {
    comments
        .iter()
        .filter_map(|body| parse_processing_ts(body, prefix))
        .max()
}

fn parse_processing_ts(body: &str, prefix: &str) -> Option<i64> {
    let start = body.find(prefix)? + prefix.len();
    let tail = &body[start..];
    let end = tail.find(" -->")?;
    tail[..end].trim().parse::<i64>().ok()
}

#[derive(Debug, Clone)]
struct ReviewAnalysis {
    source: String,
    summary: String,
    inline_comments: Vec<ReviewComment>,
}

fn build_fallback_summary(
    head_sha: &str,
    engine: &str,
    comments: &[ReviewComment],
    summary: &str,
) -> String {
    let mut out = String::new();
    out.push_str("PrismFlow fallback summary (Inline review unavailable)\n\n");
    out.push_str(&format!("- SHA: `{}`\n", head_sha));
    out.push_str(&format!("- Engine: `{}`\n", engine));
    out.push_str("- Reason: Inline comment publish failed after retries.\n\n");
    out.push_str("Analyzer summary:\n");
    out.push_str(summary);
    out.push_str("\n\n");

    for c in comments.iter().take(10) {
        out.push_str(&format!("- `{}`:{} -> {}\n", c.path, c.line, c.body));
    }

    out
}

fn render_prompt_template(
    template: &str,
    patch_file: &str,
    agents_file: &str,
    changed_files_file: &str,
    repo_dir: &str,
    repo_head_sha: &str,
    repo_head_ref: &str,
) -> String {
    template
        .replace("{patch_file}", patch_file)
        .replace("{agents_file}", agents_file)
        .replace("{changed_files_file}", changed_files_file)
        .replace("{repo_dir}", repo_dir)
        .replace("{repo_head_sha}", repo_head_sha)
        .replace("{repo_head_ref}", repo_head_ref)
}

fn build_patch_dump(
    owner: &str,
    repo: &str,
    pr_number: u64,
    head_sha: &str,
    files: &[PullRequestFilePatch],
) -> String {
    let mut out = String::new();
    out.push_str("# PrismFlow Review Task\n");
    out.push_str("You are reviewing a GitHub PR diff.\n");
    out.push_str("Only use the diff content in this file. Ignore local workspace path/context.\n");
    out.push_str("Do not ask clarification questions. Produce direct review output.\n");
    out.push_str("Output format:\n");
    out.push_str("1) A short summary paragraph.\n");
    out.push_str("2) Zero or more inline findings, each on one line as:\n");
    out.push_str("   path:line: message\n");
    out.push_str("Where `path` must be a file path from the diff and `line` is the new-file line number.\n\n");
    out.push_str(&format!(
        "Repo: {owner}/{repo}\nPR: #{pr_number}\nSHA: {head_sha}\n\n"
    ));

    for f in files {
        out.push_str(&format!("diff --git a/{0} b/{0}\n", f.path));
        if let Some(p) = &f.patch {
            out.push_str(p);
            out.push('\n');
        }
    }
    out
}

fn build_changed_files_list(
    owner: &str,
    repo: &str,
    pr_number: u64,
    head_sha: &str,
    files: &[PullRequestFilePatch],
) -> String {
    let mut out = String::new();
    out.push_str("# PrismFlow Changed Files\n");
    out.push_str(&format!(
        "Repo: {owner}/{repo}\nPR: #{pr_number}\nSHA: {head_sha}\n\n"
    ));
    for f in files {
        out.push_str(&f.path);
        out.push('\n');
    }
    out
}

fn parse_shell_inline_comments(output: &str) -> Vec<ReviewComment> {
    let mut comments = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        let mut parts = line.splitn(3, ':');
        let path = parts.next().unwrap_or_default().trim();
        let line_num = parts.next().unwrap_or_default().trim();
        let body = parts.next().unwrap_or_default().trim();
        if path.is_empty() || line_num.is_empty() || body.is_empty() {
            continue;
        }
        if let Ok(n) = line_num.parse::<u32>() {
            comments.push(ReviewComment {
                path: path.to_string(),
                line: n,
                body: body.to_string(),
            });
        }
    }
    comments
}

fn likely_unusable_shell_output(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    let bad_hints = [
        "could you clarify",
        "different project",
        "doesn't appear to match",
        "do you want me to",
        "i notice two issues",
    ];
    bad_hints.iter().any(|h| lower.contains(h))
}

pub fn ensure_agent_prompts_available(
    fs: &dyn FileSystem,
    agents: &[String],
    extra_dirs: &[String],
) -> Result<()> {
    let _ = load_agent_prompts_shim(fs, agents, extra_dirs)?;
    Ok(())
}

fn load_agent_prompts_shim(
    fs: &dyn FileSystem,
    agents: &[String],
    extra_dirs: &[String],
) -> Result<String> {
    if agents.is_empty() {
        return Ok(String::new());
    }

    let mut bases: Vec<PathBuf> = extra_dirs
        .iter()
        .map(|d| PathBuf::from(d.trim()))
        .filter(|p| !p.as_os_str().is_empty())
        .collect();
    let cwd = fs.current_dir().unwrap_or_else(|_| PathBuf::from("."));
    bases.push(cwd.join(".prismflow").join("prompts"));
    if let Some(config_dir) = fs.config_dir() {
        bases.push(config_dir.join("pr-reviewer").join("prompts"));
    }

    let mut sections = Vec::new();
    for agent in agents {
        let file_name = format!("{agent}.md");
        let mut checked: Vec<PathBuf> = Vec::new();
        let mut loaded = None;
        for base in &bases {
            let path = base.join(&file_name);
            checked.push(path.clone());
            if fs.exists(&path) {
                let content = fs.read_to_string(&path).map_err(|e| {
                    anyhow!("failed to read agent prompt file {}: {}", path.display(), e)
                })?;
                loaded = Some(content);
                break;
            }
        }
        match loaded {
            Some(content) => sections.push(format!("# Agent: {agent}\n{content}")),
            None => {
                let checked_str = checked
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(" ; ");
                return Err(anyhow!(
                    "agent prompt file missing: checked {}",
                    checked_str
                ));
            }
        }
    }

    Ok(sections.join("\n\n"))
}

fn apply_repo_file_filter(
    files: &[PullRequestFilePatch],
    filter: &ReviewFilterConfig,
) -> Vec<PullRequestFilePatch> {
    files
        .iter()
        .filter(|f| should_review_file(&f.path, f.patch.is_some(), filter))
        .cloned()
        .collect()
}

fn should_review_file(path: &str, has_patch: bool, filter: &ReviewFilterConfig) -> bool {
    let p = path.replace('\\', "/");

    if filter.skip_binary_without_patch && !has_patch {
        return false;
    }

    if filter.include_files.iter().any(|v| v == &p) {
        return true;
    }
    if filter.exclude_files.iter().any(|v| v == &p) {
        return false;
    }

    if !filter.include_prefixes.is_empty()
        && !filter.include_prefixes.iter().any(|pre| p.starts_with(pre))
    {
        return false;
    }

    if filter.exclude_prefixes.iter().any(|pre| p.starts_with(pre)) {
        return false;
    }

    let ext = p
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !filter.include_extensions.is_empty()
        && !filter
            .include_extensions
            .iter()
            .any(|e| e.trim_start_matches('.').eq_ignore_ascii_case(&ext))
    {
        return false;
    }
    if filter
        .exclude_extensions
        .iter()
        .any(|e| e.trim_start_matches('.').eq_ignore_ascii_case(&ext))
    {
        return false;
    }

    true
}

fn analyze_files_for_inline_comments(files: &[PullRequestFilePatch]) -> Vec<ReviewComment> {
    let mut out = Vec::new();

    for file in files {
        let Some(patch) = &file.patch else {
            continue;
        };

        let mut new_line: u32 = 0;

        for raw in patch.lines() {
            if let Some(line_start) = parse_hunk_new_start(raw) {
                new_line = line_start;
                continue;
            }

            if raw.starts_with('+') && !raw.starts_with("+++") {
                let content = raw.trim_start_matches('+');
                if let Some(body) = detect_risky_pattern(content) {
                    out.push(ReviewComment {
                        path: file.path.clone(),
                        line: new_line,
                        body,
                    });
                    if out.len() >= MAX_INLINE_COMMENTS {
                        return out;
                    }
                }
                new_line = new_line.saturating_add(1);
                continue;
            }

            if raw.starts_with(' ') {
                new_line = new_line.saturating_add(1);
            }
        }
    }

    out
}

fn parse_hunk_new_start(line: &str) -> Option<u32> {
    if !line.starts_with("@@") {
        return None;
    }

    let plus_pos = line.find(" +")?;
    let tail = &line[(plus_pos + 2)..];
    let mut digits = String::new();
    for ch in tail.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
        } else {
            break;
        }
    }

    digits.parse::<u32>().ok()
}

fn detect_risky_pattern(content: &str) -> Option<String> {
    let lower = content.to_ascii_lowercase();
    if lower.contains("todo") || lower.contains("fixme") {
        return Some(
            "TODO/FIXME found in added code; confirm task is tracked or remove before merge."
                .to_string(),
        );
    }
    if content.contains("unwrap()") {
        return Some(
            "Added `unwrap()` may panic in production paths; prefer explicit error handling."
                .to_string(),
        );
    }
    if content.contains("panic!") {
        return Some(
            "Added `panic!` detected; prefer recoverable error flow unless this is a strict invariant."
                .to_string(),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use crate::domain::errors::DomainError;
    use anyhow::anyhow;
    use async_trait::async_trait;

    use super::*;

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

    #[derive(Default)]
    struct MockGitHub {
        prs: Vec<PullRequestSummary>,
        files: HashMap<u64, Vec<PullRequestFilePatch>>,
        issue_comments: Mutex<HashMap<u64, Vec<String>>>,
        issue_labels: Mutex<HashMap<u64, Vec<String>>>,
        inline_fail: bool,
        list_pr_fail_msg: Option<String>,
        create_comment_failures_before_success: Mutex<usize>,
    }

    #[async_trait]
    impl GitHubRepository for MockGitHub {
        async fn current_user_login(&self) -> Result<String> {
            Ok("mock-user".to_string())
        }

        async fn list_open_pull_requests(
            &self,
            _owner: &str,
            _repo: &str,
        ) -> Result<Vec<PullRequestSummary>> {
            if let Some(msg) = &self.list_pr_fail_msg {
                return Err(anyhow!(msg.clone()));
            }
            Ok(self.prs.clone())
        }

        async fn get_pull_request(
            &self,
            _owner: &str,
            _repo: &str,
            pull_number: u64,
        ) -> Result<PullRequestSummary> {
            self.prs
                .iter()
                .find(|p| p.number == pull_number)
                .cloned()
                .ok_or_else(|| anyhow!("pull request not found"))
        }

        async fn get_pull_request_git_context(
            &self,
            owner: &str,
            repo: &str,
            pull_number: u64,
        ) -> Result<crate::domain::entities::PullRequestGitContext> {
            let pr = self
                .prs
                .iter()
                .find(|p| p.number == pull_number)
                .ok_or_else(|| anyhow!("pull request not found"))?;
            Ok(crate::domain::entities::PullRequestGitContext {
                head_sha: pr.head_sha.clone(),
                head_ref: "mock-ref".to_string(),
                head_clone_url: format!("https://github.com/{owner}/{repo}.git"),
            })
        }

        async fn get_pull_request_metrics(
            &self,
            _owner: &str,
            _repo: &str,
            pull_number: u64,
        ) -> Result<crate::domain::entities::PullRequestMetrics> {
            let files = self.files.get(&pull_number).cloned().unwrap_or_default();
            let mut additions = 0u64;
            let mut deletions = 0u64;
            for f in files {
                if let Some(patch) = f.patch {
                    for line in patch.lines() {
                        if line.starts_with('+') && !line.starts_with("+++") {
                            additions += 1;
                        } else if line.starts_with('-') && !line.starts_with("---") {
                            deletions += 1;
                        }
                    }
                }
            }
            Ok(crate::domain::entities::PullRequestMetrics {
                changed_files: self
                    .files
                    .get(&pull_number)
                    .map(|v| v.len() as u64)
                    .unwrap_or(0),
                additions,
                deletions,
            })
        }

        async fn get_pull_request_ci_snapshot(
            &self,
            _owner: &str,
            _repo: &str,
            pull_number: u64,
        ) -> Result<crate::domain::entities::PullRequestCiSnapshot> {
            let sha = self
                .prs
                .iter()
                .find(|p| p.number == pull_number)
                .map(|p| p.head_sha.clone())
                .unwrap_or_else(|| "unknown".to_string());
            Ok(crate::domain::entities::PullRequestCiSnapshot {
                head_sha: sha,
                failures: vec![],
            })
        }

        async fn list_pull_request_files(
            &self,
            _owner: &str,
            _repo: &str,
            pull_number: u64,
        ) -> Result<Vec<PullRequestFilePatch>> {
            Ok(self.files.get(&pull_number).cloned().unwrap_or_default())
        }

        async fn list_issue_comment_bodies(
            &self,
            _owner: &str,
            _repo: &str,
            issue_number: u64,
        ) -> Result<Vec<String>> {
            Ok(self
                .issue_comments
                .lock()
                .expect("lock")
                .get(&issue_number)
                .cloned()
                .unwrap_or_default())
        }

        async fn list_issue_comments(
            &self,
            _owner: &str,
            _repo: &str,
            issue_number: u64,
        ) -> Result<Vec<crate::domain::entities::SimpleComment>> {
            let items = self
                .issue_comments
                .lock()
                .expect("lock")
                .get(&issue_number)
                .cloned()
                .unwrap_or_default();
            Ok(items
                .into_iter()
                .enumerate()
                .map(|(idx, body)| crate::domain::entities::SimpleComment {
                    id: (idx + 1) as u64,
                    body,
                    author_login: Some("mock-user".to_string()),
                })
                .collect())
        }

        async fn create_issue_comment(
            &self,
            _owner: &str,
            _repo: &str,
            issue_number: u64,
            body: &str,
        ) -> Result<()> {
            let mut failures = self
                .create_comment_failures_before_success
                .lock()
                .expect("lock");
            if *failures > 0 {
                *failures -= 1;
                return Err(anyhow!("temporary timeout"));
            }

            let mut map = self.issue_comments.lock().expect("lock");
            map.entry(issue_number).or_default().push(body.to_string());
            Ok(())
        }

        async fn submit_inline_review(
            &self,
            _owner: &str,
            _repo: &str,
            _pull_number: u64,
            _body: &str,
            _comments: &[ReviewComment],
        ) -> Result<()> {
            if self.inline_fail {
                Err(anyhow!("inline failed"))
            } else {
                Ok(())
            }
        }

        async fn list_issue_labels(
            &self,
            _owner: &str,
            _repo: &str,
            issue_number: u64,
        ) -> Result<Vec<String>> {
            Ok(self
                .issue_labels
                .lock()
                .expect("lock")
                .get(&issue_number)
                .cloned()
                .unwrap_or_default())
        }

        async fn add_issue_labels(
            &self,
            _owner: &str,
            _repo: &str,
            issue_number: u64,
            labels: &[String],
        ) -> Result<()> {
            let mut map = self.issue_labels.lock().expect("lock");
            let entry = map.entry(issue_number).or_default();
            for l in labels {
                if !entry.iter().any(|v| v == l) {
                    entry.push(l.clone());
                }
            }
            Ok(())
        }

        async fn remove_issue_label(
            &self,
            _owner: &str,
            _repo: &str,
            issue_number: u64,
            label: &str,
        ) -> Result<()> {
            let mut map = self.issue_labels.lock().expect("lock");
            if let Some(entry) = map.get_mut(&issue_number) {
                entry.retain(|v| v != label);
            }
            Ok(())
        }

        async fn list_pull_review_comments(
            &self,
            _owner: &str,
            _repo: &str,
            _pull_number: u64,
        ) -> Result<Vec<crate::domain::entities::SimpleComment>> {
            Ok(vec![])
        }

        async fn list_pull_reviews(
            &self,
            _owner: &str,
            _repo: &str,
            _pull_number: u64,
        ) -> Result<Vec<crate::domain::entities::SimplePullReview>> {
            Ok(vec![])
        }

        async fn delete_issue_comment(
            &self,
            _owner: &str,
            _repo: &str,
            _comment_id: u64,
        ) -> Result<()> {
            Ok(())
        }

        async fn delete_pull_review_comment(
            &self,
            _owner: &str,
            _repo: &str,
            _comment_id: u64,
        ) -> Result<()> {
            Ok(())
        }

        async fn delete_pending_pull_review(
            &self,
            _owner: &str,
            _repo: &str,
            _pull_number: u64,
            _review_id: u64,
        ) -> Result<()> {
            Ok(())
        }

        async fn dismiss_pull_review(
            &self,
            _owner: &str,
            _repo: &str,
            _pull_number: u64,
            _review_id: u64,
            _message: &str,
        ) -> Result<()> {
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

    fn config_with_repos(names: &[&str]) -> AppConfig {
        AppConfig {
            agent_prompt_dirs: vec![],
            repos: names
                .iter()
                .map(|name| MonitoredRepo {
                    full_name: (*name).to_string(),
                    added_at: "2026-01-01T00:00:00Z".to_string(),
                    last_sha: None,
                    review_filter: ReviewFilterConfig::default(),
                    agents: vec![],
                })
                .collect(),
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
            Ok("src/main.rs:1: mock finding".to_string())
        }

        async fn run_command_line_in_dir(
            &self,
            _command_line: &str,
            _workdir: Option<&str>,
            _ctx: Option<&dyn CommandContext>,
        ) -> Result<String> {
            Ok("src/main.rs:1: mock finding".to_string())
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

    static MOCK_SHELL: MockShell = MockShell;
    static MOCK_FS: MockFileSystem = MockFileSystem;
    static MOCK_GIT: MockGit = MockGit;

    fn normalize_test_opts(mut opts: ReviewWorkflowOptions) -> ReviewWorkflowOptions {
        if opts.engine_specs.is_empty() {
            opts.engine_specs = vec![EngineSpec {
                fingerprint: "engine".to_string(),
                command: "mock {patch_file}".to_string(),
            }];
        }
        opts
    }

    fn workflow<'a>(
        cfg: &'a InMemoryConfigRepo,
        gh: &'a MockGitHub,
        opts: ReviewWorkflowOptions,
    ) -> ReviewWorkflow<'a> {
        ReviewWorkflow::new(
            cfg,
            gh,
            Some(&MOCK_SHELL),
            &MOCK_FS,
            &MOCK_GIT,
            normalize_test_opts(opts),
        )
    }

    fn workflow_with_shell<'a>(
        cfg: &'a InMemoryConfigRepo,
        gh: &'a MockGitHub,
        shell: &'a dyn ShellAdapter,
        opts: ReviewWorkflowOptions,
    ) -> ReviewWorkflow<'a> {
        ReviewWorkflow::new(
            cfg,
            gh,
            Some(shell),
            &MOCK_FS,
            &MOCK_GIT,
            normalize_test_opts(opts),
        )
    }

    struct CancelAwareShell;

    #[async_trait::async_trait]
    impl ShellAdapter for CancelAwareShell {
        fn run_capture(&self, _program: &str, _args: &[&str]) -> Result<String> {
            Ok(String::new())
        }

        async fn run_command_line(
            &self,
            _command_line: &str,
            ctx: Option<&dyn CommandContext>,
        ) -> Result<String> {
            if let Some(ctx) = ctx {
                ctx.cancelled().await;
                return Err(anyhow!(DomainError::CancelledBySignal));
            }
            tokio::time::sleep(Duration::from_secs(8)).await;
            Ok(String::new())
        }

        async fn run_command_line_in_dir(
            &self,
            _command_line: &str,
            _workdir: Option<&str>,
            ctx: Option<&dyn CommandContext>,
        ) -> Result<String> {
            self.run_command_line("", ctx).await
        }
    }

    #[tokio::test]
    async fn prepare_repo_checkout_falls_back_to_ref_fetch_when_sha_fetch_fails() {
        let github = MockGitHub::default();
        let config = InMemoryConfigRepo::new(config_with_repo());
        let git = RecordingGit::default();
        let workflow = ReviewWorkflow::new(
            &config,
            &github,
            Some(&MOCK_SHELL),
            &MOCK_FS,
            &git,
            normalize_test_opts(ReviewWorkflowOptions {
                clone_workspace_dir: ".prismflow/test-review-cache".to_string(),
                clone_depth: 1,
                ..ReviewWorkflowOptions::default()
            }),
        );

        let repo_dir = workflow
            .prepare_repo_checkout(
                "https://github.com/owner/repo.git",
                "abc123",
                "main",
                "owner",
                "repo",
                7,
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
        assert!(repo_dir.to_string_lossy().contains("owner_repo_pr7_abc123"));
    }

    #[test]
    fn pick_engine_respects_start_index() {
        let github = MockGitHub::default();
        let config = InMemoryConfigRepo::new(config_with_repo());
        let workflow = workflow(
            &config,
            &github,
            ReviewWorkflowOptions {
                engine_specs: vec![
                    EngineSpec {
                        fingerprint: "engine-a".to_string(),
                        command: "a".to_string(),
                    },
                    EngineSpec {
                        fingerprint: "engine-b".to_string(),
                        command: "b".to_string(),
                    },
                ],
                engine_start_index: 1,
                ..ReviewWorkflowOptions::default()
            },
        );

        let first = workflow.pick_engine_for_pr().expect("pick first");
        let second = workflow.pick_engine_for_pr().expect("pick second");
        assert_eq!(first.fingerprint, "engine-b");
        assert_eq!(second.fingerprint, "engine-a");
    }

    #[test]
    fn render_prompt_template_replaces_supported_placeholders() {
        let rendered = render_prompt_template(
            "p={patch_file};a={agents_file};f={changed_files_file};d={repo_dir};s={repo_head_sha};r={repo_head_ref}",
            "P",
            "A",
            "F",
            "D",
            "S",
            "R",
        );
        assert_eq!(rendered, "p=P;a=A;f=F;d=D;s=S;r=R");
    }

    #[tokio::test]
    async fn skips_when_completed_anchor_exists() {
        let sha = "abc123";
        let key = dedupe_key("owner/repo", 1, sha, "engine");
        let completed = completed_anchor(&key);

        let github = MockGitHub {
            prs: vec![PullRequestSummary {
                number: 1,
                title: "t".to_string(),
                head_sha: sha.to_string(),
                html_url: None,
                author_login: Some("mock-user".to_string()),
            }],
            issue_comments: Mutex::new(HashMap::from([(1, vec![completed])])),
            ..Default::default()
        };

        let config = InMemoryConfigRepo::new(config_with_repo());
        let stats = workflow(&config, &github, ReviewWorkflowOptions::default())
            .review_once()
            .await
            .expect("review_once");

        assert_eq!(stats[0].processed, 0);
        assert_eq!(stats[0].skipped_completed, 1);
    }

    #[tokio::test]
    async fn recovers_stale_processing_anchor_and_processes() {
        let sha = "abc123";
        let key = dedupe_key("owner/repo", 1, sha, "engine");
        let stale_anchor = format!(
            "<!-- prismflow:processing:{}:ts={} -->",
            key,
            now_unix_secs() - PROCESSING_TTL_SECS - 10
        );

        let github = MockGitHub {
            prs: vec![PullRequestSummary {
                number: 1,
                title: "t".to_string(),
                head_sha: sha.to_string(),
                html_url: None,
                author_login: Some("mock-user".to_string()),
            }],
            files: HashMap::from([(
                1,
                vec![PullRequestFilePatch {
                    path: "src/lib.rs".to_string(),
                    patch: Some("@@ -1,1 +1,2 @@\n line\n+let x = y.unwrap();".to_string()),
                }],
            )]),
            issue_comments: Mutex::new(HashMap::from([(1, vec![stale_anchor])])),
            ..Default::default()
        };

        let config = InMemoryConfigRepo::new(config_with_repo());
        let stats = workflow(&config, &github, ReviewWorkflowOptions::default())
            .review_once()
            .await
            .expect("review_once");

        assert_eq!(stats[0].processed, 1);
    }

    #[tokio::test]
    async fn falls_back_to_general_comment_when_inline_fails() {
        let sha = "abc123";
        let github = MockGitHub {
            prs: vec![PullRequestSummary {
                number: 1,
                title: "t".to_string(),
                head_sha: sha.to_string(),
                html_url: None,
                author_login: Some("mock-user".to_string()),
            }],
            files: HashMap::from([(
                1,
                vec![PullRequestFilePatch {
                    path: "src/lib.rs".to_string(),
                    patch: Some("@@ -1,1 +1,2 @@\n line\n+// TODO: improve".to_string()),
                }],
            )]),
            inline_fail: true,
            ..Default::default()
        };

        let config = InMemoryConfigRepo::new(config_with_repo());
        let stats = workflow(&config, &github, ReviewWorkflowOptions::default())
            .review_once()
            .await
            .expect("review_once");

        assert_eq!(stats[0].fallback_general, 1);
        assert_eq!(stats[0].processed, 1);
    }

    #[tokio::test]
    async fn classifies_repo_failure_retryable() {
        let github = MockGitHub {
            list_pr_fail_msg: Some("rate limit exceeded".to_string()),
            ..Default::default()
        };
        let config = InMemoryConfigRepo::new(config_with_repo());

        let stats = workflow(&config, &github, ReviewWorkflowOptions::default())
            .review_once()
            .await
            .expect("review_once");

        assert_eq!(stats[0].failed_retryable, 1);
        assert_eq!(stats[0].failed_fatal, 0);
    }

    #[tokio::test]
    async fn retries_temporary_write_failures() {
        let github = MockGitHub {
            prs: vec![PullRequestSummary {
                number: 1,
                title: "t".to_string(),
                head_sha: "abc123".to_string(),
                html_url: None,
                author_login: Some("mock-user".to_string()),
            }],
            files: HashMap::from([(
                1,
                vec![PullRequestFilePatch {
                    path: "src/main.rs".to_string(),
                    patch: Some("@@ -1,1 +1,2 @@\n line\n+// TODO: improve".to_string()),
                }],
            )]),
            create_comment_failures_before_success: Mutex::new(2),
            ..Default::default()
        };
        let config = InMemoryConfigRepo::new(config_with_repo());
        let opts = ReviewWorkflowOptions {
            retry_attempts: 3,
            retry_backoff_ms: 1,
            ..ReviewWorkflowOptions::default()
        };

        let stats = workflow(&config, &github, opts)
            .review_once()
            .await
            .expect("review_once");
        assert_eq!(stats[0].processed, 1);
    }

    #[tokio::test]
    async fn skips_when_reviewed_label_exists() {
        let sha = "5f56bffd7e24cc9f072cf00962f19cbb43dba4c1";
        let label = reviewed_label_for_sha(sha);
        let github = MockGitHub {
            prs: vec![PullRequestSummary {
                number: 7,
                title: "t".to_string(),
                head_sha: sha.to_string(),
                html_url: None,
                author_login: Some("mock-user".to_string()),
            }],
            issue_labels: Mutex::new(HashMap::from([(7, vec![label])])),
            ..Default::default()
        };
        let config = InMemoryConfigRepo::new(config_with_repo());

        let stats = workflow(&config, &github, ReviewWorkflowOptions::default())
            .review_once()
            .await
            .expect("review_once");
        assert_eq!(stats[0].skipped_completed, 1);
        assert_eq!(stats[0].processed, 0);
    }

    #[tokio::test]
    async fn applies_repo_filter_rules() {
        let mut cfg = config_with_repo();
        cfg.repos[0].review_filter.exclude_prefixes = vec!["src/generated/".to_string()];

        let github = MockGitHub {
            prs: vec![PullRequestSummary {
                number: 3,
                title: "t".to_string(),
                head_sha: "abc123".to_string(),
                html_url: None,
                author_login: Some("mock-user".to_string()),
            }],
            files: HashMap::from([(
                3,
                vec![PullRequestFilePatch {
                    path: "src/generated/file.ts".to_string(),
                    patch: Some("@@ -1,1 +1,2 @@\n line\n+// TODO: improve".to_string()),
                }],
            )]),
            ..Default::default()
        };
        let config = InMemoryConfigRepo::new(cfg);

        let stats = workflow(&config, &github, ReviewWorkflowOptions::default())
            .review_once()
            .await
            .expect("review_once");
        assert_eq!(stats[0].skipped_filtered, 1);
    }

    #[tokio::test]
    async fn adhoc_respects_cooldown_for_same_sha() {
        let sha = "abc123";
        let key = dedupe_key("owner/repo", 1, sha, "engine");
        let recent_processing = processing_anchor(&key);

        let github = MockGitHub {
            prs: vec![PullRequestSummary {
                number: 1,
                title: "t".to_string(),
                head_sha: sha.to_string(),
                html_url: None,
                author_login: Some("mock-user".to_string()),
            }],
            files: HashMap::from([(
                1,
                vec![PullRequestFilePatch {
                    path: "src/lib.rs".to_string(),
                    patch: Some("@@ -1,1 +1,2 @@\n line\n+let x = y.unwrap();".to_string()),
                }],
            )]),
            issue_comments: Mutex::new(HashMap::from([(1, vec![recent_processing])])),
            ..Default::default()
        };
        let config = InMemoryConfigRepo::new(config_with_repo());
        let stats = workflow(&config, &github, ReviewWorkflowOptions::default())
            .review_ad_hoc("owner", "repo", 1)
            .await
            .expect("review_ad_hoc");
        assert_eq!(stats.skipped_processing, 1);
        assert_eq!(stats.processed, 0);
    }

    #[tokio::test]
    async fn adds_outdated_notice_when_sha_label_changes() {
        let old_sha = "111111111111";
        let github = MockGitHub {
            prs: vec![PullRequestSummary {
                number: 1,
                title: "t".to_string(),
                head_sha: "abc123".to_string(),
                html_url: None,
                author_login: Some("mock-user".to_string()),
            }],
            files: HashMap::from([(
                1,
                vec![PullRequestFilePatch {
                    path: "src/lib.rs".to_string(),
                    patch: Some("@@ -1,1 +1,2 @@\n line\n+let x = y.unwrap();".to_string()),
                }],
            )]),
            issue_labels: Mutex::new(HashMap::from([(
                1,
                vec![format!("pr-reviewer:reviewed:{old_sha}")],
            )])),
            inline_fail: true,
            ..Default::default()
        };
        let config = InMemoryConfigRepo::new(config_with_repo());
        let stats = workflow(&config, &github, ReviewWorkflowOptions::default())
            .review_once()
            .await
            .expect("review_once");
        assert_eq!(stats[0].processed, 1);

        let comments = github
            .issue_comments
            .lock()
            .expect("lock")
            .get(&1)
            .cloned()
            .unwrap_or_default();
        let merged = comments.join("\n\n");
        assert!(merged.contains("Previous review for `111111111111` is outdated."));
    }

    #[tokio::test]
    async fn review_once_filters_with_include_repos() {
        let github = MockGitHub {
            prs: vec![PullRequestSummary {
                number: 1,
                title: "t".to_string(),
                head_sha: "abc123".to_string(),
                html_url: None,
                author_login: Some("mock-user".to_string()),
            }],
            files: HashMap::from([(
                1,
                vec![PullRequestFilePatch {
                    path: "src/lib.rs".to_string(),
                    patch: Some("@@ -1,1 +1,2 @@\n line\n+let x = 1;".to_string()),
                }],
            )]),
            ..Default::default()
        };
        let config = InMemoryConfigRepo::new(config_with_repos(&["owner/repo-a", "owner/repo-b"]));
        let opts = ReviewWorkflowOptions {
            include_repos: vec!["owner/repo-b".to_string()],
            ..ReviewWorkflowOptions::default()
        };

        let stats = workflow(&config, &github, opts)
            .review_once()
            .await
            .expect("review_once");
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].repo, "owner/repo-b");
    }

    #[tokio::test]
    async fn review_once_filters_with_exclude_repos() {
        let github = MockGitHub {
            prs: vec![PullRequestSummary {
                number: 1,
                title: "t".to_string(),
                head_sha: "abc123".to_string(),
                html_url: None,
                author_login: Some("mock-user".to_string()),
            }],
            files: HashMap::from([(
                1,
                vec![PullRequestFilePatch {
                    path: "src/lib.rs".to_string(),
                    patch: Some("@@ -1,1 +1,2 @@\n line\n+let x = 1;".to_string()),
                }],
            )]),
            ..Default::default()
        };
        let config = InMemoryConfigRepo::new(config_with_repos(&["owner/repo-a", "owner/repo-b"]));
        let opts = ReviewWorkflowOptions {
            exclude_repos: vec!["owner/repo-a".to_string()],
            ..ReviewWorkflowOptions::default()
        };

        let stats = workflow(&config, &github, opts)
            .review_once()
            .await
            .expect("review_once");
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].repo, "owner/repo-b");
    }

    #[tokio::test]
    async fn review_once_filters_with_include_authors() {
        let github = MockGitHub {
            prs: vec![
                PullRequestSummary {
                    number: 1,
                    title: "t1".to_string(),
                    head_sha: "abc123".to_string(),
                    html_url: None,
                    author_login: Some("alice".to_string()),
                },
                PullRequestSummary {
                    number: 2,
                    title: "t2".to_string(),
                    head_sha: "def456".to_string(),
                    html_url: None,
                    author_login: Some("bob".to_string()),
                },
            ],
            files: HashMap::from([
                (
                    1,
                    vec![PullRequestFilePatch {
                        path: "src/lib.rs".to_string(),
                        patch: Some("@@ -1,1 +1,2 @@\n line\n+let x = 1;".to_string()),
                    }],
                ),
                (
                    2,
                    vec![PullRequestFilePatch {
                        path: "src/lib.rs".to_string(),
                        patch: Some("@@ -1,1 +1,2 @@\n line\n+let y = 2;".to_string()),
                    }],
                ),
            ]),
            ..Default::default()
        };
        let config = InMemoryConfigRepo::new(config_with_repo());
        let opts = ReviewWorkflowOptions {
            include_authors: vec!["alice".to_string()],
            ..ReviewWorkflowOptions::default()
        };

        let stats = workflow(&config, &github, opts)
            .review_once()
            .await
            .expect("review_once");
        assert_eq!(stats[0].processed, 1);
        assert_eq!(stats[0].skipped_by_author, 1);
    }

    #[tokio::test]
    async fn review_once_filters_with_exclude_authors() {
        let github = MockGitHub {
            prs: vec![
                PullRequestSummary {
                    number: 1,
                    title: "t1".to_string(),
                    head_sha: "abc123".to_string(),
                    html_url: None,
                    author_login: Some("alice".to_string()),
                },
                PullRequestSummary {
                    number: 2,
                    title: "t2".to_string(),
                    head_sha: "def456".to_string(),
                    html_url: None,
                    author_login: Some("bob".to_string()),
                },
            ],
            files: HashMap::from([
                (
                    1,
                    vec![PullRequestFilePatch {
                        path: "src/lib.rs".to_string(),
                        patch: Some("@@ -1,1 +1,2 @@\n line\n+let x = 1;".to_string()),
                    }],
                ),
                (
                    2,
                    vec![PullRequestFilePatch {
                        path: "src/lib.rs".to_string(),
                        patch: Some("@@ -1,1 +1,2 @@\n line\n+let y = 2;".to_string()),
                    }],
                ),
            ]),
            ..Default::default()
        };
        let config = InMemoryConfigRepo::new(config_with_repo());
        let opts = ReviewWorkflowOptions {
            exclude_authors: vec!["bob".to_string()],
            ..ReviewWorkflowOptions::default()
        };

        let stats = workflow(&config, &github, opts)
            .review_once()
            .await
            .expect("review_once");
        assert_eq!(stats[0].processed, 1);
        assert_eq!(stats[0].skipped_by_author, 1);
    }

    #[tokio::test]
    async fn review_once_author_exclude_has_higher_priority_than_include() {
        let github = MockGitHub {
            prs: vec![PullRequestSummary {
                number: 1,
                title: "t1".to_string(),
                head_sha: "abc123".to_string(),
                html_url: None,
                author_login: Some("alice".to_string()),
            }],
            files: HashMap::from([(
                1,
                vec![PullRequestFilePatch {
                    path: "src/lib.rs".to_string(),
                    patch: Some("@@ -1,1 +1,2 @@\n line\n+let x = 1;".to_string()),
                }],
            )]),
            ..Default::default()
        };
        let config = InMemoryConfigRepo::new(config_with_repo());
        let opts = ReviewWorkflowOptions {
            include_authors: vec!["alice".to_string()],
            exclude_authors: vec!["alice".to_string()],
            ..ReviewWorkflowOptions::default()
        };

        let stats = workflow(&config, &github, opts)
            .review_once()
            .await
            .expect("review_once");
        assert_eq!(stats[0].processed, 0);
        assert_eq!(stats[0].skipped_by_author, 1);
    }

    #[tokio::test]
    async fn review_once_respects_cancelled_task_context() {
        let github = MockGitHub {
            prs: vec![PullRequestSummary {
                number: 1,
                title: "t".to_string(),
                head_sha: "abc123".to_string(),
                html_url: None,
                author_login: Some("mock-user".to_string()),
            }],
            ..Default::default()
        };
        let config = InMemoryConfigRepo::new(config_with_repo());
        let ctx = Arc::new(TaskContext::new("cancelled-review-once"));
        ctx.cancel();
        let opts = ReviewWorkflowOptions {
            task_context: Some(ctx),
            ..ReviewWorkflowOptions::default()
        };

        let stats = workflow(&config, &github, opts)
            .review_once()
            .await
            .expect("review_once");
        assert_eq!(stats[0].processed, 0);
        assert_eq!(stats[0].failed_retryable, 1);
    }

    #[tokio::test]
    async fn engine_stage_cancel_does_not_write_completed_comment() {
        let github = MockGitHub {
            prs: vec![PullRequestSummary {
                number: 1,
                title: "t".to_string(),
                head_sha: "abc123".to_string(),
                html_url: None,
                author_login: Some("mock-user".to_string()),
            }],
            files: HashMap::from([(
                1,
                vec![PullRequestFilePatch {
                    path: "src/lib.rs".to_string(),
                    patch: Some("@@ -1,1 +1,2 @@\n line\n+let x = 1;".to_string()),
                }],
            )]),
            ..Default::default()
        };
        let config = InMemoryConfigRepo::new(config_with_repo());
        let ctx = Arc::new(TaskContext::new("engine-cancel-test"));
        let opts = ReviewWorkflowOptions {
            task_context: Some(ctx.clone()),
            ..ReviewWorkflowOptions::default()
        };
        let shell = CancelAwareShell;

        let ((), stats) = tokio::join!(
            async {
                tokio::time::sleep(Duration::from_millis(250)).await;
                ctx.cancel();
            },
            async {
                workflow_with_shell(&config, &github, &shell, opts)
                    .review_once()
                    .await
                    .expect("review_once")
            }
        );

        assert_eq!(stats[0].processed, 0);
        assert_eq!(stats[0].failed_fatal + stats[0].failed_retryable, 1);

        let comments = github
            .issue_comments
            .lock()
            .expect("lock")
            .get(&1)
            .cloned()
            .unwrap_or_default();
        let merged = comments.join("\n");
        assert!(merged.contains("prismflow:processing:"));
        assert!(!merged.contains("prismflow:completed:"));
    }
}
