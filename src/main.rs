mod application;
mod domain;
mod infrastructure;
mod interface;

use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::{Context, Result};
use application::{
    agent_preflight::{panic_if_cli_agents_missing, panic_if_required_agents_missing},
    auth_manager::AuthManager,
    ci_workflow::{CiWorkflow, CiWorkflowOptions},
    repo_manager::RepoManager,
    review_workflow::{EngineSpec, ReviewWorkflow, ReviewWorkflowOptions},
};
use axum::{
    Router,
    extract::{Form, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect},
    routing::{get, post},
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
    AuthSubcommand, CiSubcommand, Cli, Commands, RepoAgentSubcommand, RepoSubcommand,
    ReviewSubcommand, ScanSubcommand,
};
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::{Mutex, Notify, broadcast, mpsc};
use tokio::time::{Duration, sleep};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Clone)]
enum UiCommand {
    TriggerReviewNow,
    TriggerSkip,
    AdHoc(String),
    RepoAdd(String),
    RepoRemove(String),
    AgentAdd { repo: String, agent: String },
    AgentRemove { repo: String, agent: String },
    DirAdd(String),
    DirRemove(String),
}

#[derive(Clone)]
struct UiState {
    token: Option<String>,
    status_log: Arc<Mutex<Vec<String>>>,
    command_tx: mpsc::UnboundedSender<UiCommand>,
    notify: Arc<Notify>,
    config_repo: Arc<LocalConfigAdapter>,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    let shell_override = cli.shell.clone();
    let config_repo = Arc::new(LocalConfigAdapter::new()?);
    let shell = CommandShellAdapter::new(shell_override);
    let stored_provider = StoredTokenProvider::new(config_repo.auth_token_path());
    let gh_provider = GhCliTokenProvider::new(&shell);
    let env_provider = EnvTokenProvider;

    let auth_manager = AuthManager::new(
        vec![&gh_provider, &env_provider, &stored_provider],
        &stored_provider,
    );
    let repo_manager = RepoManager::new(config_repo.as_ref());

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
                RepoAgentSubcommand::Add { args } => {
                    let (full_name, agent) = parse_repo_agent_args(args)?;
                    let target = if let Some(full_name) = full_name {
                        full_name
                    } else {
                        let Some(selected) = select_repo_interactively(
                            &repo_manager,
                            "select repository to add agent:",
                        )?
                        else {
                            println!("no repositories configured");
                            return Ok(());
                        };
                        selected
                    };
                    repo_manager.add_agent(&target, &agent)?;
                    println!("repo={} agent_added={}", target, agent);
                }
                RepoAgentSubcommand::Remove { args } => {
                    let (full_name, agent) = parse_repo_agent_args(args)?;
                    let target = if let Some(full_name) = full_name {
                        full_name
                    } else {
                        let Some(selected) = select_repo_interactively(
                            &repo_manager,
                            "select repository to remove agent:",
                        )?
                        else {
                            println!("no repositories configured");
                            return Ok(());
                        };
                        selected
                    };
                    repo_manager.remove_agent(&target, &agent)?;
                    println!("repo={} agent_removed={}", target, agent);
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
                max_concurrent_api,
                repos,
                exclude_repos,
            } => {
                if repo_manager.list_repos()?.is_empty() {
                    println!("no repositories configured");
                    return Ok(());
                }

                let github = github_client_for_action(&auth_manager, max_concurrent_api, "scan")?;
                let workflow = ReviewWorkflow::new(
                    config_repo.as_ref(),
                    &github,
                    None,
                    ReviewWorkflowOptions {
                        include_repos: repos,
                        exclude_repos,
                        ..ReviewWorkflowOptions::default()
                    },
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
        Commands::Ci(ci) => match ci.command {
            CiSubcommand::Once {
                engines,
                engine_prompt,
                engine_prompt_file,
                clone_repo,
                clone_workspace_dir,
                clone_depth,
                max_concurrent_repos,
                max_concurrent_api,
                repos,
                exclude_repos,
            } => {
                if repo_manager.list_repos()?.is_empty() {
                    println!("no repositories configured");
                    return Ok(());
                }
                let resolved_prompt = resolve_engine_prompt(engine_prompt, engine_prompt_file)?;
                let resolved_engines = resolve_engine_specs(engines)?;
                let github = github_client_for_action(&auth_manager, max_concurrent_api, "ci")?;
                let workflow = CiWorkflow::new(
                    config_repo.as_ref(),
                    &github,
                    &shell,
                    CiWorkflowOptions {
                        max_concurrent_repos,
                        engine_specs: resolved_engines,
                        engine_prompt: resolved_prompt,
                        clone_repo_enabled: clone_repo,
                        clone_workspace_dir,
                        clone_depth,
                        include_repos: repos,
                        exclude_repos,
                    },
                );
                let stats = run_with_heartbeat("ci-once", workflow.run_once()).await?;
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
            }
        },
        Commands::Review(review) => match review.command {
            ReviewSubcommand::Once {
                engines,
                engine_prompt,
                engine_prompt_file,
                agents,
                clone_repo,
                clone_workspace_dir,
                clone_depth,
                keep_diff_files,
                max_concurrent_repos,
                max_concurrent_prs,
                max_concurrent_api,
                large_pr_max_files,
                large_pr_max_lines,
                repos,
                exclude_repos,
            } => {
                let resolved_prompt = resolve_engine_prompt(engine_prompt, engine_prompt_file)?;
                let resolved_engines = resolve_engine_specs(engines)?;
                let options = ReviewWorkflowOptions {
                    engine_specs: resolved_engines,
                    engine_prompt: resolved_prompt,
                    agent_prompt_dirs: repo_manager.list_agent_prompt_dirs()?,
                    cli_agents: agents,
                    clone_repo_enabled: clone_repo,
                    clone_workspace_dir,
                    clone_depth,
                    large_pr_max_files,
                    large_pr_max_changed_lines: large_pr_max_lines,
                    keep_diff_files,
                    max_concurrent_repos,
                    max_concurrent_prs,
                    include_repos: repos,
                    exclude_repos,
                    ..ReviewWorkflowOptions::default()
                };
                run_review_once(
                    &auth_manager,
                    config_repo.as_ref(),
                    &shell,
                    max_concurrent_api,
                    options,
                    None,
                )
                .await?;
            }
            ReviewSubcommand::Daemon {
                interval_secs,
                ui,
                ui_bind,
                ui_token,
                engines,
                engine_prompt,
                engine_prompt_file,
                agents,
                clone_repo,
                clone_workspace_dir,
                clone_depth,
                keep_diff_files,
                max_concurrent_repos,
                max_concurrent_prs,
                max_concurrent_api,
                large_pr_max_files,
                large_pr_max_lines,
                repos,
                exclude_repos,
            } => {
                let resolved_prompt = resolve_engine_prompt(engine_prompt, engine_prompt_file)?;
                let resolved_engines = resolve_engine_specs(engines)?;
                let mut options = ReviewWorkflowOptions {
                    engine_specs: resolved_engines,
                    engine_prompt: resolved_prompt,
                    agent_prompt_dirs: repo_manager.list_agent_prompt_dirs()?,
                    cli_agents: agents,
                    clone_repo_enabled: clone_repo,
                    clone_workspace_dir,
                    clone_depth,
                    large_pr_max_files,
                    large_pr_max_changed_lines: large_pr_max_lines,
                    keep_diff_files,
                    max_concurrent_repos,
                    max_concurrent_prs,
                    include_repos: repos,
                    exclude_repos,
                    ..ReviewWorkflowOptions::default()
                };
                let (status_tx, mut status_rx) = broadcast::channel::<String>(256);
                let status_log = Arc::new(Mutex::new(Vec::<String>::new()));
                let status_log_for_task = status_log.clone();
                let skip_flag = Arc::new(AtomicBool::new(false));
                let (ui_cmd_tx, mut ui_cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
                let ui_notify = Arc::new(Notify::new());
                let status_task = tokio::spawn(async move {
                    while let Ok(msg) = status_rx.recv().await {
                        println!("[STATUS] {msg}");
                        let mut log = status_log_for_task.lock().await;
                        log.push(msg);
                        if log.len() > 200 {
                            let drop_n = log.len().saturating_sub(200);
                            log.drain(0..drop_n);
                        }
                    }
                });
                let mut ui_task = None;
                if ui {
                    let addr: SocketAddr = ui_bind
                        .parse()
                        .with_context(|| format!("invalid --ui-bind address: {ui_bind}"))?;
                    let ui_state = UiState {
                        token: ui_token.clone(),
                        status_log: status_log.clone(),
                        command_tx: ui_cmd_tx.clone(),
                        notify: ui_notify.clone(),
                        config_repo: config_repo.clone(),
                    };
                    println!("ui started: http://{addr}/");
                    if let Some(token) = &ui_token {
                        println!("ui token required: append ?token={token} in URL");
                    }
                    ui_task = Some(tokio::spawn(async move {
                        if let Err(err) = run_ui_server(addr, ui_state).await {
                            eprintln!("ui server stopped: {err:#}");
                        }
                    }));
                }
                let skip_flag_for_input = skip_flag.clone();
                let ui_notify_for_input = ui_notify.clone();
                let input_task = tokio::task::spawn_blocking(move || {
                    use std::io::{self, BufRead};
                    let stdin = io::stdin();
                    for line in stdin.lock().lines().map_while(|v| v.ok()) {
                        if line.trim().eq_ignore_ascii_case("skip") {
                            skip_flag_for_input.store(true, Ordering::Relaxed);
                            ui_notify_for_input.notify_one();
                            println!("[CONTROL] skip received; next pending PR will be skipped");
                        }
                    }
                });

                println!("review daemon started: interval={}s", interval_secs);
                println!("control: type `skip` then Enter to skip next pending PR");
                loop {
                    if let Ok(dirs) = repo_manager.list_agent_prompt_dirs() {
                        options.agent_prompt_dirs = dirs;
                    }
                    let mut run_now = false;
                    while let Ok(cmd) = ui_cmd_rx.try_recv() {
                        match cmd {
                            UiCommand::TriggerReviewNow => run_now = true,
                            UiCommand::TriggerSkip => {
                                skip_flag.store(true, Ordering::Relaxed);
                            }
                            UiCommand::AdHoc(pr_url) => {
                                let _ = status_tx.send(format!("ui:adhoc requested url={pr_url}"));
                                match parse_github_pr_url(&pr_url) {
                                    Ok((owner, repo, pr_number)) => {
                                        let mut adhoc_opts = options.clone();
                                        adhoc_opts.status_tx = Some(status_tx.clone());
                                        if let Err(err) = run_review_ad_hoc(
                                            &auth_manager,
                                            config_repo.as_ref(),
                                            &shell,
                                            owner,
                                            repo,
                                            pr_number,
                                            max_concurrent_api,
                                            adhoc_opts,
                                        )
                                        .await
                                        {
                                            let _ = status_tx
                                                .send(format!("ui:adhoc failed error={err:#}"));
                                        } else {
                                            let _ = status_tx.send("ui:adhoc done".to_string());
                                        }
                                    }
                                    Err(err) => {
                                        let _ = status_tx
                                            .send(format!("ui:adhoc invalid_url error={err:#}"));
                                    }
                                }
                            }
                            UiCommand::RepoAdd(repo) => match repo_manager.add_repo(&repo) {
                                Ok(v) => {
                                    let _ = status_tx.send(format!("ui:repo added {v}"));
                                }
                                Err(err) => {
                                    let _ =
                                        status_tx.send(format!("ui:repo add failed error={err:#}"));
                                }
                            },
                            UiCommand::RepoRemove(repo) => match repo_manager.remove_repo(&repo) {
                                Ok(_) => {
                                    let _ = status_tx.send(format!("ui:repo removed {repo}"));
                                }
                                Err(err) => {
                                    let _ = status_tx
                                        .send(format!("ui:repo remove failed error={err:#}"));
                                }
                            },
                            UiCommand::AgentAdd { repo, agent } => {
                                match repo_manager.add_agent(&repo, &agent) {
                                    Ok(_) => {
                                        let _ = status_tx.send(format!(
                                            "ui:agent added repo={repo} agent={agent}"
                                        ));
                                    }
                                    Err(err) => {
                                        let _ = status_tx
                                            .send(format!("ui:agent add failed error={err:#}"));
                                    }
                                }
                            }
                            UiCommand::AgentRemove { repo, agent } => {
                                match repo_manager.remove_agent(&repo, &agent) {
                                    Ok(_) => {
                                        let _ = status_tx.send(format!(
                                            "ui:agent removed repo={repo} agent={agent}"
                                        ));
                                    }
                                    Err(err) => {
                                        let _ = status_tx
                                            .send(format!("ui:agent remove failed error={err:#}"));
                                    }
                                }
                            }
                            UiCommand::DirAdd(dir) => {
                                match repo_manager.add_agent_prompt_dir(&dir) {
                                    Ok(_) => {
                                        if let Ok(dirs) = repo_manager.list_agent_prompt_dirs() {
                                            options.agent_prompt_dirs = dirs;
                                        }
                                        let _ =
                                            status_tx.send(format!("ui:global dir added {dir}"));
                                    }
                                    Err(err) => {
                                        let _ = status_tx.send(format!(
                                            "ui:global dir add failed error={err:#}"
                                        ));
                                    }
                                }
                            }
                            UiCommand::DirRemove(dir) => {
                                match repo_manager.remove_agent_prompt_dir(&dir) {
                                    Ok(_) => {
                                        if let Ok(dirs) = repo_manager.list_agent_prompt_dirs() {
                                            options.agent_prompt_dirs = dirs;
                                        }
                                        let _ =
                                            status_tx.send(format!("ui:global dir removed {dir}"));
                                    }
                                    Err(err) => {
                                        let _ = status_tx.send(format!(
                                            "ui:global dir remove failed error={err:#}"
                                        ));
                                    }
                                }
                            }
                        }
                    }
                    let _ = status_tx.send("cycle:start".to_string());
                    tokio::select! {
                        r = run_review_once(
                            &auth_manager,
                            config_repo.as_ref(),
                            &shell,
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

                    if run_now {
                        let _ = status_tx.send("ui:triggered immediate next cycle".to_string());
                        continue;
                    }

                    tokio::select! {
                        _ = sleep(Duration::from_secs(interval_secs)) => {
                            let _ = status_tx.send("daemon:sleep_done".to_string());
                        }
                        _ = ui_notify.notified() => {
                            let _ = status_tx.send("ui:wakeup".to_string());
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
                if let Some(task) = ui_task {
                    task.abort();
                }
            }
            ReviewSubcommand::AdHoc {
                pr_url,
                engines,
                engine_prompt,
                engine_prompt_file,
                agents,
                clone_repo,
                clone_workspace_dir,
                clone_depth,
                keep_diff_files,
                max_concurrent_api,
                large_pr_max_files,
                large_pr_max_lines,
            } => {
                let (owner, repo, pr_number) = parse_github_pr_url(&pr_url)
                    .with_context(|| format!("invalid GitHub PR URL: {pr_url}"))?;
                let resolved_prompt = resolve_engine_prompt(engine_prompt, engine_prompt_file)?;
                let resolved_engines = resolve_engine_specs(engines)?;
                let options = ReviewWorkflowOptions {
                    engine_specs: resolved_engines,
                    engine_prompt: resolved_prompt,
                    agent_prompt_dirs: repo_manager.list_agent_prompt_dirs()?,
                    cli_agents: agents,
                    clone_repo_enabled: clone_repo,
                    clone_workspace_dir,
                    clone_depth,
                    large_pr_max_files,
                    large_pr_max_changed_lines: large_pr_max_lines,
                    keep_diff_files,
                    ..ReviewWorkflowOptions::default()
                };
                run_review_ad_hoc(
                    &auth_manager,
                    config_repo.as_ref(),
                    &shell,
                    owner,
                    repo,
                    pr_number,
                    max_concurrent_api,
                    options,
                )
                .await?;
            }
            ReviewSubcommand::Clean {
                pr_urls,
                max_concurrent_api,
            } => {
                let mut failed = 0usize;
                for pr_url in pr_urls {
                    match parse_github_pr_url(&pr_url)
                        .with_context(|| format!("invalid GitHub PR URL: {pr_url}"))
                    {
                        Ok((owner, repo, pr_number)) => {
                            if let Err(err) = run_review_clean(
                                &auth_manager,
                                owner,
                                repo,
                                pr_number,
                                max_concurrent_api,
                            )
                            .await
                            {
                                failed += 1;
                                eprintln!("clean_failed url={} error={err:#}", pr_url);
                            }
                        }
                        Err(err) => {
                            failed += 1;
                            eprintln!("clean_failed url={} error={err:#}", pr_url);
                        }
                    }
                }
                if failed > 0 {
                    anyhow::bail!("clean finished with failures: {}", failed);
                }
            }
        },
    }

    Ok(())
}

async fn run_review_once(
    auth_manager: &AuthManager<'_>,
    config_repo: &LocalConfigAdapter,
    shell: &CommandShellAdapter,
    max_concurrent_api: usize,
    options: ReviewWorkflowOptions,
    status_tx: Option<&broadcast::Sender<String>>,
) -> Result<()> {
    let config = config_repo.load_config()?;
    if config.repos.is_empty() {
        println!("no repositories configured");
        return Ok(());
    }
    panic_if_required_agents_missing(&config, &options, "review-once");

    let github = github_client_for_action(auth_manager, max_concurrent_api, "review")?;
    let workflow = ReviewWorkflow::new(config_repo, &github, Some(shell), options);
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
    max_concurrent_api: usize,
    options: ReviewWorkflowOptions,
) -> Result<()> {
    panic_if_cli_agents_missing(&options, "review-ad-hoc");
    let github = github_client_for_action(auth_manager, max_concurrent_api, "ad-hoc review")?;
    let workflow = ReviewWorkflow::new(config_repo, &github, Some(shell), options);
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

fn github_client_for_action(
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

fn select_repo_interactively(
    repo_manager: &RepoManager<'_>,
    prompt: &str,
) -> Result<Option<String>> {
    let repos = repo_manager.list_repos()?;
    if repos.is_empty() {
        return Ok(None);
    }

    println!("{prompt}");
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
    Ok(Some(repos[selected - 1].full_name.clone()))
}

fn parse_repo_agent_args(args: Vec<String>) -> Result<(Option<String>, String)> {
    match args.len() {
        1 => Ok((None, args[0].clone())),
        2 => Ok((Some(args[0].clone()), args[1].clone())),
        _ => anyhow::bail!("agent command expects: [owner/repo] <agent>"),
    }
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

fn resolve_engine_specs(engines: Vec<String>) -> Result<Vec<EngineSpec>> {
    if engines.is_empty() {
        anyhow::bail!("missing engine command: set --engine <fingerprint> <command>");
    }
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
    Ok(out)
}

#[derive(Debug, Deserialize, Default)]
struct UiTokenQuery {
    token: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct UiActionForm {
    token: Option<String>,
    pr_url: Option<String>,
    repo: Option<String>,
    agent: Option<String>,
    dir: Option<String>,
}

async fn run_ui_server(addr: SocketAddr, state: UiState) -> Result<()> {
    let app = Router::new()
        .route("/", get(ui_index))
        .route("/api/trigger-review", post(ui_trigger_review))
        .route("/api/skip", post(ui_skip))
        .route("/api/adhoc", post(ui_adhoc))
        .route("/api/repo/add", post(ui_repo_add))
        .route("/api/repo/remove", post(ui_repo_remove))
        .route("/api/agent/add", post(ui_agent_add))
        .route("/api/agent/remove", post(ui_agent_remove))
        .route("/api/dir/add", post(ui_dir_add))
        .route("/api/dir/remove", post(ui_dir_remove))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn ui_index(
    State(state): State<UiState>,
    Query(q): Query<UiTokenQuery>,
) -> impl IntoResponse {
    if !authorized(&state.token, q.token.as_deref()) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }

    let cfg = match state.config_repo.load_config() {
        Ok(v) => v,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("config load failed: {err:#}"),
            )
                .into_response();
        }
    };
    let log = state.status_log.lock().await.clone();
    let token = q.token.unwrap_or_default();
    let token_hidden = if token.is_empty() {
        String::new()
    } else {
        format!(
            "<input type=\"hidden\" name=\"token\" value=\"{}\" />",
            html_escape(&token)
        )
    };

    let repos_html = if cfg.repos.is_empty() {
        "<li>(no repos)</li>".to_string()
    } else {
        cfg.repos
            .iter()
            .map(|r| {
                let agents = if r.agents.is_empty() {
                    "-".to_string()
                } else {
                    r.agents.join(", ")
                };
                format!(
                    "<li><b>{}</b> | agents={} | last_sha={}</li>",
                    html_escape(&r.full_name),
                    html_escape(&agents),
                    html_escape(r.last_sha.as_deref().unwrap_or("-"))
                )
            })
            .collect::<Vec<_>>()
            .join("")
    };
    let dirs_html = if cfg.agent_prompt_dirs.is_empty() {
        "<li>(no global dirs)</li>".to_string()
    } else {
        cfg.agent_prompt_dirs
            .iter()
            .map(|d| format!("<li>{}</li>", html_escape(d)))
            .collect::<Vec<_>>()
            .join("")
    };
    let logs_html = if log.is_empty() {
        "<li>(no status yet)</li>".to_string()
    } else {
        log.iter()
            .rev()
            .take(30)
            .map(|s| format!("<li>{}</li>", html_escape(s)))
            .collect::<Vec<_>>()
            .join("")
    };

    let page = format!(
        "<html><body>\
<h1>PrismFlow UI</h1>\
<p>Token protected: {}</p>\
<h2>Actions</h2>\
<form method=\"post\" action=\"/api/trigger-review\">{}<button type=\"submit\">Trigger Review Now</button></form>\
<form method=\"post\" action=\"/api/skip\">{}<button type=\"submit\">Skip Next PR</button></form>\
<form method=\"post\" action=\"/api/adhoc\">{}<input name=\"pr_url\" placeholder=\"https://github.com/owner/repo/pull/123\" size=\"70\"/><button type=\"submit\">Run Ad-hoc</button></form>\
<h2>Repo Management</h2>\
<form method=\"post\" action=\"/api/repo/add\">{}<input name=\"repo\" placeholder=\"owner/repo or github url\" size=\"70\"/><button type=\"submit\">Repo Add</button></form>\
<form method=\"post\" action=\"/api/repo/remove\">{}<input name=\"repo\" placeholder=\"owner/repo\" size=\"40\"/><button type=\"submit\">Repo Remove</button></form>\
<h2>Agent Management</h2>\
<form method=\"post\" action=\"/api/agent/add\">{}<input name=\"repo\" placeholder=\"owner/repo\" size=\"35\"/><input name=\"agent\" placeholder=\"agent\" size=\"20\"/><button type=\"submit\">Agent Add</button></form>\
<form method=\"post\" action=\"/api/agent/remove\">{}<input name=\"repo\" placeholder=\"owner/repo\" size=\"35\"/><input name=\"agent\" placeholder=\"agent\" size=\"20\"/><button type=\"submit\">Agent Remove</button></form>\
<h2>Global Agent Dirs</h2>\
<form method=\"post\" action=\"/api/dir/add\">{}<input name=\"dir\" placeholder=\"path/to/prompts\" size=\"60\"/><button type=\"submit\">Dir Add</button></form>\
<form method=\"post\" action=\"/api/dir/remove\">{}<input name=\"dir\" placeholder=\"path/to/prompts\" size=\"60\"/><button type=\"submit\">Dir Remove</button></form>\
<h3>Configured Global Dirs</h3><ul>{}</ul>\
<h3>Repos</h3><ul>{}</ul>\
<h3>Recent Status</h3><ul>{}</ul>\
<p><a href=\"/?token={}\">Refresh</a></p>\
</body></html>",
        if state.token.is_some() { "yes" } else { "no" },
        token_hidden,
        token_hidden,
        token_hidden,
        token_hidden,
        token_hidden,
        token_hidden,
        token_hidden,
        token_hidden,
        token_hidden,
        dirs_html,
        repos_html,
        logs_html,
        html_escape(&token)
    );
    Html(page).into_response()
}

async fn ui_trigger_review(
    State(state): State<UiState>,
    Form(form): Form<UiActionForm>,
) -> impl IntoResponse {
    if !authorized(&state.token, form.token.as_deref()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let _ = state.command_tx.send(UiCommand::TriggerReviewNow);
    state.notify.notify_one();
    ui_redirect(&state.token, form.token.as_deref()).into_response()
}

async fn ui_skip(
    State(state): State<UiState>,
    Form(form): Form<UiActionForm>,
) -> impl IntoResponse {
    if !authorized(&state.token, form.token.as_deref()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let _ = state.command_tx.send(UiCommand::TriggerSkip);
    state.notify.notify_one();
    ui_redirect(&state.token, form.token.as_deref()).into_response()
}

async fn ui_adhoc(
    State(state): State<UiState>,
    Form(form): Form<UiActionForm>,
) -> impl IntoResponse {
    if !authorized(&state.token, form.token.as_deref()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if let Some(pr_url) = form.pr_url.filter(|v| !v.trim().is_empty()) {
        let _ = state.command_tx.send(UiCommand::AdHoc(pr_url));
        state.notify.notify_one();
    }
    ui_redirect(&state.token, form.token.as_deref()).into_response()
}

async fn ui_repo_add(
    State(state): State<UiState>,
    Form(form): Form<UiActionForm>,
) -> impl IntoResponse {
    if !authorized(&state.token, form.token.as_deref()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if let Some(repo) = form.repo.filter(|v| !v.trim().is_empty()) {
        let _ = state.command_tx.send(UiCommand::RepoAdd(repo));
        state.notify.notify_one();
    }
    ui_redirect(&state.token, form.token.as_deref()).into_response()
}

async fn ui_repo_remove(
    State(state): State<UiState>,
    Form(form): Form<UiActionForm>,
) -> impl IntoResponse {
    if !authorized(&state.token, form.token.as_deref()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if let Some(repo) = form.repo.filter(|v| !v.trim().is_empty()) {
        let _ = state.command_tx.send(UiCommand::RepoRemove(repo));
        state.notify.notify_one();
    }
    ui_redirect(&state.token, form.token.as_deref()).into_response()
}

async fn ui_agent_add(
    State(state): State<UiState>,
    Form(form): Form<UiActionForm>,
) -> impl IntoResponse {
    if !authorized(&state.token, form.token.as_deref()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if let (Some(repo), Some(agent)) = (form.repo, form.agent) {
        if !repo.trim().is_empty() && !agent.trim().is_empty() {
            let _ = state.command_tx.send(UiCommand::AgentAdd { repo, agent });
            state.notify.notify_one();
        }
    }
    ui_redirect(&state.token, form.token.as_deref()).into_response()
}

async fn ui_agent_remove(
    State(state): State<UiState>,
    Form(form): Form<UiActionForm>,
) -> impl IntoResponse {
    if !authorized(&state.token, form.token.as_deref()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if let (Some(repo), Some(agent)) = (form.repo, form.agent) {
        if !repo.trim().is_empty() && !agent.trim().is_empty() {
            let _ = state
                .command_tx
                .send(UiCommand::AgentRemove { repo, agent });
            state.notify.notify_one();
        }
    }
    ui_redirect(&state.token, form.token.as_deref()).into_response()
}

async fn ui_dir_add(
    State(state): State<UiState>,
    Form(form): Form<UiActionForm>,
) -> impl IntoResponse {
    if !authorized(&state.token, form.token.as_deref()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if let Some(dir) = form.dir.filter(|v| !v.trim().is_empty()) {
        let _ = state.command_tx.send(UiCommand::DirAdd(dir));
        state.notify.notify_one();
    }
    ui_redirect(&state.token, form.token.as_deref()).into_response()
}

async fn ui_dir_remove(
    State(state): State<UiState>,
    Form(form): Form<UiActionForm>,
) -> impl IntoResponse {
    if !authorized(&state.token, form.token.as_deref()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if let Some(dir) = form.dir.filter(|v| !v.trim().is_empty()) {
        let _ = state.command_tx.send(UiCommand::DirRemove(dir));
        state.notify.notify_one();
    }
    ui_redirect(&state.token, form.token.as_deref()).into_response()
}

fn ui_redirect(expected: &Option<String>, provided: Option<&str>) -> Redirect {
    if expected.is_some() {
        let token = provided.unwrap_or_default();
        Redirect::to(&format!("/?token={token}"))
    } else {
        Redirect::to("/")
    }
}

fn authorized(expected: &Option<String>, provided: Option<&str>) -> bool {
    match expected {
        Some(v) => provided.map(|s| s == v).unwrap_or(false),
        None => true,
    }
}

fn html_escape(v: &str) -> String {
    v.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\"', "&quot;")
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
