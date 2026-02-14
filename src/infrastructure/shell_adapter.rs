use anyhow::{Context, Result};
use std::process::Command;

pub trait ShellAdapter: Send + Sync {
    fn run_capture(&self, program: &str, args: &[&str]) -> Result<String>;
    fn run_command_line(&self, command_line: &str) -> Result<String>;
}

#[derive(Debug, Default)]
pub struct CommandShellAdapter;

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
        #[cfg(target_os = "windows")]
        let output = Command::new("cmd")
            .args(["/C", command_line])
            .output()
            .with_context(|| "failed to execute command line via cmd".to_string())?;

        #[cfg(not(target_os = "windows"))]
        let output = Command::new("sh")
            .args(["-lc", command_line])
            .output()
            .with_context(|| "failed to execute command line via sh".to_string())?;

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
