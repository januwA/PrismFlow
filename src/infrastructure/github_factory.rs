use anyhow::Result;

use crate::domain::ports::{GitHubRepository, GitHubRepositoryFactory};

use super::github_adapter::OctocrabGitHubRepository;

#[derive(Debug, Default, Clone, Copy)]
pub struct OctocrabGitHubRepositoryFactory;

impl GitHubRepositoryFactory for OctocrabGitHubRepositoryFactory {
    fn create(
        &self,
        token: String,
        max_concurrent_api: usize,
    ) -> Result<Box<dyn GitHubRepository>> {
        Ok(Box::new(OctocrabGitHubRepository::new(
            token,
            max_concurrent_api,
        )?))
    }
}
