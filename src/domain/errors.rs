use thiserror::Error;

#[derive(Debug, Error)]
#[allow(dead_code)]
pub enum DomainError {
    #[error("invalid repository format, expected owner/repo: {0}")]
    InvalidRepoFormat(String),
    #[error("operation cancelled by signal")]
    CancelledBySignal,
    #[error("operation cancelled by operator")]
    CancelledByOperator,
    #[error("child process was killed")]
    ChildProcessKilled,
}
