mod application;
mod domain;
mod infrastructure;
mod interface;

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};

use anyhow::{Context, Result};
use application::{
    auth_manager::AuthManager,
    repo_manager::RepoManager,
    review_workflow::{EngineSpec, ReviewWorkflow, ReviewWorkflowOptions},
};
use clap::Parser;
use domain::ports::{ConfigRepository, GitHubRepository};
use infrastructure::{
    github_adapter::OctocrabGitHubRepository,
    local_config_adapter::LocalConfigAdapter,
    shell_adapter::CommandShellAdapter,
    token_providers::{EnvTokenProvider, GhCliTokenProvider, StoredTokenProvider},
};
use interface::cli::{
    AuthSubcommand, Cli, Commands, RepoAgentSubcommand, RepoSubcommand,
    ReviewSubcommand, ScanSubcommand,
};
use tokio::sync::broadcast;
use tokio::time::{Duration, sleep};
use tracing_subscriber::EnvFilter;
use serde::Serialize;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    let config_repo = LocalConfigAdapter::new()?;
    let shell = CommandShellAdapter;
    let stored_provider = StoredTokenProvider::new(config_repo.auth_token_path());
    let gh_provider = GhCliTokenProvider::new(&shell);
    let env_provider = EnvTokenProvider;

    let auth_manager = AuthManager::new(
        vec![&gh_provider, &env_provider, &stored_provider],
        &stored_provider,
    );
    let repo_manager = RepoManager::new(&config_repo);

    match cli.command {
        Commands::Repo(repo) => match repo.command {
            RepoSubcommand::Add { full_name } => {
                let normalized = repo_manager.add_repo(&full_name)?;
                println!("added: {normalized}");
                println!("config: {}", config_repo.config_path().display());
            }
            RepoSubcommand::Remove { full_name } => {
                let target = if let Some(full_name) = full_name {
                    full_name
                } else {
                    let repos = repo_manager.list_repos()?;
                    if repos.is_empty() {
                        println!("no repositories configured");
                        return Ok(());
                    }

                    println!("select repository to remove:");
                    for (idx, item) in repos.iter().enumerate() {
                        println!("  {}. {}", idx + 1, item.full_name);
                    }
                    print!("enter number: ");
                    {
                        use std::io::Write as _;
                        std::io::stdout().flush()?;
                    }
                    let mut input = String::new();
                    std::io::stdin().read_line(&mut input)?;
                    let selected = input
                        .trim()
                        .parse::<usize>()
                        .with_context(|| "invalid selection: expected a number")?;
                    if selected == 0 || selected > repos.len() {
                        anyhow::bail!("invalid selection: out of range");
                    }
                    repos[selected - 1].full_name.clone()
                };

                repo_manager.remove_repo(&target)?;
                println!("removed: {target}");
            }
            RepoSubcommand::List => {
                let repos = repo_manager.list_repos()?;
                if repos.is_empty() {
                    println!("no repositories configured");
                } else {
                    for item in repos {
                        println!(
                            "{} | added_at={} | last_sha={}",
                            item.full_name,
                            item.added_at,
                            item.last_sha.unwrap_or_else(|| "-".to_string())
                        );
                    }
                }
            }
            RepoSubcommand::Agent(agent) => match agent.command {
                RepoAgentSubcommand::Add { full_name, agent } => {
                    repo_manager.add_agent(&full_name, &agent)?;
                    println!("repo={} agent_added={}", full_name, agent);
                }
                RepoAgentSubcommand::Remove { full_name, agent } => {
                    repo_manager.remove_agent(&full_name, &agent)?;
                    println!("repo={} agent_removed={}", full_name, agent);
                }
                RepoAgentSubcommand::List { full_name } => {
                    if let Some(full_name) = full_name {
                        let agents = repo_manager.list_agents(&full_name)?;
                        if agents.is_empty() {
                            println!("repo={} agents=[]", full_name);
                        } else {
                            println!("repo={} agents={}", full_name, agents.join(","));
                        }
                    } else {
                        let repos = repo_manager.list_repos()?;
                        if repos.is_empty() {
                            println!("no repositories configured");
                        } else {
                            for repo in repos {
                                if repo.agents.is_empty() {
                                    println!("repo={} agents=[]", repo.full_name);
                                } else {
                                    println!(
                                        "repo={} agents={}",
                                        repo.full_name,
                                        repo.agents.join(",")
                                    );
                                }
                            }
                        }
                    }
                }
                RepoAgentSubcommand::DirAdd { dir } => {
                    repo_manager.add_agent_prompt_dir(&dir)?;
                    println!("global_agent_prompt_dir_added={}", dir);
                }
                RepoAgentSubcommand::DirRemove { dir } => {
                    repo_manager.remove_agent_prompt_dir(&dir)?;
                    println!("global_agent_prompt_dir_removed={}", dir);
                }
                RepoAgentSubcommand::DirList => {
                    let dirs = repo_manager.list_agent_prompt_dirs()?;
                    if dirs.is_empty() {
                        println!("global_agent_prompt_dirs=[]");
                    } else {
                        println!("global_agent_prompt_dirs={}", dirs.join(","));
                    }
                }
            },
        },
        Commands::Auth(auth) => match auth.command {
            AuthSubcommand::Login { token } => {
                auth_manager.login(&token)?;
                println!("token saved to local config");
            }
            AuthSubcommand::Which => {
                if let Some(resolution) = auth_manager.resolve_token()? {
                    println!("token source: {}", resolution.source);
                    println!(
                        "token prefix: {}***",
                        &resolution.token.chars().take(6).collect::<String>()
                    );
                } else {
                    println!("no token found (checked: gh auth token, GITHUB_TOKEN, stored token)");
                }
            }
        },
        Commands::Scan(scan) => match scan.command {
            ScanSubcommand::Once {
                engine_fingerprint,
                max_concurrent_api,
            } => {
                if repo_manager.list_repos()?.is_empty() {
                    println!("no repositories configured");
                    return Ok(());
                }

                let token = auth_manager
                    .resolve_token()?
                    .map(|r| r.token)
                    .context("token required for scan; run `prismflow auth login <token>` or configure gh/GITHUB_TOKEN")?;

                let github = OctocrabGitHubRepository::new(token, max_concurrent_api)?;
                let workflow = ReviewWorkflow::new(
                    &config_repo,
                    &github,
                    None,
                    engine_fingerprint,
                    ReviewWorkflowOptions::default(),
                );
                let reports = workflow.scan_once().await?;
                if reports.is_empty() {
                    println!("no repositories configured");
                } else {
                    for r in reports {
                        println!("repo={} open_prs={}", r.repo, r.prs.len());
                        for pr in r.prs {
                            println!("  #{} {}", pr.number, pr.title);
                            if let Some(url) = pr.url {
                                println!("    url={url}");
                            }
                            println!("    anchor_key={}", short_key(&pr.anchor_key));
                        }
                    }
                }
            }
        },
        Commands::Review(review) => match review.command {
            ReviewSubcommand::Once {
                engine_fingerprint,
                engine_command,
                engines,
                engine_prompt,
                engine_prompt_file,
                agents,
                keep_diff_files,
                max_concurrent_repos,
                max_concurrent_prs,
                max_concurrent_api,
            } => {
                let resolved_prompt = resolve_engine_prompt(engine_prompt, engine_prompt_file)?;
                let resolved_engines =
                    resolve_engine_specs(&engine_fingerprint, engine_command, engines)?;
                let options = ReviewWorkflowOptions {
                    engine_specs: resolved_engines,
                    engine_prompt: resolved_prompt,
                    agent_prompt_dirs: repo_manager.list_agent_prompt_dirs()?,
                    cli_agents: agents,
                    keep_diff_files,
                    max_concurrent_repos,
                    max_concurrent_prs,
                    ..ReviewWorkflowOptions::default()
                };
                run_review_once(
                    &auth_manager,
                    &config_repo,
                    &shell,
                    engine_fingerprint,
                    max_concurrent_api,
                    options,
                    None,
                )
                .await?;
            }
            ReviewSubcommand::Daemon {
                interval_secs,
                engine_fingerprint,
                engine_command,
                engines,
                engine_prompt,
                engine_prompt_file,
                agents,
                keep_diff_files,
                max_concurrent_repos,
                max_concurrent_prs,
                max_concurrent_api,
            } => {
                let resolved_prompt = resolve_engine_prompt(engine_prompt, engine_prompt_file)?;
                let resolved_engines =
                    resolve_engine_specs(&engine_fingerprint, engine_command, engines)?;
                let options = ReviewWorkflowOptions {
                    engine_specs: resolved_engines,
                    engine_prompt: resolved_prompt,
                    agent_prompt_dirs: repo_manager.list_agent_prompt_dirs()?,
                    cli_agents: agents,
                    keep_diff_files,
                    max_concurrent_repos,
                    max_concurrent_prs,
                    ..ReviewWorkflowOptions::default()
                };
                let (status_tx, mut status_rx) = broadcast::channel::<String>(256);
                let skip_flag = Arc::new(AtomicBool::new(false));
                let status_task = tokio::spawn(async move {
                    while let Ok(msg) = status_rx.recv().await {
                        println!("[STATUS] {msg}");
                    }
                });
                let skip_flag_for_input = skip_flag.clone();
                let input_task = tokio::task::spawn_blocking(move || {
                    use std::io::{self, BufRead};
                    let stdin = io::stdin();
                    for line in stdin.lock().lines().map_while(|v| v.ok()) {
                        if line.trim().eq_ignore_ascii_case("skip") {
                            skip_flag_for_input.store(true, Ordering::Relaxed);
                            println!("[CONTROL] skip received; next pending PR will be skipped");
                        }
                    }
                });

                println!("review daemon started: interval={}s", interval_secs);
                println!("control: type `skip` then Enter to skip next pending PR");
                loop {
                    let _ = status_tx.send("cycle:start".to_string());
                    tokio::select! {
                        r = run_review_once(
                            &auth_manager,
                            &config_repo,
                            &shell,
                            engine_fingerprint.clone(),
                            max_concurrent_api,
                            ReviewWorkflowOptions {
                                status_tx: Some(status_tx.clone()),
                                skip_flag: Some(skip_flag.clone()),
                                ..options.clone()
                            },
                            Some(&status_tx),
                        ) => {
                            if let Err(err) = r {
                                let _ = status_tx.send(format!("cycle:failed error={err:#}"));
                                eprintln!("review cycle failed: {err:#}");
                            } else {
                                let _ = status_tx.send("cycle:done".to_string());
                            }
                        }
                        _ = tokio::signal::ctrl_c() => {
                            println!("received Ctrl+C, exiting daemon");
                            break;
                        }
                    }

                    tokio::select! {
                        _ = sleep(Duration::from_secs(interval_secs)) => {
                            let _ = status_tx.send("daemon:sleep_done".to_string());
                        }
                        _ = tokio::signal::ctrl_c() => {
                            println!("received Ctrl+C, exiting daemon");
                            break;
                        }
                    }
                }
                drop(status_tx);
                let _ = status_task.await;
                input_task.abort();
            }
            ReviewSubcommand::AdHoc {
                pr_url,
                engine_fingerprint,
                engine_command,
                engines,
                engine_prompt,
                engine_prompt_file,
                agents,
                keep_diff_files,
                max_concurrent_api,
            } => {
                let (owner, repo, pr_number) = parse_github_pr_url(&pr_url)
                    .with_context(|| format!("invalid GitHub PR URL: {pr_url}"))?;
                let resolved_prompt = resolve_engine_prompt(engine_prompt, engine_prompt_file)?;
                let resolved_engines =
                    resolve_engine_specs(&engine_fingerprint, engine_command, engines)?;
                let options = ReviewWorkflowOptions {
                    engine_specs: resolved_engines,
                    engine_prompt: resolved_prompt,
                    agent_prompt_dirs: repo_manager.list_agent_prompt_dirs()?,
                    cli_agents: agents,
                    keep_diff_files,
                    ..ReviewWorkflowOptions::default()
                };
                run_review_ad_hoc(
                    &auth_manager,
                    &config_repo,
                    &shell,
                    owner,
                    repo,
                    pr_number,
                    engine_fingerprint,
                    max_concurrent_api,
                    options,
                )
                .await?;
            }
            ReviewSubcommand::Clean {
                pr_url,
                max_concurrent_api,
            } => {
                let (owner, repo, pr_number) = parse_github_pr_url(&pr_url)
                    .with_context(|| format!("invalid GitHub PR URL: {pr_url}"))?;
                run_review_clean(
                    &auth_manager,
                    owner,
                    repo,
                    pr_number,
                    max_concurrent_api,
                )
                .await?;
            }
        },
    }

    Ok(())
}

async fn run_review_once(
    auth_manager: &AuthManager<'_>,
    config_repo: &LocalConfigAdapter,
    shell: &CommandShellAdapter,
    engine_fingerprint: String,
    max_concurrent_api: usize,
    options: ReviewWorkflowOptions,
    status_tx: Option<&broadcast::Sender<String>>,
) -> Result<()> {
    if config_repo.load_config()?.repos.is_empty() {
        println!("no repositories configured");
        return Ok(());
    }

    let token = auth_manager
        .resolve_token()?
        .map(|r| r.token)
        .context("token required for review; run `prismflow auth login <token>` or configure gh/GITHUB_TOKEN")?;

    let github = OctocrabGitHubRepository::new(token, max_concurrent_api)?;
    let workflow = ReviewWorkflow::new(config_repo, &github, Some(shell), engine_fingerprint, options);
    let stats = run_with_heartbeat("review-once", workflow.review_once()).await?;

    if stats.is_empty() {
        println!("no repositories configured");
    } else {
        let report_path = write_review_report("review-once", &stats)?;
        println!("report_file={}", report_path.display());
        for item in stats {
            if let Some(tx) = status_tx {
                let _ = tx.send(format!(
                    "repo={} processed={} skip_completed={} skip_processing={} skip_filtered={} skip_by_operator={} retryable_fail={} fatal_fail={} retryable_error={:?} fatal_error={:?}",
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

async fn run_review_ad_hoc(
    auth_manager: &AuthManager<'_>,
    config_repo: &LocalConfigAdapter,
    shell: &CommandShellAdapter,
    owner: &str,
    repo: &str,
    pr_number: u64,
    engine_fingerprint: String,
    max_concurrent_api: usize,
    options: ReviewWorkflowOptions,
) -> Result<()> {
    let token = auth_manager
        .resolve_token()?
        .map(|r| r.token)
        .context("token required for ad-hoc review; run `prismflow auth login <token>` or configure gh/GITHUB_TOKEN")?;

    let github = OctocrabGitHubRepository::new(token, max_concurrent_api)?;
    let workflow = ReviewWorkflow::new(config_repo, &github, Some(shell), engine_fingerprint, options);
    let stats = run_with_heartbeat(
        "review-ad-hoc",
        workflow.review_ad_hoc(owner, repo, pr_number),
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

async fn run_review_clean(
    auth_manager: &AuthManager<'_>,
    owner: &str,
    repo: &str,
    pr_number: u64,
    max_concurrent_api: usize,
) -> Result<()> {
    let token = auth_manager
        .resolve_token()?
        .map(|r| r.token)
        .context("token required for clean; run `prismflow auth login <token>` or configure gh/GITHUB_TOKEN")?;

    let github = OctocrabGitHubRepository::new(token, max_concurrent_api)?;
    let issue_comments = github.list_issue_comments(owner, repo, pr_number).await?;
    let mut removed_issue_comments = 0usize;
    for c in issue_comments {
        if is_prismflow_trace_comment(&c.body) {
            if github.delete_issue_comment(owner, repo, c.id).await.is_ok() {
                removed_issue_comments += 1;
            }
        }
    }

    let review_comments = github.list_pull_review_comments(owner, repo, pr_number).await?;
    let mut removed_review_comments = 0usize;
    for c in review_comments {
        if is_prismflow_trace_comment(&c.body) {
            if github.delete_pull_review_comment(owner, repo, c.id).await.is_ok() {
                removed_review_comments += 1;
            }
        }
    }

    let labels = github.list_issue_labels(owner, repo, pr_number).await?;
    let mut removed_labels = 0usize;
    for label in labels {
        if label.starts_with("pr-reviewer:reviewed:") {
            if github.remove_issue_label(owner, repo, pr_number, &label).await.is_ok() {
                removed_labels += 1;
            }
        }
    }

    println!(
        "clean_result repo={}/{} pr={} removed_issue_comments={} removed_review_comments={} removed_labels={}",
        owner, repo, pr_number, removed_issue_comments, removed_review_comments, removed_labels
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
    repos: &'a [application::review_workflow::RepoReviewStats],
}

fn write_review_report(
    mode: &str,
    stats: &[application::review_workflow::RepoReviewStats],
) -> Result<PathBuf> {
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

async fn run_with_heartbeat<T, F>(tag: &str, fut: F) -> Result<T>
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
        }
    }
}

fn parse_github_pr_url(url: &str) -> Result<(&str, &str, u64)> {
    let normalized = url.trim().trim_end_matches('/');
    let needle = "github.com/";
    let idx = normalized
        .find(needle)
        .ok_or_else(|| anyhow::anyhow!("not a github.com URL"))?;
    let tail = &normalized[(idx + needle.len())..];
    let parts = tail.split('/').collect::<Vec<_>>();
    if parts.len() < 4 {
        anyhow::bail!("URL format must be github.com/<owner>/<repo>/pull/<number>");
    }
    let owner = parts[0];
    let repo = parts[1];
    let pull_literal = parts[2];
    let num_str = parts[3];
    if pull_literal != "pull" {
        anyhow::bail!("URL path segment must contain /pull/");
    }
    let pr_number = num_str.parse::<u64>()?;
    Ok((owner, repo, pr_number))
}

fn resolve_engine_prompt(
    engine_prompt: Option<String>,
    engine_prompt_file: Option<String>,
) -> Result<Option<String>> {
    if engine_prompt.is_some() && engine_prompt_file.is_some() {
        anyhow::bail!("--engine-prompt and --engine-prompt-file cannot be used together");
    }

    if let Some(file) = engine_prompt_file {
        let content = fs::read_to_string(&file)
            .with_context(|| format!("failed to read engine prompt file: {file}"))?;
        return Ok(Some(content));
    }

    Ok(engine_prompt)
}

fn resolve_engine_specs(
    engine_fingerprint: &str,
    engine_command: Option<String>,
    engines: Vec<String>,
) -> Result<Vec<EngineSpec>> {
    if engine_command.is_some() && !engines.is_empty() {
        anyhow::bail!("--engine-command and --engine cannot be used together");
    }

    if !engines.is_empty() {
        if engines.len() % 2 != 0 {
            anyhow::bail!("--engine requires pairs: <fingerprint> <command>");
        }
        let mut out = Vec::new();
        for pair in engines.chunks_exact(2) {
            let fingerprint = pair[0].trim();
            let command = pair[1].trim();
            if fingerprint.is_empty() || command.is_empty() {
                anyhow::bail!("--engine values cannot be empty");
            }
            out.push(EngineSpec {
                fingerprint: fingerprint.to_string(),
                command: command.to_string(),
            });
        }
        return Ok(out);
    }

    let command = engine_command
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing engine command: set --engine-command or --engine"))?;
    Ok(vec![EngineSpec {
        fingerprint: engine_fingerprint.to_string(),
        command,
    }])
}

fn is_prismflow_trace_comment(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("prismflow")
        || lower.contains("<!-- prismflow:")
        || lower.contains("[prismflow]")
}

fn short_key(full: &str) -> &str {
    let end = full.len().min(12);
    &full[..end]
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}
