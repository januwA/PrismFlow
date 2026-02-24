use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::{Context, Result};
use tokio::sync::{Mutex, Notify, broadcast, mpsc};
use tokio::time::{Duration, sleep};

use crate::application::{
    auth_manager::AuthManager,
    ci_workflow::CiWorkflowOptions,
    context::TaskContext,
    repo_manager::RepoManager,
    review_workflow::{EngineSpec, ReviewWorkflow, ReviewWorkflowOptions},
    usecases::{run_ci_once, run_review_ad_hoc, run_review_clean, run_review_once},
};
use crate::domain::ports::{
    ConfigRepository, FileSystem, GitHubRepository, GitHubRepositoryFactory, GitService,
    ProcessManager, ShellAdapter,
};
use crate::interface::cli::{
    AuthSubcommand, CiSubcommand, Commands, RepoAgentSubcommand, RepoSubcommand, ReviewSubcommand,
    ScanSubcommand,
};
use crate::interface::web::{UiCommand, UiState, run_ui_server};

const SHUTDOWN_GRACE_SECS: u64 = 8;

pub async fn dispatch(
    command: Commands,
    auth_manager: &AuthManager<'_>,
    repo_manager: &RepoManager<'_>,
    config_repo: Arc<dyn ConfigRepository>,
    shell: &dyn ShellAdapter,
    process_manager: &dyn ProcessManager,
    github_factory: &dyn GitHubRepositoryFactory,
    fs: &dyn FileSystem,
    git: &dyn GitService,
) -> Result<()> {
    match command {
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

                let github = github_client_for_action(
                    auth_manager,
                    github_factory,
                    max_concurrent_api,
                    "scan",
                )?;
                let workflow = ReviewWorkflow::new(
                    config_repo.as_ref(),
                    github.as_ref(),
                    None,
                    fs,
                    git,
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
                let resolved_prompt = resolve_engine_prompt(fs, engine_prompt, engine_prompt_file)?;
                let resolved_engines = resolve_engine_specs(engines)?;
                let once_ctx = Arc::new(TaskContext::new("ci-once"));
                let github = github_client_for_action(
                    auth_manager,
                    github_factory,
                    max_concurrent_api,
                    "ci",
                )?;
                run_ci_once(
                    config_repo.as_ref(),
                    github.as_ref(),
                    shell,
                    fs,
                    git,
                    CiWorkflowOptions {
                        max_concurrent_repos,
                        engine_specs: resolved_engines,
                        engine_prompt: resolved_prompt,
                        clone_repo_enabled: clone_repo,
                        clone_workspace_dir,
                        clone_depth,
                        include_repos: repos,
                        exclude_repos,
                        task_context: None,
                    },
                    once_ctx,
                )
                .await?;
            }
            CiSubcommand::Daemon {
                interval_secs,
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
                let resolved_prompt = resolve_engine_prompt(fs, engine_prompt, engine_prompt_file)?;
                let options = CiWorkflowOptions {
                    max_concurrent_repos,
                    engine_specs: resolve_engine_specs(engines)?,
                    engine_prompt: resolved_prompt,
                    clone_repo_enabled: clone_repo,
                    clone_workspace_dir,
                    clone_depth,
                    include_repos: repos,
                    exclude_repos,
                    task_context: None,
                };
                println!("ci daemon started: interval={}s", interval_secs);
                loop {
                    let cycle_ctx = Arc::new(TaskContext::new("ci-daemon-cycle"));
                    let github = github_client_for_action(
                        auth_manager,
                        github_factory,
                        max_concurrent_api,
                        "ci",
                    )?;
                    let mut stop_daemon = false;
                    tokio::select! {
                        r = run_ci_once(
                            config_repo.as_ref(),
                            github.as_ref(),
                            shell,
                            fs,
                            git,
                            options.clone(),
                            cycle_ctx.clone(),
                        ) => {
                            if let Err(err) = r {
                                eprintln!("ci cycle failed: {err:#}");
                            }
                        }
                        _ = tokio::signal::ctrl_c() => {
                            stop_daemon = true;
                            shutdown_cycle(
                                "ci",
                                &cycle_ctx,
                                SHUTDOWN_GRACE_SECS,
                                process_manager,
                                None,
                            )
                            .await;
                        }
                    }
                    if stop_daemon {
                        println!("received Ctrl+C, exiting ci daemon");
                        break;
                    }
                    tokio::select! {
                        _ = sleep(Duration::from_secs(interval_secs)) => {}
                        _ = tokio::signal::ctrl_c() => {
                            println!("received Ctrl+C, exiting ci daemon");
                            break;
                        }
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
                authors,
                exclude_authors,
            } => {
                let resolved_prompt = resolve_engine_prompt(fs, engine_prompt, engine_prompt_file)?;
                let resolved_engines = resolve_engine_specs(engines)?;
                let once_ctx = Arc::new(TaskContext::new("review-once"));
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
                    include_authors: authors,
                    exclude_authors,
                    ..ReviewWorkflowOptions::default()
                };
                let github = github_client_for_action(
                    auth_manager,
                    github_factory,
                    max_concurrent_api,
                    "review",
                )?;
                run_review_once(
                    config_repo.as_ref(),
                    github.as_ref(),
                    shell,
                    fs,
                    git,
                    options,
                    once_ctx,
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
                authors,
                exclude_authors,
            } => {
                let resolved_prompt = resolve_engine_prompt(fs, engine_prompt, engine_prompt_file)?;
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
                    include_authors: authors,
                    exclude_authors,
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
                                        let adhoc_ctx = Arc::new(TaskContext::new("review-adhoc"));
                                        let github = github_client_for_action(
                                            auth_manager,
                                            github_factory,
                                            max_concurrent_api,
                                            "ad-hoc review",
                                        );
                                        let github = match github {
                                            Ok(client) => client,
                                            Err(err) => {
                                                let _ = status_tx
                                                    .send(format!("ui:adhoc failed error={err:#}"));
                                                continue;
                                            }
                                        };
                                        if let Err(err) = run_review_ad_hoc(
                                            config_repo.as_ref(),
                                            github.as_ref(),
                                            shell,
                                            fs,
                                            git,
                                            owner,
                                            repo,
                                            pr_number,
                                            adhoc_opts,
                                            adhoc_ctx,
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
                    let cycle_ctx = Arc::new(TaskContext::new("review-daemon-cycle"));
                    let github = github_client_for_action(
                        auth_manager,
                        github_factory,
                        max_concurrent_api,
                        "review",
                    )?;
                    let mut stop_daemon = false;
                    tokio::select! {
                        r = run_review_once(
                            config_repo.as_ref(),
                            github.as_ref(),
                            shell,
                            fs,
                            git,
                            ReviewWorkflowOptions {
                                status_tx: Some(status_tx.clone()),
                                skip_flag: Some(skip_flag.clone()),
                                ..options.clone()
                            },
                            cycle_ctx.clone(),
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
                            stop_daemon = true;
                            shutdown_cycle(
                                "review",
                                &cycle_ctx,
                                SHUTDOWN_GRACE_SECS,
                                process_manager,
                                Some(&status_tx),
                            )
                            .await;
                        }
                    }
                    if stop_daemon {
                        println!("received Ctrl+C, exiting daemon");
                        break;
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
                let resolved_prompt = resolve_engine_prompt(fs, engine_prompt, engine_prompt_file)?;
                let resolved_engines = resolve_engine_specs(engines)?;
                let adhoc_ctx = Arc::new(TaskContext::new("review-adhoc"));
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
                let github = github_client_for_action(
                    auth_manager,
                    github_factory,
                    max_concurrent_api,
                    "ad-hoc review",
                )?;
                run_review_ad_hoc(
                    config_repo.as_ref(),
                    github.as_ref(),
                    shell,
                    fs,
                    git,
                    owner,
                    repo,
                    pr_number,
                    options,
                    adhoc_ctx,
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
                            let clean_ctx = Arc::new(TaskContext::new("review-clean"));
                            let github = github_client_for_action(
                                auth_manager,
                                github_factory,
                                max_concurrent_api,
                                "clean",
                            );
                            let github = match github {
                                Ok(client) => client,
                                Err(err) => {
                                    failed += 1;
                                    eprintln!("clean_failed url={} error={err:#}", pr_url);
                                    continue;
                                }
                            };
                            if let Err(err) =
                                run_review_clean(github.as_ref(), owner, repo, pr_number, clean_ctx)
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
    fs: &dyn FileSystem,
    engine_prompt: Option<String>,
    engine_prompt_file: Option<String>,
) -> Result<Option<String>> {
    if engine_prompt.is_some() && engine_prompt_file.is_some() {
        anyhow::bail!("--engine-prompt and --engine-prompt-file cannot be used together");
    }

    if let Some(file) = engine_prompt_file {
        let content = fs
            .read_to_string(&PathBuf::from(&file))
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

fn short_key(full: &str) -> &str {
    let end = full.len().min(12);
    &full[..end]
}

fn github_client_for_action(
    auth_manager: &AuthManager<'_>,
    github_factory: &dyn GitHubRepositoryFactory,
    max_concurrent_api: usize,
    action: &str,
) -> Result<Box<dyn GitHubRepository>> {
    let token = auth_manager
        .resolve_token()?
        .map(|r| r.token)
        .with_context(|| {
            format!(
                "token required for {action}; run `prismflow auth login <token>` or configure gh/GITHUB_TOKEN"
            )
        })?;
    github_factory.create(token, max_concurrent_api)
}

async fn shutdown_cycle(
    kind: &str,
    ctx: &Arc<TaskContext>,
    grace_secs: u64,
    process_manager: &dyn ProcessManager,
    status_tx: Option<&broadcast::Sender<String>>,
) {
    ctx.cancel();
    if let Some(tx) = status_tx {
        let _ = tx.send(format!(
            "cycle:cancel_requested kind={} run_id={} cancel_reason=signal",
            kind,
            ctx.run_id()
        ));
    }
    let initial_children = ctx.child_count().await;
    let child_snapshot = ctx.list_children().await;
    println!(
        "shutdown:{} graceful_begin run_id={} in_flight_children={} cancel_reason=signal",
        kind,
        ctx.run_id(),
        initial_children
    );
    for (pid, label) in child_snapshot {
        println!("shutdown:{} child_pid={} {}", kind, pid, label);
    }
    if let Some(tx) = status_tx {
        let _ = tx.send(format!(
            "cycle:graceful_shutdown_begin kind={} run_id={} in_flight_children={} cancel_reason=signal",
            kind,
            ctx.run_id(),
            initial_children
        ));
    }
    if initial_children == 0 {
        println!("shutdown:{} graceful_end in_flight_children=0", kind);
        if let Some(tx) = status_tx {
            let _ = tx.send(format!(
                "cycle:graceful_shutdown_end kind={} run_id={} in_flight_children=0",
                kind,
                ctx.run_id()
            ));
        }
        return;
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(grace_secs);
    loop {
        let remaining_children = ctx.child_count().await;
        if remaining_children == 0 {
            println!("shutdown:{} graceful_end in_flight_children=0", kind);
            if let Some(tx) = status_tx {
                let _ = tx.send(format!(
                    "cycle:graceful_shutdown_end kind={} run_id={} in_flight_children=0",
                    kind,
                    ctx.run_id()
                ));
            }
            return;
        }

        if tokio::time::Instant::now() >= deadline {
            break;
        }

        tokio::select! {
            _ = sleep(Duration::from_millis(200)) => {}
            _ = tokio::signal::ctrl_c() => {
                println!(
                    "shutdown:{} second_ctrl_c immediate_force_kill cancel_reason=signal_second",
                    kind
                );
                break;
            }
        }
    }

    println!("shutdown:{} force_kill_begin", kind);
    if let Some(tx) = status_tx {
        let _ = tx.send(format!(
            "cycle:force_kill_begin kind={} run_id={} cancel_reason=signal",
            kind,
            ctx.run_id()
        ));
    }
    let killed = ctx.kill_all_children(process_manager).await;
    println!(
        "shutdown:{} force_kill_end killed_children={}",
        kind, killed
    );
    if let Some(tx) = status_tx {
        let _ = tx.send(format!(
            "cycle:force_kill_end kind={} run_id={} killed_children={}",
            kind,
            ctx.run_id(),
            killed
        ));
    }
}
