use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use tokio::sync::broadcast;

use crate::application::{
    cache_service::CacheCleanupService,
    ci_workflow::{CiWorkflow, CiWorkflowOptions},
    context::TaskContext,
    review_orchestrator::ReviewOrchestrator,
    review_workflow::ReviewWorkflowOptions,
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
    options: ReviewWorkflowOptions,
    ctx: Arc<TaskContext>,
    cache_cleanup_hours: u64,
    archive_report_on_failure_only: bool,
    status_tx: Option<&broadcast::Sender<String>>,
) -> Result<()> {
    let orchestrator = ReviewOrchestrator::new(config_repo, fs);
    let _ = orchestrator
        .run_once(
            github,
            Some(shell),
            git,
            options,
            ctx,
            cache_cleanup_hours,
            archive_report_on_failure_only,
            status_tx,
        )
        .await?;
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
    cache_cleanup_hours: u64,
) -> Result<()> {
    if ctx.is_cancelled() {
        return Err(anyhow!(DomainError::CancelledBySignal));
    }
    CacheCleanupService::new(fs).cleanup_if_needed(cache_cleanup_hours);
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
    options: ReviewWorkflowOptions,
    ctx: Arc<TaskContext>,
    cache_cleanup_hours: u64,
) -> Result<()> {
    let orchestrator = ReviewOrchestrator::new(config_repo, fs);
    let _ = orchestrator
        .run_ad_hoc(
            github,
            Some(shell),
            git,
            owner,
            repo,
            pr_number,
            options,
            ctx,
            cache_cleanup_hours,
        )
        .await?;
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
                tracing::info!(tag = tag, elapsed_secs = secs, "task is still running");
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
