use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

use crate::domain::{entities::AppConfig, ports::ConfigRepository};

#[derive(Debug, Clone)]
pub struct LocalConfigAdapter {
    config_root: PathBuf,
    repos_path: PathBuf,
}

impl LocalConfigAdapter {
    pub fn new() -> Result<Self> {
        let root = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("pr-reviewer");
        let repos_path = root.join("repos.json");

        fs::create_dir_all(&root)
            .with_context(|| format!("failed to create config dir: {}", root.display()))?;

        if !repos_path.exists() {
            let initial = serde_json::to_string_pretty(&AppConfig::default())?;
            fs::write(&repos_path, initial).with_context(|| {
                format!("failed to initialize config file: {}", repos_path.display())
            })?;
        }

        Ok(Self {
            config_root: root,
            repos_path,
        })
    }

    pub fn auth_token_path(&self) -> PathBuf {
        self.config_root.join("auth_token")
    }

    fn load_raw(&self) -> Result<String> {
        fs::read_to_string(&self.repos_path)
            .with_context(|| format!("failed to read {}", self.repos_path.display()))
    }
}

impl ConfigRepository for LocalConfigAdapter {
    fn load_config(&self) -> Result<AppConfig> {
        let raw = self.load_raw()?;
        let cfg: AppConfig =
            serde_json::from_str(&raw).with_context(|| "invalid repos.json format".to_string())?;
        Ok(cfg)
    }

    fn save_config(&self, config: &AppConfig) -> Result<()> {
        let raw = serde_json::to_string_pretty(config)?;
        fs::write(&self.repos_path, raw)
            .with_context(|| format!("failed to write {}", self.repos_path.display()))?;
        Ok(())
    }

    fn config_path(&self) -> &Path {
        &self.repos_path
    }
}
