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
    #[serde(default = "default_exclude_prefixes")]
    pub exclude_prefixes: Vec<String>,
    #[serde(default)]
    pub include_prefixes: Vec<String>,
    #[serde(default)]
    pub include_files: Vec<String>,
    #[serde(default)]
    pub exclude_files: Vec<String>,
    #[serde(default)]
    pub include_extensions: Vec<String>,
    #[serde(default)]
    pub exclude_extensions: Vec<String>,
    #[serde(default = "default_true")]
    pub skip_binary_without_patch: bool,
}

impl Default for ReviewFilterConfig {
    fn default() -> Self {
        Self {
            exclude_prefixes: default_exclude_prefixes(),
            include_prefixes: vec![],
            include_files: vec![
                ".github/workflows/ci.yml".to_string(),
                ".github/workflows/release.yml".to_string(),
                "package-lock.json".to_string(),
                "pnpm-lock.yaml".to_string(),
                "yarn.lock".to_string(),
            ],
            exclude_files: vec![],
            include_extensions: vec![],
            exclude_extensions: vec![],
            skip_binary_without_patch: true,
        }
    }
}

fn default_exclude_prefixes() -> Vec<String> {
    vec![
        "vendor/".to_string(),
        "node_modules/".to_string(),
        "dist/".to_string(),
        "build/".to_string(),
    ]
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
