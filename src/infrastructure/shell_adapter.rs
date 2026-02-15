use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

pub trait ShellAdapter: Send + Sync {
    fn run_capture(&self, program: &str, args: &[&str]) -> Result<String>;
    fn run_command_line(&self, command_line: &str) -> Result<String>;
    fn run_command_line_in_dir(&self, command_line: &str, workdir: Option<&str>) -> Result<String>;
}

#[derive(Debug, Clone, Default)]
pub struct CommandShellAdapter {
    shell_override: Option<String>,
}

impl CommandShellAdapter {
    pub fn new(shell_override: Option<String>) -> Self {
        Self {
            shell_override: shell_override
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
        }
    }

    fn resolve_shell_program(&self) -> String {
        if let Some(v) = &self.shell_override {
            return v.clone();
        }
        if let Ok(v) = std::env::var("PRISMFLOW_SHELL") {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
        if let Ok(v) = std::env::var("SHELL") {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
        #[cfg(target_os = "windows")]
        {
            if std::env::var("PSModulePath").is_ok() {
                return "powershell".to_string();
            }
            if let Ok(v) = std::env::var("ComSpec") {
                let trimmed = v.trim();
                if !trimmed.is_empty() {
                    return trimmed.to_string();
                }
            }
            return "cmd".to_string();
        }
        #[cfg(not(target_os = "windows"))]
        {
            "sh".to_string()
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ShellKind {
    Cmd,
    PowerShell,
    Posix,
}

fn shell_kind(program: &str) -> ShellKind {
    let name = Path::new(program)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(program)
        .to_ascii_lowercase();
    if name == "cmd" || name == "cmd.exe" {
        return ShellKind::Cmd;
    }
    if name.contains("powershell") || name == "pwsh" || name == "pwsh.exe" {
        return ShellKind::PowerShell;
    }
    ShellKind::Posix
}

impl ShellAdapter for CommandShellAdapter {
    fn run_capture(&self, program: &str, args: &[&str]) -> Result<String> {
        let output = Command::new(program)
            .args(args)
            .output()
            .with_context(|| format!("failed to execute command: {}", program))?;

        if !output.status.success() {
            anyhow::bail!(
                "command exited with status {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    fn run_command_line(&self, command_line: &str) -> Result<String> {
        self.run_command_line_in_dir(command_line, None)
    }

    fn run_command_line_in_dir(&self, command_line: &str, workdir: Option<&str>) -> Result<String> {
        let shell_program = self.resolve_shell_program();
        let output = match shell_kind(&shell_program) {
            ShellKind::Cmd => {
                let mut cmd = Command::new(&shell_program);
                cmd.args(["/C", command_line]);
                if let Some(dir) = workdir {
                    cmd.current_dir(dir);
                }
                cmd.output().with_context(|| {
                    format!("failed to execute command line via {}", shell_program)
                })?
            }
            ShellKind::PowerShell => {
                let mut cmd = Command::new(&shell_program);
                cmd.args(["-NoProfile", "-Command", command_line]);
                if let Some(dir) = workdir {
                    cmd.current_dir(dir);
                }
                cmd.output().with_context(|| {
                    format!("failed to execute command line via {}", shell_program)
                })?
            }
            ShellKind::Posix => {
                let mut cmd = Command::new(&shell_program);
                cmd.args(["-lc", command_line]);
                if let Some(dir) = workdir {
                    cmd.current_dir(dir);
                }
                cmd.output().with_context(|| {
                    format!("failed to execute command line via {}", shell_program)
                })?
            }
        };

        if !output.status.success() {
            anyhow::bail!(
                "command line exited with status {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
}
