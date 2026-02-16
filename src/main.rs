mod application;
mod domain;
mod infrastructure;
mod interface;

use std::sync::Arc;

use anyhow::Result;
use application::{auth_manager::AuthManager, repo_manager::RepoManager};
use clap::Parser;
use domain::ports::ConfigRepository;
use infrastructure::{
    github_factory::OctocrabGitHubRepositoryFactory,
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
    let github_factory = OctocrabGitHubRepositoryFactory;
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
        &github_factory,
    )
    .await?;

    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}
