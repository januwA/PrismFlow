use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

const CACHE_SCAN_DIRS: [&str; 2] = ["tmp-diffs", "tmp-ci"];
const REPO_CACHE_DIRS: [&str; 2] = ["ci-repo-cache", "repo-cache"];

pub async fn run_review_once(
    config_repo: &dyn ConfigRepository,
    github: &dyn GitHubRepository,
    shell: &dyn ShellAdapter,
    fs: &dyn FileSystem,
    git: &dyn GitService,
    mut options: ReviewWorkflowOptions,
    ctx: Arc<TaskContext>,
    cache_cleanup_hours: u64,
    archive_report_on_failure_only: bool,
    status_tx: Option<&broadcast::Sender<String>>,
) -> Result<()> {
    if ctx.is_cancelled() {
        return Err(anyhow!(DomainError::CancelledBySignal));
    }
    inspect_and_cleanup_stale_prismflow_cache(fs, cache_cleanup_hours);
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
    cache_cleanup_hours: u64,
) -> Result<()> {
    if ctx.is_cancelled() {
        return Err(anyhow!(DomainError::CancelledBySignal));
    }
    inspect_and_cleanup_stale_prismflow_cache(fs, cache_cleanup_hours);
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
    cache_cleanup_hours: u64,
) -> Result<()> {
    if ctx.is_cancelled() {
        return Err(anyhow!(DomainError::CancelledBySignal));
    }
    inspect_and_cleanup_stale_prismflow_cache(fs, cache_cleanup_hours);
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

#[derive(Debug, Default, Clone, Copy)]
struct CacheCleanupStats {
    checked_files: usize,
    stale_files: usize,
    removed_files: usize,
    remove_failures: usize,
    checked_repo_entries: usize,
    stale_repo_entries: usize,
    removed_repo_entries: usize,
    repo_remove_failures: usize,
}

fn inspect_and_cleanup_stale_prismflow_cache(fs: &dyn FileSystem, cache_cleanup_hours: u64) {
    let cwd = match fs.current_dir() {
        Ok(v) => v,
        Err(_) => return,
    };
    let stale_after =
        Duration::from_secs(cache_cleanup_hours.saturating_mul(60).saturating_mul(60));
    let stats = cleanup_stale_prismflow_cache_under(&cwd, stale_after, SystemTime::now());
    if stats.stale_files > 0
        || stats.remove_failures > 0
        || stats.stale_repo_entries > 0
        || stats.repo_remove_failures > 0
    {
        println!(
            "cache_check root={} cleanup_hours={} checked_files={} stale_files={} removed_files={} remove_failures={} checked_repo_entries={} stale_repo_entries={} removed_repo_entries={} repo_remove_failures={}",
            cwd.join(".prismflow").display(),
            cache_cleanup_hours,
            stats.checked_files,
            stats.stale_files,
            stats.removed_files,
            stats.remove_failures,
            stats.checked_repo_entries,
            stats.stale_repo_entries,
            stats.removed_repo_entries,
            stats.repo_remove_failures
        );
    }
}

fn cleanup_stale_prismflow_cache_under(
    cwd: &Path,
    stale_after: Duration,
    now: SystemTime,
) -> CacheCleanupStats {
    let root = cwd.join(".prismflow");
    let mut stats = CacheCleanupStats::default();
    for dir in CACHE_SCAN_DIRS {
        walk_and_cleanup_stale_files(&root.join(dir), stale_after, now, &mut stats);
    }
    for dir in REPO_CACHE_DIRS {
        cleanup_stale_repo_cache_entries(&root.join(dir), stale_after, now, &mut stats);
    }
    stats
}

fn cleanup_stale_repo_cache_entries(
    root: &Path,
    stale_after: Duration,
    now: SystemTime,
    stats: &mut CacheCleanupStats,
) {
    let entries = match fs::read_dir(root) {
        Ok(v) => v,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(v) => v,
            Err(_) => continue,
        };
        if !(file_type.is_file() || file_type.is_dir()) {
            continue;
        }
        stats.checked_repo_entries += 1;

        let modified = cache_entry_timestamp(&path)
            .or_else(|| entry.metadata().ok().and_then(|m| m.modified().ok()));
        let Some(modified) = modified else {
            continue;
        };
        let age = match now.duration_since(modified) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if age < stale_after {
            continue;
        }

        stats.stale_repo_entries += 1;
        let removed = if file_type.is_dir() {
            fs::remove_dir_all(&path).is_ok()
        } else {
            fs::remove_file(&path).is_ok()
        };
        if removed {
            stats.removed_repo_entries += 1;
        } else {
            stats.repo_remove_failures += 1;
        }
    }
}

fn cache_entry_timestamp(path: &Path) -> Option<SystemTime> {
    let name = path.file_name()?.to_str()?;
    let (_, ts_part) = name.rsplit_once('_')?;
    let ts = ts_part.parse::<u64>().ok()?;
    UNIX_EPOCH.checked_add(Duration::from_secs(ts))
}

fn walk_and_cleanup_stale_files(
    dir: &Path,
    stale_after: Duration,
    now: SystemTime,
    stats: &mut CacheCleanupStats,
) {
    let entries = match fs::read_dir(dir) {
        Ok(v) => v,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(v) => v,
            Err(_) => continue,
        };
        if file_type.is_dir() {
            walk_and_cleanup_stale_files(&path, stale_after, now, stats);
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        stats.checked_files += 1;

        let metadata = match entry.metadata() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let modified = match metadata.modified() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let age = match now.duration_since(modified) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if age < stale_after {
            continue;
        }

        stats.stale_files += 1;
        if fs::remove_file(&path).is_ok() {
            stats.removed_files += 1;
        } else {
            stats.remove_failures += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

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

    #[test]
    fn cleanup_stale_prismflow_cache_removes_old_files() {
        let base = make_temp_dir("cleanup_removes_old_files");
        let root = base.join(".prismflow").join("tmp-diffs");
        fs::create_dir_all(&root).expect("create tmp-diffs");
        let stale_file = root.join("a.diff");
        fs::write(&stale_file, b"old").expect("write stale file");

        let stats =
            cleanup_stale_prismflow_cache_under(&base, Duration::from_secs(0), SystemTime::now());

        assert_eq!(stats.checked_files, 1);
        assert_eq!(stats.stale_files, 1);
        assert_eq!(stats.removed_files, 1);
        assert!(!stale_file.exists());
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn cleanup_stale_prismflow_cache_keeps_recent_files() {
        let base = make_temp_dir("cleanup_keeps_recent_files");
        let root = base.join(".prismflow").join("tmp-ci");
        fs::create_dir_all(&root).expect("create tmp-ci");
        let recent_file = root.join("payload.json");
        fs::write(&recent_file, b"recent").expect("write recent file");

        let stats = cleanup_stale_prismflow_cache_under(
            &base,
            Duration::from_secs(24 * 60 * 60),
            SystemTime::now(),
        );

        assert_eq!(stats.checked_files, 1);
        assert_eq!(stats.stale_files, 0);
        assert_eq!(stats.removed_files, 0);
        assert!(recent_file.exists());
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn cleanup_stale_prismflow_cache_removes_old_repo_cache_entry() {
        let base = make_temp_dir("cleanup_removes_old_repo_cache_entry");
        let root = base
            .join(".prismflow")
            .join("ci-repo-cache")
            .join("owner_repo");
        fs::create_dir_all(&root).expect("create repo cache dir");
        let cached_file = root.join("README.md");
        fs::write(&cached_file, b"old").expect("write repo cache file");

        let stats =
            cleanup_stale_prismflow_cache_under(&base, Duration::from_secs(0), SystemTime::now());

        assert_eq!(stats.checked_repo_entries, 1);
        assert_eq!(stats.stale_repo_entries, 1);
        assert_eq!(stats.removed_repo_entries, 1);
        assert!(!root.exists());
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn cleanup_stale_prismflow_cache_keeps_recent_repo_cache_entry() {
        let base = make_temp_dir("cleanup_keeps_recent_repo_cache_entry");
        let root = base
            .join(".prismflow")
            .join("ci-repo-cache")
            .join("owner_repo");
        fs::create_dir_all(&root).expect("create repo cache dir");
        let cached_file = root.join("README.md");
        fs::write(&cached_file, b"recent").expect("write repo cache file");

        let stats = cleanup_stale_prismflow_cache_under(
            &base,
            Duration::from_secs(24 * 60 * 60),
            SystemTime::now(),
        );

        assert_eq!(stats.checked_repo_entries, 1);
        assert_eq!(stats.stale_repo_entries, 0);
        assert_eq!(stats.removed_repo_entries, 0);
        assert!(root.exists());
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn cleanup_stale_prismflow_cache_removes_old_legacy_repo_cache_entry() {
        let base = make_temp_dir("cleanup_removes_old_legacy_repo_cache_entry");
        let root = base
            .join(".prismflow")
            .join("repo-cache")
            .join("owner_repo");
        fs::create_dir_all(&root).expect("create legacy repo cache dir");
        let cached_file = root.join("README.md");
        fs::write(&cached_file, b"old").expect("write legacy repo cache file");

        let stats =
            cleanup_stale_prismflow_cache_under(&base, Duration::from_secs(0), SystemTime::now());

        assert_eq!(stats.stale_repo_entries, 1);
        assert_eq!(stats.removed_repo_entries, 1);
        assert!(!root.exists());
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn cleanup_stale_prismflow_cache_uses_repo_cache_dir_timestamp() {
        let base = make_temp_dir("cleanup_uses_repo_cache_dir_timestamp");
        let root = base.join(".prismflow").join("repo-cache");
        fs::create_dir_all(&root).expect("create legacy repo cache root");
        let stale = root.join("owner_repo_pr1_abc_10");
        let fresh = root.join("owner_repo_pr1_abc_200");
        fs::create_dir_all(&stale).expect("create stale cache dir");
        fs::create_dir_all(&fresh).expect("create fresh cache dir");

        let now = UNIX_EPOCH
            .checked_add(Duration::from_secs(100))
            .expect("build now");
        let stats = cleanup_stale_prismflow_cache_under(&base, Duration::from_secs(50), now);

        assert_eq!(stats.checked_repo_entries, 2);
        assert_eq!(stats.stale_repo_entries, 1);
        assert_eq!(stats.removed_repo_entries, 1);
        assert!(!stale.exists());
        assert!(fresh.exists());
        let _ = fs::remove_dir_all(base);
    }

    fn make_temp_dir(tag: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("duration")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("prismflow-usecases-{tag}-{stamp}"));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }
}
