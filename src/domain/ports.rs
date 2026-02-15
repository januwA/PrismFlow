use std::path::Path;

use anyhow::Result;
use async_trait::async_trait;

use crate::domain::entities::{
    AppConfig, PullRequestFilePatch, PullRequestGitContext, PullRequestMetrics, PullRequestSummary,
    ReviewComment, SimpleComment, SimplePullReview,
};

pub trait ConfigRepository: Send + Sync {
    fn load_config(&self) -> Result<AppConfig>;
    fn save_config(&self, config: &AppConfig) -> Result<()>;
    fn config_path(&self) -> &Path;
}

pub trait TokenProvider: Send + Sync {
    fn source_name(&self) -> &'static str;
    fn token(&self) -> Result<Option<String>>;
}

#[async_trait]
pub trait GitHubRepository: Send + Sync {
    async fn current_user_login(&self) -> Result<String>;
    async fn list_open_pull_requests(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<Vec<PullRequestSummary>>;
    async fn get_pull_request(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
    ) -> Result<PullRequestSummary>;
    async fn get_pull_request_git_context(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
    ) -> Result<PullRequestGitContext>;
    async fn get_pull_request_metrics(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
    ) -> Result<PullRequestMetrics>;
    async fn list_pull_request_files(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
    ) -> Result<Vec<PullRequestFilePatch>>;
    async fn list_issue_comment_bodies(
        &self,
        owner: &str,
        repo: &str,
        issue_number: u64,
    ) -> Result<Vec<String>>;
    async fn list_issue_comments(
        &self,
        owner: &str,
        repo: &str,
        issue_number: u64,
    ) -> Result<Vec<SimpleComment>>;
    async fn create_issue_comment(
        &self,
        owner: &str,
        repo: &str,
        issue_number: u64,
        body: &str,
    ) -> Result<()>;
    async fn submit_inline_review(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
        body: &str,
        comments: &[ReviewComment],
    ) -> Result<()>;
    async fn list_issue_labels(
        &self,
        owner: &str,
        repo: &str,
        issue_number: u64,
    ) -> Result<Vec<String>>;
    async fn add_issue_labels(
        &self,
        owner: &str,
        repo: &str,
        issue_number: u64,
        labels: &[String],
    ) -> Result<()>;
    async fn remove_issue_label(
        &self,
        owner: &str,
        repo: &str,
        issue_number: u64,
        label: &str,
    ) -> Result<()>;
    async fn list_pull_review_comments(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
    ) -> Result<Vec<SimpleComment>>;
    async fn list_pull_reviews(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
    ) -> Result<Vec<SimplePullReview>>;
    async fn delete_issue_comment(&self, owner: &str, repo: &str, comment_id: u64) -> Result<()>;
    async fn delete_pull_review_comment(
        &self,
        owner: &str,
        repo: &str,
        comment_id: u64,
    ) -> Result<()>;
    async fn delete_pending_pull_review(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
        review_id: u64,
    ) -> Result<()>;
    async fn dismiss_pull_review(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
        review_id: u64,
        message: &str,
    ) -> Result<()>;
}
