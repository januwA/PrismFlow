use anyhow::{Result, bail};

use crate::domain::ports::{TokenProvider, TokenWriter};

#[derive(Debug, Clone)]
pub struct TokenResolution {
    pub source: &'static str,
    pub token: String,
}

pub struct AuthManager<'a> {
    providers: Vec<&'a dyn TokenProvider>,
    stored: &'a dyn TokenWriter,
}

impl<'a> AuthManager<'a> {
    pub fn new(providers: Vec<&'a dyn TokenProvider>, stored: &'a dyn TokenWriter) -> Self {
        Self { providers, stored }
    }

    pub fn resolve_token(&self) -> Result<Option<TokenResolution>> {
        for provider in &self.providers {
            if let Some(token) = provider.token()? {
                return Ok(Some(TokenResolution {
                    source: provider.source_name(),
                    token,
                }));
            }
        }
        Ok(None)
    }

    pub fn login(&self, token: &str) -> Result<()> {
        let cleaned = token.trim();
        if cleaned.is_empty() {
            bail!("token cannot be empty");
        }
        self.stored.save_token(cleaned)
    }
}
