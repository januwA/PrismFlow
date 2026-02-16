mod application;
mod domain;
mod infrastructure;
mod interface;

use std::fs;
use std::sync::Arc;

use anyhow::{Context, Result};
use application::{
    auth_manager::AuthManager, repo_manager::RepoManager, review_workflow::EngineSpec,
};
use clap::Parser;
use domain::ports::ConfigRepository;
use infrastructure::{
    local_config_adapter::LocalConfigAdapter,
    process_manager::OsProcessManager,
    shell_adapter::CommandShellAdapter,
    token_providers::{EnvTokenProvider, GhCliTokenProvider, StoredTokenProvider},
};
use interface::cli::Cli;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    let shell_override = cli.shell.clone();
    let local_config_repo = Arc::new(LocalConfigAdapter::new()?);
    let config_repo: Arc<dyn ConfigRepository> = local_config_repo.clone();
    let shell = CommandShellAdapter::new(shell_override);
    let process_manager = OsProcessManager;
    let stored_provider = StoredTokenProvider::new(local_config_repo.auth_token_path());
    let gh_provider = GhCliTokenProvider::new(&shell);
    let env_provider = EnvTokenProvider;

    let auth_manager = AuthManager::new(
        vec![&gh_provider, &env_provider, &stored_provider],
        &stored_provider,
    );
    let repo_manager = RepoManager::new(config_repo.as_ref());

    interface::cli_handlers::dispatch(
        cli.command,
        &auth_manager,
        &repo_manager,
        config_repo.clone(),
        &shell,
        &process_manager,
    )
    .await?;

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

fn short_key(full: &str) -> &str {
    let end = full.len().min(12);
    &full[..end]
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}
