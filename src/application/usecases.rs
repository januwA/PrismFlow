use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde::Serialize;
use tokio::sync::broadcast;

use crate::application::{
    agent_preflight::{panic_if_cli_agents_missing, panic_if_required_agents_missing},
    ci_workflow::{CiWorkflow, CiWorkflowOptions},
    context::TaskContext,
    review_workflow::{RepoReviewStats, ReviewWorkflow, ReviewWorkflowOptions},
};
use crate::domain::errors::DomainError;
use crate::domain::ports::{
    ConfigRepository, FileSystem, GitHubRepository, GitService, ShellAdapter,
};

pub async fn run_review_once(
    config_repo: &dyn ConfigRepository,
    github: &dyn GitHubRepository,
    shell: &dyn ShellAdapter,
    fs: &dyn FileSystem,
    git: &dyn GitService,
    mut options: ReviewWorkflowOptions,
    ctx: Arc<TaskContext>,
    archive_report_on_failure_only: bool,
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
    panic_if_required_agents_missing(fs, &config, &options, "review-once");
    options.task_context = Some(ctx.clone());

    let workflow = ReviewWorkflow::new(config_repo, github, Some(shell), fs, git, options);
    let stats = run_with_heartbeat("review-once", workflow.review_once(), &ctx).await?;

    if stats.is_empty() {
        println!("no repositories configured");
    } else {
        let report_path =
            write_review_report(fs, "review-once", &stats, archive_report_on_failure_only)?;
        println!("report_file={}", report_path.display());
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
            println!(
                "repo={} processed={} skipped_completed={} skipped_processing={} skipped_filtered={} skipped_by_author={} skipped_by_operator={} recovered_stale_processing={} fallback_general={} failed_retryable={} failed_fatal={} last_retryable_error={:?} last_fatal_error={:?}",
                item.repo,
                item.processed,
                item.skipped_completed,
                item.skipped_processing,
                item.skipped_filtered,
                item.skipped_by_author,
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
    config_repo: &dyn ConfigRepository,
    github: &dyn GitHubRepository,
    shell: &dyn ShellAdapter,
    fs: &dyn FileSystem,
    git: &dyn GitService,
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

    let workflow = CiWorkflow::new(config_repo, github, shell, fs, git, options);
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
    config_repo: &dyn ConfigRepository,
    github: &dyn GitHubRepository,
    shell: &dyn ShellAdapter,
    fs: &dyn FileSystem,
    git: &dyn GitService,
    owner: &str,
    repo: &str,
    pr_number: u64,
    mut options: ReviewWorkflowOptions,
    ctx: Arc<TaskContext>,
) -> Result<()> {
    if ctx.is_cancelled() {
        return Err(anyhow!(DomainError::CancelledBySignal));
    }
    panic_if_cli_agents_missing(fs, &options, "review-ad-hoc");
    options.task_context = Some(ctx.clone());
    let workflow = ReviewWorkflow::new(config_repo, github, Some(shell), fs, git, options);
    let stats = run_with_heartbeat(
        "review-ad-hoc",
        workflow.review_ad_hoc(owner, repo, pr_number),
        &ctx,
    )
    .await?;

    let report_path =
        write_review_report(fs, "review-ad-hoc", std::slice::from_ref(&stats), false)?;
    println!("report_file={}", report_path.display());

    println!(
        "repo={} processed={} skipped_completed={} skipped_processing={} skipped_filtered={} skipped_by_author={} skipped_by_operator={} recovered_stale_processing={} fallback_general={} failed_retryable={} failed_fatal={} last_retryable_error={:?} last_fatal_error={:?}",
        stats.repo,
        stats.processed,
        stats.skipped_completed,
        stats.skipped_processing,
        stats.skipped_filtered,
        stats.skipped_by_author,
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
    github: &dyn GitHubRepository,
    owner: &str,
    repo: &str,
    pr_number: u64,
    ctx: Arc<TaskContext>,
) -> Result<()> {
    if ctx.is_cancelled() {
        return Err(anyhow!(DomainError::CancelledBySignal));
    }
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

#[derive(Debug, Serialize)]
struct ReviewReport<'a> {
    generated_at: String,
    mode: &'a str,
    total_processed: usize,
    total_failed_retryable: usize,
    total_failed_fatal: usize,
    repos: &'a [RepoReviewStats],
}

fn write_review_report(
    fs: &dyn FileSystem,
    mode: &str,
    stats: &[RepoReviewStats],
    archive_on_failure_only: bool,
) -> Result<PathBuf> {
    let report = ReviewReport {
        generated_at: chrono::Utc::now().to_rfc3339(),
        mode,
        total_processed: stats.iter().map(|s| s.processed).sum(),
        total_failed_retryable: stats.iter().map(|s| s.failed_retryable).sum(),
        total_failed_fatal: stats.iter().map(|s| s.failed_fatal).sum(),
        repos: stats,
    };

    let root = fs.current_dir()?.join(".prismflow").join("reports");
    fs.create_dir_all(&root)?;

    let latest = root.join("latest-review-report.json");
    let timestamped = root.join(format!(
        "review-report-{}.json",
        chrono::Utc::now().format("%Y%m%d-%H%M%S")
    ));
    let raw = serde_json::to_string_pretty(&report)?;
    fs.write(&latest, raw.as_bytes())?;
    let should_archive = !archive_on_failure_only || has_review_failures(stats);
    if should_archive {
        fs.write(&timestamped, raw.as_bytes())?;
        return Ok(timestamped);
    }
    Ok(latest)
}

fn has_review_failures(stats: &[RepoReviewStats]) -> bool {
    stats
        .iter()
        .any(|s| s.failed_retryable > 0 || s.failed_fatal > 0)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_review_failures_detects_no_failure() {
        let stats = vec![RepoReviewStats {
            repo: "owner/repo".to_string(),
            processed: 1,
            ..RepoReviewStats::default()
        }];
        assert!(!has_review_failures(&stats));
    }

    #[test]
    fn has_review_failures_detects_retryable_or_fatal() {
        let retryable = vec![RepoReviewStats {
            repo: "owner/repo".to_string(),
            failed_retryable: 1,
            ..RepoReviewStats::default()
        }];
        assert!(has_review_failures(&retryable));

        let fatal = vec![RepoReviewStats {
            repo: "owner/repo".to_string(),
            failed_fatal: 1,
            ..RepoReviewStats::default()
        }];
        assert!(has_review_failures(&fatal));
    }
}
