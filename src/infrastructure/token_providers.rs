use std::{env, fs, path::PathBuf};

use anyhow::{Context, Result};

use crate::domain::ports::{ShellAdapter, TokenProvider};

pub struct GhCliTokenProvider<'a> {
    shell: &'a dyn ShellAdapter,
}

impl<'a> GhCliTokenProvider<'a> {
    pub fn new(shell: &'a dyn ShellAdapter) -> Self {
        Self { shell }
    }
}

impl TokenProvider for GhCliTokenProvider<'_> {
    fn source_name(&self) -> &'static str {
        "gh auth token"
    }

    fn token(&self) -> Result<Option<String>> {
        match self.shell.run_capture("gh", &["auth", "token"]) {
            Ok(v) if !v.trim().is_empty() => Ok(Some(v)),
            Ok(_) => Ok(None),
            Err(_) => Ok(None),
        }
    }
}

#[derive(Debug, Default)]
pub struct EnvTokenProvider;

impl TokenProvider for EnvTokenProvider {
    fn source_name(&self) -> &'static str {
        "GITHUB_TOKEN"
    }

    fn token(&self) -> Result<Option<String>> {
        Ok(env::var("GITHUB_TOKEN")
            .ok()
            .filter(|v| !v.trim().is_empty()))
    }
}

#[derive(Debug, Clone)]
pub struct StoredTokenProvider {
    path: PathBuf,
}

impl StoredTokenProvider {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn save_token(&self, token: &str) -> Result<()> {
        fs::write(&self.path, token.trim())
            .with_context(|| format!("failed to save token to {}", self.path.display()))
    }
}

impl TokenProvider for StoredTokenProvider {
    fn source_name(&self) -> &'static str {
        "stored token"
    }

    fn token(&self) -> Result<Option<String>> {
        if !self.path.exists() {
            return Ok(None);
        }

        let token = fs::read_to_string(&self.path)
            .with_context(|| format!("failed to read {}", self.path.display()))?;
        let token = token.trim().to_string();
        if token.is_empty() {
            Ok(None)
        } else {
            Ok(Some(token))
        }
    }
}
