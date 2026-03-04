use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};

use crate::domain::ports::FileSystem;

/// Agent 提示词加载服务
pub trait AgentPromptService: Send + Sync {
    /// 验证所有必需的 Agent 提示词是否存在
    fn validate_agents(&self, agents: &[String]) -> Result<()>;

    /// 加载所有 Agent 提示词内容
    fn load_agents(&self, agents: &[String]) -> Result<String>;
}

/// 基于文件系统的 Agent 提示词服务实现
pub struct FileSystemAgentPromptService<'a> {
    fs: &'a dyn FileSystem,
    extra_dirs: Vec<String>,
}

impl<'a> FileSystemAgentPromptService<'a> {
    pub fn new(fs: &'a dyn FileSystem, extra_dirs: Vec<String>) -> Self {
        Self { fs, extra_dirs }
    }

    fn resolve_prompt_paths(&self, agent: &str) -> Vec<PathBuf> {
        let file_name = format!("{agent}.md");
        let mut bases: Vec<PathBuf> = self
            .extra_dirs
            .iter()
            .map(|d| PathBuf::from(d.trim()))
            .filter(|p| !p.as_os_str().is_empty())
            .collect();

        let cwd = self.fs.current_dir().unwrap_or_else(|_| PathBuf::from("."));
        bases.push(cwd.join(".prismflow").join("prompts"));

        if let Some(config_dir) = self.fs.config_dir() {
            bases.push(config_dir.join("pr-reviewer").join("prompts"));
        }

        bases
            .into_iter()
            .map(|base| base.join(&file_name))
            .collect()
    }
}

impl<'a> AgentPromptService for FileSystemAgentPromptService<'a> {
    fn validate_agents(&self, agents: &[String]) -> Result<()> {
        if agents.is_empty() {
            return Ok(());
        }

        for agent in agents {
            let paths = self.resolve_prompt_paths(agent);
            let found = paths
                .iter()
                .any(|path| self.fs.exists(path) && file_name_matches_exact_case(self.fs, path));

            if !found {
                let checked_str = paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(" ; ");
                return Err(anyhow!(
                    "agent prompt file missing: checked {}",
                    checked_str
                ));
            }
        }

        Ok(())
    }

    fn load_agents(&self, agents: &[String]) -> Result<String> {
        if agents.is_empty() {
            return Ok(String::new());
        }

        let mut sections = Vec::new();
        for agent in agents {
            let paths = self.resolve_prompt_paths(agent);
            let mut loaded = None;

            for path in &paths {
                if self.fs.exists(path) && file_name_matches_exact_case(self.fs, path) {
                    let content = self.fs.read_to_string(path).map_err(|e| {
                        anyhow!("failed to read agent prompt file {}: {}", path.display(), e)
                    })?;
                    loaded = Some(content);
                    break;
                }
            }

            match loaded {
                Some(content) => sections.push(format!("# Agent: {agent}\n{content}")),
                None => {
                    let checked_str = paths
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(" ; ");
                    return Err(anyhow!(
                        "agent prompt file missing: checked {}",
                        checked_str
                    ));
                }
            }
        }

        Ok(sections.join("\n\n"))
    }
}

fn file_name_matches_exact_case(fs: &dyn FileSystem, path: &Path) -> bool {
    let Some(parent) = path.parent() else {
        return true;
    };
    let Some(target_name) = path.file_name() else {
        return true;
    };

    // Keep compatibility for non-real filesystems used in tests/mocks.
    let entries = match std::fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(_) => return fs.exists(path),
    };

    entries
        .filter_map(Result::ok)
        .any(|entry| entry.file_name() == target_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ports::FileSystem;
    use std::{
        collections::HashMap,
        fs,
        path::{Path, PathBuf},
        sync::Arc,
        time::{SystemTime, UNIX_EPOCH},
    };

    struct MockFileSystem {
        files: HashMap<PathBuf, String>,
    }

    impl MockFileSystem {
        fn new() -> Self {
            Self {
                files: HashMap::new(),
            }
        }

        fn add_file(&mut self, path: &str, content: &str) {
            self.files.insert(PathBuf::from(path), content.to_string());
        }
    }

    impl FileSystem for MockFileSystem {
        fn create_dir_all(&self, _path: &Path) -> Result<()> {
            Ok(())
        }

        fn write(&self, _path: &Path, _content: &[u8]) -> Result<()> {
            Ok(())
        }

        fn read_to_string(&self, path: &Path) -> Result<String> {
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| anyhow!("file not found: {}", path.display()))
        }

        fn remove_file(&self, _path: &Path) -> Result<()> {
            Ok(())
        }

        fn current_dir(&self) -> Result<PathBuf> {
            Ok(PathBuf::from("/test"))
        }

        fn config_dir(&self) -> Option<PathBuf> {
            Some(PathBuf::from("/config"))
        }

        fn exists(&self, path: &Path) -> bool {
            self.files.contains_key(path)
        }
    }

    #[test]
    fn test_validate_agents_success() {
        let mut mock_fs = MockFileSystem::new();
        mock_fs.add_file("/test/.prismflow/prompts/security.md", "Security rules");

        let service = FileSystemAgentPromptService::new(&mock_fs, vec![]);
        let result = service.validate_agents(&vec!["security".to_string()]);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_agents_missing() {
        let mock_fs = MockFileSystem::new();
        let service = FileSystemAgentPromptService::new(&mock_fs, vec![]);
        let result = service.validate_agents(&vec!["missing".to_string()]);
        assert!(result.is_err());
    }

    #[test]
    fn test_load_agents_success() {
        let mut mock_fs = MockFileSystem::new();
        mock_fs.add_file("/test/.prismflow/prompts/security.md", "Security rules");

        let service = FileSystemAgentPromptService::new(&mock_fs, vec![]);
        let result = service.load_agents(&vec!["security".to_string()]);
        assert!(result.is_ok());
        let content = result.unwrap();
        assert!(content.contains("# Agent: security"));
        assert!(content.contains("Security rules"));
    }

    struct TempFileSystem {
        root: PathBuf,
    }

    impl FileSystem for TempFileSystem {
        fn create_dir_all(&self, path: &Path) -> Result<()> {
            fs::create_dir_all(path)?;
            Ok(())
        }

        fn write(&self, path: &Path, content: &[u8]) -> Result<()> {
            fs::write(path, content)?;
            Ok(())
        }

        fn read_to_string(&self, path: &Path) -> Result<String> {
            Ok(fs::read_to_string(path)?)
        }

        fn remove_file(&self, path: &Path) -> Result<()> {
            fs::remove_file(path)?;
            Ok(())
        }

        fn current_dir(&self) -> Result<PathBuf> {
            Ok(self.root.clone())
        }

        fn config_dir(&self) -> Option<PathBuf> {
            None
        }

        fn exists(&self, path: &Path) -> bool {
            path.exists()
        }
    }

    fn make_temp_dir(tag: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("duration")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("prismflow-agent-service-{tag}-{stamp}"));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn test_validate_agents_requires_exact_case_file_name() {
        let root = make_temp_dir("agent-case-sensitive-miss");
        fs::write(root.join("DEBUG.md"), "debug uppercase").expect("write prompt");
        let fs_adapter = TempFileSystem { root: root.clone() };
        let service = FileSystemAgentPromptService::new(
            &fs_adapter,
            vec![root.to_string_lossy().to_string()],
        );
        let result = service.validate_agents(&["debug".to_string()]);
        assert!(result.is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn test_validate_agents_accepts_exact_case_file_name() {
        let root = make_temp_dir("agent-case-sensitive-hit");
        fs::write(root.join("DEBUG.md"), "debug uppercase").expect("write prompt");
        let fs_adapter = TempFileSystem { root: root.clone() };
        let service = FileSystemAgentPromptService::new(
            &fs_adapter,
            vec![root.to_string_lossy().to_string()],
        );
        let result = service.validate_agents(&["DEBUG".to_string()]);
        assert!(result.is_ok());
        let _ = fs::remove_dir_all(root);
    }
}
