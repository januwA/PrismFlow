use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use tokio::sync::broadcast;

use crate::application::{
    agent_preflight::{panic_if_cli_agents_missing, panic_if_required_agents_missing},
    auth_manager::AuthManager,
    ci_workflow::{CiWorkflow, CiWorkflowOptions},
    context::TaskContext,
    review_workflow::{RepoReviewStats, ReviewWorkflow, ReviewWorkflowOptions},
};
use crate::domain::ports::{ConfigRepository, GitHubRepository};
use crate::domain::errors::DomainError;
use crate::infrastructure::{
    github_adapter::OctocrabGitHubRepository, local_config_adapter::LocalConfigAdapter,
    shell_adapter::CommandShellAdapter,
};

pub async fn run_review_once(
    auth_manager: &AuthManager<'_>,
    config_repo: &LocalConfigAdapter,
    shell: &CommandShellAdapter,
    max_concurrent_api: usize,
    mut options: ReviewWorkflowOptions,
    ctx: Arc<TaskContext>,
    status_tx: Option<&broadcast::Sender<String>>,
) -> Result<()> {
    if ctx.is_cancelled() {
        return Err(anyhow!(DomainError::CancelledBySignal));
    }
    let config = config_repo.load_config()?;
    if config.repos.is_empty() {
        println!("no repositories configured");
        return Ok(());
    }
    panic_if_required_agents_missing(&config, &options, "review-once");
    options.task_context = Some(ctx.clone());

    let github = github_client_for_action(auth_manager, max_concurrent_api, "review")?;
    let workflow = ReviewWorkflow::new(config_repo, &github, Some(shell), options);
    let stats = run_with_heartbeat("review-once", workflow.review_once(), &ctx).await?;

    if stats.is_empty() {
        println!("no repositories configured");
    } else {
        let report_path = write_review_report("review-once", &stats)?;
        println!("report_file={}", report_path.display());
        for item in stats {
            if let Some(tx) = status_tx {
                let _ = tx.send(format!(
                    "run_id={} repo={} processed={} skip_completed={} skip_processing={} skip_filtered={} skip_by_operator={} retryable_fail={} fatal_fail={} retryable_error={:?} fatal_error={:?}",
                    ctx.run_id(),
                    item.repo,
                    item.processed,
                    item.skipped_completed,
                    item.skipped_processing,
                    item.skipped_filtered,
                    item.skipped_by_operator,
                    item.failed_retryable,
                    item.failed_fatal,
                    item.last_retryable_error,
                    item.last_fatal_error
                ));
            }
            println!(
                "repo={} processed={} skipped_completed={} skipped_processing={} skipped_filtered={} skipped_by_operator={} recovered_stale_processing={} fallback_general={} failed_retryable={} failed_fatal={} last_retryable_error={:?} last_fatal_error={:?}",
                item.repo,
                item.processed,
                item.skipped_completed,
                item.skipped_processing,
                item.skipped_filtered,
                item.skipped_by_operator,
                item.recovered_stale_processing,
                item.fallback_general,
                item.failed_retryable,
                item.failed_fatal,
                item.last_retryable_error,
                item.last_fatal_error
            );
        }
    }

    Ok(())
}

pub async fn run_ci_once(
    auth_manager: &AuthManager<'_>,
    config_repo: &LocalConfigAdapter,
    shell: &CommandShellAdapter,
    max_concurrent_api: usize,
    mut options: CiWorkflowOptions,
    ctx: Arc<TaskContext>,
) -> Result<()> {
    if ctx.is_cancelled() {
        return Err(anyhow!(DomainError::CancelledBySignal));
    }
    if config_repo.load_config()?.repos.is_empty() {
        println!("no repositories configured");
        return Ok(());
    }
    options.task_context = Some(ctx.clone());

    let github = github_client_for_action(auth_manager, max_concurrent_api, "ci")?;
    let workflow = CiWorkflow::new(config_repo, &github, shell, options);
    let stats = run_with_heartbeat("ci-once", workflow.run_once(), &ctx).await?;
    if stats.is_empty() {
        println!("no repositories matched current filters");
    } else {
        for item in stats {
            println!(
                "repo={} analyzed={} skipped_no_failures={} skipped_completed={} failed={}",
                item.repo,
                item.analyzed,
                item.skipped_no_failures,
                item.skipped_completed,
                item.failed
            );
        }
    }
    Ok(())
}

pub async fn run_review_ad_hoc(
    auth_manager: &AuthManager<'_>,
    config_repo: &LocalConfigAdapter,
    shell: &CommandShellAdapter,
    owner: &str,
    repo: &str,
    pr_number: u64,
    max_concurrent_api: usize,
    mut options: ReviewWorkflowOptions,
    ctx: Arc<TaskContext>,
) -> Result<()> {
    if ctx.is_cancelled() {
        return Err(anyhow!(DomainError::CancelledBySignal));
    }
    panic_if_cli_agents_missing(&options, "review-ad-hoc");
    options.task_context = Some(ctx.clone());
    let github = github_client_for_action(auth_manager, max_concurrent_api, "ad-hoc review")?;
    let workflow = ReviewWorkflow::new(config_repo, &github, Some(shell), options);
    let stats = run_with_heartbeat(
        "review-ad-hoc",
        workflow.review_ad_hoc(owner, repo, pr_number),
        &ctx,
    )
    .await?;

    let report_path = write_review_report("review-ad-hoc", std::slice::from_ref(&stats))?;
    println!("report_file={}", report_path.display());

    println!(
        "repo={} processed={} skipped_completed={} skipped_processing={} skipped_filtered={} skipped_by_operator={} recovered_stale_processing={} fallback_general={} failed_retryable={} failed_fatal={} last_retryable_error={:?} last_fatal_error={:?}",
        stats.repo,
        stats.processed,
        stats.skipped_completed,
        stats.skipped_processing,
        stats.skipped_filtered,
        stats.skipped_by_operator,
        stats.recovered_stale_processing,
        stats.fallback_general,
        stats.failed_retryable,
        stats.failed_fatal,
        stats.last_retryable_error,
        stats.last_fatal_error
    );
    Ok(())
}

pub async fn run_review_clean(
    auth_manager: &AuthManager<'_>,
    owner: &str,
    repo: &str,
    pr_number: u64,
    max_concurrent_api: usize,
    ctx: Arc<TaskContext>,
) -> Result<()> {
    if ctx.is_cancelled() {
        return Err(anyhow!(DomainError::CancelledBySignal));
    }
    let github = github_client_for_action(auth_manager, max_concurrent_api, "clean")?;
    let me = github.current_user_login().await.ok();
    let issue_comments = github.list_issue_comments(owner, repo, pr_number).await?;
    let mut removed_issue_comments = 0usize;
    for c in issue_comments {
        if is_prismflow_trace_comment(&c.body)
            || me
                .as_ref()
                .map(|m| c.author_login.as_deref() == Some(m.as_str()))
                .unwrap_or(false)
        {
            if github.delete_issue_comment(owner, repo, c.id).await.is_ok() {
                removed_issue_comments += 1;
            }
        }
    }

    let review_comments = github
        .list_pull_review_comments(owner, repo, pr_number)
        .await?;
    let mut removed_review_comments = 0usize;
    for c in review_comments {
        if is_prismflow_trace_comment(&c.body)
            || me
                .as_ref()
                .map(|m| c.author_login.as_deref() == Some(m.as_str()))
                .unwrap_or(false)
        {
            if github
                .delete_pull_review_comment(owner, repo, c.id)
                .await
                .is_ok()
            {
                removed_review_comments += 1;
            }
        }
    }

    let reviews = github.list_pull_reviews(owner, repo, pr_number).await?;
    let mut deleted_pending_reviews = 0usize;
    let mut dismissed_reviews = 0usize;
    for r in reviews {
        let owned = me
            .as_ref()
            .map(|m| r.author_login.as_deref() == Some(m.as_str()))
            .unwrap_or(false);
        if !owned && !is_prismflow_trace_comment(&r.body) {
            continue;
        }

        let state_lower = r.state.to_ascii_lowercase();
        if state_lower == "pending"
            && github
                .delete_pending_pull_review(owner, repo, pr_number, r.id)
                .await
                .is_ok()
        {
            deleted_pending_reviews += 1;
            continue;
        }
        if github
            .dismiss_pull_review(
                owner,
                repo,
                pr_number,
                r.id,
                "PrismFlow clean: dismiss stale auto review",
            )
            .await
            .is_ok()
        {
            dismissed_reviews += 1;
        }
    }

    let labels = github.list_issue_labels(owner, repo, pr_number).await?;
    let mut removed_labels = 0usize;
    for label in labels {
        if label.starts_with("pr-reviewer:reviewed:") {
            if github
                .remove_issue_label(owner, repo, pr_number, &label)
                .await
                .is_ok()
            {
                removed_labels += 1;
            }
        }
    }

    println!(
        "clean_result repo={}/{} pr={} removed_issue_comments={} removed_review_comments={} deleted_pending_reviews={} dismissed_reviews={} removed_labels={}",
        owner,
        repo,
        pr_number,
        removed_issue_comments,
        removed_review_comments,
        deleted_pending_reviews,
        dismissed_reviews,
        removed_labels
    );
    Ok(())
}

pub fn github_client_for_action(
    auth_manager: &AuthManager<'_>,
    max_concurrent_api: usize,
    action: &str,
) -> Result<OctocrabGitHubRepository> {
    let token = auth_manager
        .resolve_token()?
        .map(|r| r.token)
        .with_context(|| {
            format!(
                "token required for {action}; run `prismflow auth login <token>` or configure gh/GITHUB_TOKEN"
            )
        })?;
    OctocrabGitHubRepository::new(token, max_concurrent_api)
}

#[derive(Debug, Serialize)]
struct ReviewReport<'a> {
    generated_at: String,
    mode: &'a str,
    total_processed: usize,
    total_failed_retryable: usize,
    total_failed_fatal: usize,
    repos: &'a [RepoReviewStats],
}

fn write_review_report(mode: &str, stats: &[RepoReviewStats]) -> Result<PathBuf> {
    let report = ReviewReport {
        generated_at: chrono::Utc::now().to_rfc3339(),
        mode,
        total_processed: stats.iter().map(|s| s.processed).sum(),
        total_failed_retryable: stats.iter().map(|s| s.failed_retryable).sum(),
        total_failed_fatal: stats.iter().map(|s| s.failed_fatal).sum(),
        repos: stats,
    };

    let root = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".prismflow")
        .join("reports");
    fs::create_dir_all(&root)?;

    let latest = root.join("latest-review-report.json");
    let timestamped = root.join(format!(
        "review-report-{}.json",
        chrono::Utc::now().format("%Y%m%d-%H%M%S")
    ));
    let raw = serde_json::to_string_pretty(&report)?;
    fs::write(&latest, &raw)?;
    fs::write(&timestamped, raw)?;
    Ok(timestamped)
}

async fn run_with_heartbeat<T, F>(tag: &str, fut: F, ctx: &TaskContext) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    let started = std::time::Instant::now();
    tokio::pin!(fut);

    loop {
        tokio::select! {
            res = &mut fut => {
                return res;
            }
            _ = interval.tick() => {
                let secs = started.elapsed().as_secs();
                println!("[WORKING] {tag} is still running... elapsed={}s", secs);
            }
            _ = ctx.cancelled() => {
                return Err(anyhow!(DomainError::CancelledBySignal).context(format!("{tag} cancelled")));
            }
        }
    }
}

fn is_prismflow_trace_comment(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("prismflow")
        || lower.contains("<!-- prismflow:")
        || lower.contains("[prismflow]")
}
