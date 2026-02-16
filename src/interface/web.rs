use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use axum::{
    Router,
    extract::{Form, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect},
    routing::{get, post},
};
use serde::Deserialize;
use tokio::sync::{Mutex, Notify, mpsc};

use crate::domain::ports::ConfigRepository;
use crate::infrastructure::local_config_adapter::LocalConfigAdapter;

#[derive(Debug, Clone)]
pub enum UiCommand {
    TriggerReviewNow,
    TriggerSkip,
    AdHoc(String),
    RepoAdd(String),
    RepoRemove(String),
    AgentAdd { repo: String, agent: String },
    AgentRemove { repo: String, agent: String },
    DirAdd(String),
    DirRemove(String),
}

#[derive(Clone)]
pub struct UiState {
    pub token: Option<String>,
    pub status_log: Arc<Mutex<Vec<String>>>,
    pub command_tx: mpsc::UnboundedSender<UiCommand>,
    pub notify: Arc<Notify>,
    pub config_repo: Arc<LocalConfigAdapter>,
}

#[derive(Debug, Deserialize, Default)]
struct UiTokenQuery {
    token: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct UiActionForm {
    token: Option<String>,
    pr_url: Option<String>,
    repo: Option<String>,
    agent: Option<String>,
    dir: Option<String>,
}

pub async fn run_ui_server(addr: SocketAddr, state: UiState) -> Result<()> {
    let app = Router::new()
        .route("/", get(ui_index))
        .route("/api/trigger-review", post(ui_trigger_review))
        .route("/api/skip", post(ui_skip))
        .route("/api/adhoc", post(ui_adhoc))
        .route("/api/repo/add", post(ui_repo_add))
        .route("/api/repo/remove", post(ui_repo_remove))
        .route("/api/agent/add", post(ui_agent_add))
        .route("/api/agent/remove", post(ui_agent_remove))
        .route("/api/dir/add", post(ui_dir_add))
        .route("/api/dir/remove", post(ui_dir_remove))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn ui_index(
    State(state): State<UiState>,
    Query(q): Query<UiTokenQuery>,
) -> impl IntoResponse {
    if !authorized(&state.token, q.token.as_deref()) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }

    let cfg = match state.config_repo.load_config() {
        Ok(v) => v,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("config load failed: {err:#}"),
            )
                .into_response();
        }
    };
    let log = state.status_log.lock().await.clone();
    let token = q.token.unwrap_or_default();
    let token_hidden = if token.is_empty() {
        String::new()
    } else {
        format!(
            "<input type=\"hidden\" name=\"token\" value=\"{}\" />",
            html_escape(&token)
        )
    };

    let repos_html = if cfg.repos.is_empty() {
        "<li>(no repos)</li>".to_string()
    } else {
        cfg.repos
            .iter()
            .map(|r| {
                let agents = if r.agents.is_empty() {
                    "-".to_string()
                } else {
                    r.agents.join(", ")
                };
                format!(
                    "<li><b>{}</b> | agents={} | last_sha={}</li>",
                    html_escape(&r.full_name),
                    html_escape(&agents),
                    html_escape(r.last_sha.as_deref().unwrap_or("-"))
                )
            })
            .collect::<Vec<_>>()
            .join("")
    };
    let dirs_html = if cfg.agent_prompt_dirs.is_empty() {
        "<li>(no global dirs)</li>".to_string()
    } else {
        cfg.agent_prompt_dirs
            .iter()
            .map(|d| format!("<li>{}</li>", html_escape(d)))
            .collect::<Vec<_>>()
            .join("")
    };
    let logs_html = if log.is_empty() {
        "<li>(no status yet)</li>".to_string()
    } else {
        log.iter()
            .rev()
            .take(30)
            .map(|s| format!("<li>{}</li>", html_escape(s)))
            .collect::<Vec<_>>()
            .join("")
    };

    let page = format!(
        "<html><body>\
<h1>PrismFlow UI</h1>\
<p>Token protected: {}</p>\
<h2>Actions</h2>\
<form method=\"post\" action=\"/api/trigger-review\">{}<button type=\"submit\">Trigger Review Now</button></form>\
<form method=\"post\" action=\"/api/skip\">{}<button type=\"submit\">Skip Next PR</button></form>\
<form method=\"post\" action=\"/api/adhoc\">{}<input name=\"pr_url\" placeholder=\"https://github.com/owner/repo/pull/123\" size=\"70\"/><button type=\"submit\">Run Ad-hoc</button></form>\
<h2>Repo Management</h2>\
<form method=\"post\" action=\"/api/repo/add\">{}<input name=\"repo\" placeholder=\"owner/repo or github url\" size=\"70\"/><button type=\"submit\">Repo Add</button></form>\
<form method=\"post\" action=\"/api/repo/remove\">{}<input name=\"repo\" placeholder=\"owner/repo\" size=\"40\"/><button type=\"submit\">Repo Remove</button></form>\
<h2>Agent Management</h2>\
<form method=\"post\" action=\"/api/agent/add\">{}<input name=\"repo\" placeholder=\"owner/repo\" size=\"35\"/><input name=\"agent\" placeholder=\"agent\" size=\"20\"/><button type=\"submit\">Agent Add</button></form>\
<form method=\"post\" action=\"/api/agent/remove\">{}<input name=\"repo\" placeholder=\"owner/repo\" size=\"35\"/><input name=\"agent\" placeholder=\"agent\" size=\"20\"/><button type=\"submit\">Agent Remove</button></form>\
<h2>Global Agent Dirs</h2>\
<form method=\"post\" action=\"/api/dir/add\">{}<input name=\"dir\" placeholder=\"path/to/prompts\" size=\"60\"/><button type=\"submit\">Dir Add</button></form>\
<form method=\"post\" action=\"/api/dir/remove\">{}<input name=\"dir\" placeholder=\"path/to/prompts\" size=\"60\"/><button type=\"submit\">Dir Remove</button></form>\
<h3>Configured Global Dirs</h3><ul>{}</ul>\
<h3>Repos</h3><ul>{}</ul>\
<h3>Recent Status</h3><ul>{}</ul>\
<p><a href=\"/?token={}\">Refresh</a></p>\
</body></html>",
        if state.token.is_some() { "yes" } else { "no" },
        token_hidden,
        token_hidden,
        token_hidden,
        token_hidden,
        token_hidden,
        token_hidden,
        token_hidden,
        token_hidden,
        token_hidden,
        dirs_html,
        repos_html,
        logs_html,
        html_escape(&token)
    );
    Html(page).into_response()
}

async fn ui_trigger_review(
    State(state): State<UiState>,
    Form(form): Form<UiActionForm>,
) -> impl IntoResponse {
    if !authorized(&state.token, form.token.as_deref()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let _ = state.command_tx.send(UiCommand::TriggerReviewNow);
    state.notify.notify_one();
    ui_redirect(&state.token, form.token.as_deref()).into_response()
}

async fn ui_skip(
    State(state): State<UiState>,
    Form(form): Form<UiActionForm>,
) -> impl IntoResponse {
    if !authorized(&state.token, form.token.as_deref()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let _ = state.command_tx.send(UiCommand::TriggerSkip);
    state.notify.notify_one();
    ui_redirect(&state.token, form.token.as_deref()).into_response()
}

async fn ui_adhoc(
    State(state): State<UiState>,
    Form(form): Form<UiActionForm>,
) -> impl IntoResponse {
    if !authorized(&state.token, form.token.as_deref()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if let Some(pr_url) = form.pr_url.filter(|v| !v.trim().is_empty()) {
        let _ = state.command_tx.send(UiCommand::AdHoc(pr_url));
        state.notify.notify_one();
    }
    ui_redirect(&state.token, form.token.as_deref()).into_response()
}

async fn ui_repo_add(
    State(state): State<UiState>,
    Form(form): Form<UiActionForm>,
) -> impl IntoResponse {
    if !authorized(&state.token, form.token.as_deref()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if let Some(repo) = form.repo.filter(|v| !v.trim().is_empty()) {
        let _ = state.command_tx.send(UiCommand::RepoAdd(repo));
        state.notify.notify_one();
    }
    ui_redirect(&state.token, form.token.as_deref()).into_response()
}

async fn ui_repo_remove(
    State(state): State<UiState>,
    Form(form): Form<UiActionForm>,
) -> impl IntoResponse {
    if !authorized(&state.token, form.token.as_deref()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if let Some(repo) = form.repo.filter(|v| !v.trim().is_empty()) {
        let _ = state.command_tx.send(UiCommand::RepoRemove(repo));
        state.notify.notify_one();
    }
    ui_redirect(&state.token, form.token.as_deref()).into_response()
}

async fn ui_agent_add(
    State(state): State<UiState>,
    Form(form): Form<UiActionForm>,
) -> impl IntoResponse {
    if !authorized(&state.token, form.token.as_deref()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if let (Some(repo), Some(agent)) = (form.repo, form.agent) {
        if !repo.trim().is_empty() && !agent.trim().is_empty() {
            let _ = state.command_tx.send(UiCommand::AgentAdd { repo, agent });
            state.notify.notify_one();
        }
    }
    ui_redirect(&state.token, form.token.as_deref()).into_response()
}

async fn ui_agent_remove(
    State(state): State<UiState>,
    Form(form): Form<UiActionForm>,
) -> impl IntoResponse {
    if !authorized(&state.token, form.token.as_deref()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if let (Some(repo), Some(agent)) = (form.repo, form.agent) {
        if !repo.trim().is_empty() && !agent.trim().is_empty() {
            let _ = state
                .command_tx
                .send(UiCommand::AgentRemove { repo, agent });
            state.notify.notify_one();
        }
    }
    ui_redirect(&state.token, form.token.as_deref()).into_response()
}

async fn ui_dir_add(
    State(state): State<UiState>,
    Form(form): Form<UiActionForm>,
) -> impl IntoResponse {
    if !authorized(&state.token, form.token.as_deref()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if let Some(dir) = form.dir.filter(|v| !v.trim().is_empty()) {
        let _ = state.command_tx.send(UiCommand::DirAdd(dir));
        state.notify.notify_one();
    }
    ui_redirect(&state.token, form.token.as_deref()).into_response()
}

async fn ui_dir_remove(
    State(state): State<UiState>,
    Form(form): Form<UiActionForm>,
) -> impl IntoResponse {
    if !authorized(&state.token, form.token.as_deref()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if let Some(dir) = form.dir.filter(|v| !v.trim().is_empty()) {
        let _ = state.command_tx.send(UiCommand::DirRemove(dir));
        state.notify.notify_one();
    }
    ui_redirect(&state.token, form.token.as_deref()).into_response()
}

fn ui_redirect(expected: &Option<String>, provided: Option<&str>) -> Redirect {
    if expected.is_some() {
        let token = provided.unwrap_or_default();
        Redirect::to(&format!("/?token={token}"))
    } else {
        Redirect::to("/")
    }
}

fn authorized(expected: &Option<String>, provided: Option<&str>) -> bool {
    match expected {
        Some(v) => provided.map(|s| s == v).unwrap_or(false),
        None => true,
    }
}

fn html_escape(v: &str) -> String {
    v.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\"', "&quot;")
}
