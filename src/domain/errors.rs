use thiserror::Error;

#[derive(Debug, Error)]
pub enum DomainError {
    #[error("invalid repository format, expected owner/repo: {0}")]
    InvalidRepoFormat(String),
}