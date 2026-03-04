use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    pub repos: Vec<MonitoredRepo>,
    #[serde(default)]
    pub agent_prompt_dirs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitoredRepo {
    pub full_name: String,
    pub added_at: String,
    pub last_sha: Option<String>,
    #[serde(default)]
    pub review_filter: ReviewFilterConfig,
    #[serde(default)]
    pub agents: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewFilterConfig {
    #[serde(default)]
    pub exclude_prefixes: Vec<String>,
    #[serde(default)]
    pub exclude_files: Vec<String>,
    #[serde(default)]
    pub exclude_extensions: Vec<String>,
    #[serde(default = "default_true")]
    pub skip_binary_without_patch: bool,
}

impl Default for ReviewFilterConfig {
    fn default() -> Self {
        Self {
            exclude_prefixes: vec![
                "vendor/".to_string(),
                "node_modules/".to_string(),
                "dist/".to_string(),
                "build/".to_string(),
                ".github/".to_string(),
                ".vscode/".to_string(),
            ],
            exclude_files: vec![
                "package-lock.json".to_string(),
                "pnpm-lock.yaml".to_string(),
            ],
            exclude_extensions: vec![".db".to_string(), ".lock".to_string()],
            skip_binary_without_patch: true,
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone)]
pub struct PullRequestSummary {
    pub number: u64,
    pub title: String,
    pub head_sha: String,
    pub html_url: Option<String>,
    pub author_login: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CiFailure {
    pub source: String,
    pub name: String,
    pub conclusion: String,
    pub details_url: Option<String>,
    pub summary: Option<String>,
    pub text: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PullRequestCiSnapshot {
    pub head_sha: String,
    pub failures: Vec<CiFailure>,
}

#[derive(Debug, Clone)]
pub struct PullRequestGitContext {
    pub head_sha: String,
    pub head_ref: String,
    pub head_clone_url: String,
}

#[derive(Debug, Clone)]
pub struct PullRequestFilePatch {
    pub path: String,
    pub patch: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SimpleComment {
    pub id: u64,
    pub body: String,
    pub author_login: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SimplePullReview {
    pub id: u64,
    pub body: String,
    pub state: String,
    pub author_login: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ReviewComment {
    pub path: String,
    pub line: u32,
    pub body: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum ReviewDecision {
    Approve,
    Comment,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ReviewResult {
    pub decision: ReviewDecision,
    pub summary: String,
    pub comments: Vec<ReviewComment>,
}
