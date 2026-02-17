use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

use crate::domain::ports::FileSystem;

#[derive(Debug, Clone, Default)]
pub struct StdFileSystemAdapter;

impl FileSystem for StdFileSystemAdapter {
    fn create_dir_all(&self, path: &Path) -> Result<()> {
        fs::create_dir_all(path)
            .with_context(|| format!("failed to create dir: {}", path.display()))
    }

    fn write(&self, path: &Path, content: &[u8]) -> Result<()> {
        fs::write(path, content)
            .with_context(|| format!("failed to write file: {}", path.display()))
    }

    fn read_to_string(&self, path: &Path) -> Result<String> {
        fs::read_to_string(path).with_context(|| format!("failed to read file: {}", path.display()))
    }

    fn remove_file(&self, path: &Path) -> Result<()> {
        fs::remove_file(path).with_context(|| format!("failed to remove file: {}", path.display()))
    }

    fn current_dir(&self) -> Result<PathBuf> {
        std::env::current_dir().context("failed to get current dir")
    }

    fn config_dir(&self) -> Option<PathBuf> {
        dirs::config_dir()
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
}
