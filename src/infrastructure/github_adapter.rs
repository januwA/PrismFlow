use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use octocrab::params::State;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Semaphore;

use crate::domain::{
    entities::{
        CiFailure, PullRequestCiSnapshot, PullRequestFilePatch, PullRequestGitContext,
        PullRequestMetrics, PullRequestSummary, ReviewComment, SimpleComment, SimplePullReview,
    },
    ports::GitHubRepository,
};

#[derive(Clone)]
pub struct OctocrabGitHubRepository {
    client: octocrab::Octocrab,
    api_semaphore: Arc<Semaphore>,
}

impl OctocrabGitHubRepository {
    pub fn new(token: String, max_concurrent_api: usize) -> Result<Self> {
        let client = octocrab::Octocrab::builder()
            .personal_token(token)
            .build()?;
        Ok(Self {
            client,
            api_semaphore: Arc::new(Semaphore::new(max_concurrent_api.max(1))),
        })
    }

    async fn acquire_api_permit(&self) -> Result<tokio::sync::OwnedSemaphorePermit> {
        self.api_semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| anyhow::anyhow!("api semaphore closed: {e}"))
    }
}

#[async_trait]
impl GitHubRepository for OctocrabGitHubRepository {
    async fn current_user_login(&self) -> Result<String> {
        #[derive(Debug, Deserialize)]
        struct UserDto {
            login: String,
        }
        let _permit = self.acquire_api_permit().await?;
        let me: UserDto = self.client.get("/user", None::<&()>).await?;
        Ok(me.login)
    }

    async fn list_open_pull_requests(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<Vec<PullRequestSummary>> {
        let _permit = self.acquire_api_permit().await?;
        let page = self
            .client
            .pulls(owner, repo)
            .list()
            .state(State::Open)
            .send()
            .await?;

        let items = page
            .items
            .into_iter()
            .map(|pr| PullRequestSummary {
                number: pr.number,
                title: pr.title.unwrap_or_else(|| "(no title)".to_string()),
                head_sha: pr.head.sha,
                html_url: pr.html_url.map(|url| url.to_string()),
            })
            .collect();

        Ok(items)
    }

    async fn get_pull_request(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
    ) -> Result<PullRequestSummary> {
        let _permit = self.acquire_api_permit().await?;
        let pr = self.client.pulls(owner, repo).get(pull_number).await?;
        Ok(PullRequestSummary {
            number: pr.number,
            title: pr.title.unwrap_or_else(|| "(no title)".to_string()),
            head_sha: pr.head.sha,
            html_url: pr.html_url.map(|u| u.to_string()),
        })
    }

    async fn get_pull_request_git_context(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
    ) -> Result<PullRequestGitContext> {
        #[derive(Debug, Deserialize)]
        struct PullDto {
            head: HeadDto,
        }
        #[derive(Debug, Deserialize)]
        struct HeadDto {
            sha: String,
            #[serde(rename = "ref")]
            head_ref: String,
            repo: Option<HeadRepoDto>,
        }
        #[derive(Debug, Deserialize)]
        struct HeadRepoDto {
            clone_url: String,
        }

        let _permit = self.acquire_api_permit().await?;
        let route = format!("/repos/{owner}/{repo}/pulls/{pull_number}");
        let pr: PullDto = self.client.get(route, None::<&()>).await?;
        let clone_url = pr
            .head
            .repo
            .map(|r| r.clone_url)
            .unwrap_or_else(|| format!("https://github.com/{owner}/{repo}.git"));
        Ok(PullRequestGitContext {
            head_sha: pr.head.sha,
            head_ref: pr.head.head_ref,
            head_clone_url: clone_url,
        })
    }

    async fn get_pull_request_metrics(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
    ) -> Result<PullRequestMetrics> {
        #[derive(Debug, Deserialize)]
        struct PullDto {
            changed_files: Option<u64>,
            additions: Option<u64>,
            deletions: Option<u64>,
        }

        let _permit = self.acquire_api_permit().await?;
        let route = format!("/repos/{owner}/{repo}/pulls/{pull_number}");
        let pr: PullDto = self.client.get(route, None::<&()>).await?;
        Ok(PullRequestMetrics {
            changed_files: pr.changed_files.unwrap_or(0),
            additions: pr.additions.unwrap_or(0),
            deletions: pr.deletions.unwrap_or(0),
        })
    }

    async fn get_pull_request_ci_snapshot(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
    ) -> Result<PullRequestCiSnapshot> {
        #[derive(Debug, Deserialize)]
        struct PullDto {
            head: HeadDto,
        }
        #[derive(Debug, Deserialize)]
        struct HeadDto {
            sha: String,
        }
        #[derive(Debug, Deserialize)]
        struct CheckRunsResp {
            check_runs: Vec<CheckRunDto>,
        }
        #[derive(Debug, Deserialize)]
        struct CheckRunDto {
            name: String,
            status: Option<String>,
            conclusion: Option<String>,
            details_url: Option<String>,
            output: Option<CheckOutputDto>,
        }
        #[derive(Debug, Deserialize)]
        struct CheckOutputDto {
            summary: Option<String>,
            text: Option<String>,
        }
        #[derive(Debug, Deserialize)]
        struct CommitStatusResp {
            statuses: Vec<CommitStatusDto>,
        }
        #[derive(Debug, Deserialize)]
        struct CommitStatusDto {
            context: Option<String>,
            state: Option<String>,
            target_url: Option<String>,
            description: Option<String>,
        }

        let _permit = self.acquire_api_permit().await?;
        let pr_route = format!("/repos/{owner}/{repo}/pulls/{pull_number}");
        let pr: PullDto = self.client.get(pr_route, None::<&()>).await?;
        let head_sha = pr.head.sha;

        let checks_route = format!("/repos/{owner}/{repo}/commits/{head_sha}/check-runs");
        let checks: CheckRunsResp = self.client.get(checks_route, None::<&()>).await?;
        let status_route = format!("/repos/{owner}/{repo}/commits/{head_sha}/status");
        let statuses: CommitStatusResp = self.client.get(status_route, None::<&()>).await?;

        let mut failures = Vec::<CiFailure>::new();
        for check in checks.check_runs {
            let conclusion = check
                .conclusion
                .clone()
                .or(check.status.clone())
                .unwrap_or_else(|| "unknown".to_string());
            let lower = conclusion.to_ascii_lowercase();
            if !matches!(
                lower.as_str(),
                "failure" | "failed" | "timed_out" | "cancelled" | "action_required" | "stale"
            ) {
                continue;
            }
            failures.push(CiFailure {
                source: "check-run".to_string(),
                name: check.name,
                conclusion,
                details_url: check.details_url,
                summary: check.output.as_ref().and_then(|o| o.summary.clone()),
                text: check.output.as_ref().and_then(|o| o.text.clone()),
            });
        }

        for status in statuses.statuses {
            let state = status.state.unwrap_or_else(|| "unknown".to_string());
            let lower = state.to_ascii_lowercase();
            if !matches!(lower.as_str(), "failure" | "error") {
                continue;
            }
            failures.push(CiFailure {
                source: "commit-status".to_string(),
                name: status.context.unwrap_or_else(|| "(no context)".to_string()),
                conclusion: state,
                details_url: status.target_url,
                summary: status.description,
                text: None,
            });
        }

        Ok(PullRequestCiSnapshot { head_sha, failures })
    }

    async fn list_pull_request_files(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
    ) -> Result<Vec<PullRequestFilePatch>> {
        let _permit = self.acquire_api_permit().await?;
        let page = self
            .client
            .pulls(owner, repo)
            .list_files(pull_number)
            .await?;

        let files = page
            .items
            .into_iter()
            .map(|f| PullRequestFilePatch {
                path: f.filename,
                patch: f.patch,
            })
            .collect();

        Ok(files)
    }

    async fn list_issue_comment_bodies(
        &self,
        owner: &str,
        repo: &str,
        issue_number: u64,
    ) -> Result<Vec<String>> {
        let _permit = self.acquire_api_permit().await?;
        let page = self
            .client
            .issues(owner, repo)
            .list_comments(issue_number)
            .send()
            .await?;

        let bodies = page
            .items
            .into_iter()
            .map(|c| c.body.unwrap_or_default())
            .collect::<Vec<_>>();

        Ok(bodies)
    }

    async fn list_issue_comments(
        &self,
        owner: &str,
        repo: &str,
        issue_number: u64,
    ) -> Result<Vec<SimpleComment>> {
        #[derive(Debug, Deserialize)]
        struct CommentDto {
            id: u64,
            body: Option<String>,
            user: Option<UserDto>,
        }
        #[derive(Debug, Deserialize)]
        struct UserDto {
            login: String,
        }

        let _permit = self.acquire_api_permit().await?;
        let route = format!("/repos/{owner}/{repo}/issues/{issue_number}/comments");
        let items: Vec<CommentDto> = self.client.get(route, None::<&()>).await?;
        Ok(items
            .into_iter()
            .map(|i| SimpleComment {
                id: i.id,
                body: i.body.unwrap_or_default(),
                author_login: i.user.map(|u| u.login),
            })
            .collect())
    }

    async fn create_issue_comment(
        &self,
        owner: &str,
        repo: &str,
        issue_number: u64,
        body: &str,
    ) -> Result<()> {
        let _permit = self.acquire_api_permit().await?;
        self.client
            .issues(owner, repo)
            .create_comment(issue_number, body)
            .await?;
        Ok(())
    }

    async fn submit_inline_review(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
        body: &str,
        comments: &[ReviewComment],
    ) -> Result<()> {
        let _permit = self.acquire_api_permit().await?;
        let route = format!("/repos/{owner}/{repo}/pulls/{pull_number}/reviews");
        let comments = comments
            .iter()
            .map(|c| {
                json!({
                    "path": c.path,
                    "line": c.line,
                    "side": "RIGHT",
                    "body": c.body,
                })
            })
            .collect::<Vec<_>>();

        let payload = json!({
            "body": body,
            "event": "COMMENT",
            "comments": comments,
        });

        let _: serde_json::Value = self.client.post(route, Some(&payload)).await?;
        Ok(())
    }

    async fn list_issue_labels(
        &self,
        owner: &str,
        repo: &str,
        issue_number: u64,
    ) -> Result<Vec<String>> {
        #[derive(Debug, Deserialize)]
        struct LabelDto {
            name: String,
        }

        let _permit = self.acquire_api_permit().await?;
        let route = format!("/repos/{owner}/{repo}/issues/{issue_number}/labels");
        let labels: Vec<LabelDto> = self.client.get(route, None::<&()>).await?;
        Ok(labels.into_iter().map(|l| l.name).collect())
    }

    async fn add_issue_labels(
        &self,
        owner: &str,
        repo: &str,
        issue_number: u64,
        labels: &[String],
    ) -> Result<()> {
        if labels.is_empty() {
            return Ok(());
        }
        let _permit = self.acquire_api_permit().await?;
        let route = format!("/repos/{owner}/{repo}/issues/{issue_number}/labels");
        let payload = json!({ "labels": labels });
        let _: serde_json::Value = self.client.post(route, Some(&payload)).await?;
        Ok(())
    }

    async fn remove_issue_label(
        &self,
        owner: &str,
        repo: &str,
        issue_number: u64,
        label: &str,
    ) -> Result<()> {
        let _permit = self.acquire_api_permit().await?;
        let route = format!("/repos/{owner}/{repo}/issues/{issue_number}/labels/{label}");
        let _: serde_json::Value = self.client.delete(route, None::<&()>).await?;
        Ok(())
    }

    async fn list_pull_review_comments(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
    ) -> Result<Vec<SimpleComment>> {
        #[derive(Debug, Deserialize)]
        struct CommentDto {
            id: u64,
            body: Option<String>,
            user: Option<UserDto>,
        }
        #[derive(Debug, Deserialize)]
        struct UserDto {
            login: String,
        }
        let _permit = self.acquire_api_permit().await?;
        let route = format!("/repos/{owner}/{repo}/pulls/{pull_number}/comments");
        let items: Vec<CommentDto> = self.client.get(route, None::<&()>).await?;
        Ok(items
            .into_iter()
            .map(|i| SimpleComment {
                id: i.id,
                body: i.body.unwrap_or_default(),
                author_login: i.user.map(|u| u.login),
            })
            .collect())
    }

    async fn list_pull_reviews(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
    ) -> Result<Vec<SimplePullReview>> {
        #[derive(Debug, Deserialize)]
        struct ReviewDto {
            id: u64,
            body: Option<String>,
            state: Option<String>,
            user: Option<UserDto>,
        }
        #[derive(Debug, Deserialize)]
        struct UserDto {
            login: String,
        }
        let _permit = self.acquire_api_permit().await?;
        let route = format!("/repos/{owner}/{repo}/pulls/{pull_number}/reviews");
        let items: Vec<ReviewDto> = self.client.get(route, None::<&()>).await?;
        Ok(items
            .into_iter()
            .map(|i| SimplePullReview {
                id: i.id,
                body: i.body.unwrap_or_default(),
                state: i.state.unwrap_or_default(),
                author_login: i.user.map(|u| u.login),
            })
            .collect())
    }

    async fn delete_issue_comment(&self, owner: &str, repo: &str, comment_id: u64) -> Result<()> {
        let _permit = self.acquire_api_permit().await?;
        let route = format!("/repos/{owner}/{repo}/issues/comments/{comment_id}");
        let _: serde_json::Value = self.client.delete(route, None::<&()>).await?;
        Ok(())
    }

    async fn delete_pull_review_comment(
        &self,
        owner: &str,
        repo: &str,
        comment_id: u64,
    ) -> Result<()> {
        let _permit = self.acquire_api_permit().await?;
        let route = format!("/repos/{owner}/{repo}/pulls/comments/{comment_id}");
        let _: serde_json::Value = self.client.delete(route, None::<&()>).await?;
        Ok(())
    }

    async fn delete_pending_pull_review(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
        review_id: u64,
    ) -> Result<()> {
        let _permit = self.acquire_api_permit().await?;
        let route = format!("/repos/{owner}/{repo}/pulls/{pull_number}/reviews/{review_id}");
        let _: serde_json::Value = self.client.delete(route, None::<&()>).await?;
        Ok(())
    }

    async fn dismiss_pull_review(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
        review_id: u64,
        message: &str,
    ) -> Result<()> {
        let _permit = self.acquire_api_permit().await?;
        let route =
            format!("/repos/{owner}/{repo}/pulls/{pull_number}/reviews/{review_id}/dismissals");
        let payload = json!({ "message": message });
        let _: serde_json::Value = self.client.put(route, Some(&payload)).await?;
        Ok(())
    }
}
