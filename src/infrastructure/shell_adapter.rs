use crate::domain::errors::DomainError;
use crate::domain::ports::{CommandContext, ShellAdapter};
use anyhow::{Context, Result};
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::process::Command;
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::process::Command as TokioCommand;

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

#[async_trait]
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

    async fn run_command_line(
        &self,
        command_line: &str,
        ctx: Option<&dyn CommandContext>,
    ) -> Result<String> {
        self.run_command_line_in_dir(command_line, None, ctx).await
    }

    async fn run_command_line_in_dir(
        &self,
        command_line: &str,
        workdir: Option<&str>,
        ctx: Option<&dyn CommandContext>,
    ) -> Result<String> {
        let shell_program = self.resolve_shell_program();
        let mut cmd = TokioCommand::new(&shell_program);
        match shell_kind(&shell_program) {
            ShellKind::Cmd => {
                cmd.args(["/C", command_line]);
            }
            ShellKind::PowerShell => {
                cmd.args(["-NoProfile", "-Command", command_line]);
            }
            ShellKind::Posix => {
                cmd.args(["-lc", command_line]);
            }
        }
        if let Some(dir) = workdir {
            cmd.current_dir(dir);
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to execute command line via {}", shell_program))?;
        let pid = child.id();
        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        if let (Some(task_ctx), Some(pid)) = (ctx, pid) {
            task_ctx
                .register_child(
                    pid,
                    format!(
                        "command_fingerprint={} command={}",
                        command_fingerprint(command_line),
                        command_line
                    ),
                )
                .await;
        }
        let status = if let Some(task_ctx) = ctx {
            tokio::select! {
                out = child.wait() => out,
                _ = task_ctx.cancelled() => {
                    let _ = child.kill().await;
                    if let (Some(task_ctx), Some(pid)) = (ctx, pid) {
                        task_ctx.unregister_child(pid).await;
                    }
                    anyhow::bail!(DomainError::CancelledBySignal);
                }
            }?
        } else {
            child.wait().await?
        };
        let mut stdout_buf = Vec::new();
        let mut stderr_buf = Vec::new();
        if let Some(ref mut out) = stdout {
            let _ = out.read_to_end(&mut stdout_buf).await;
        }
        if let Some(ref mut err) = stderr {
            let _ = err.read_to_end(&mut stderr_buf).await;
        };
        if let (Some(task_ctx), Some(pid)) = (ctx, pid) {
            task_ctx.unregister_child(pid).await;
        }

        if !status.success() {
            anyhow::bail!(
                "command line exited with status {}: {}",
                status,
                String::from_utf8_lossy(&stderr_buf)
            );
        }

        Ok(String::from_utf8_lossy(&stdout_buf).trim().to_string())
    }
}

fn command_fingerprint(command_line: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(command_line.as_bytes());
    let hex = hex::encode(hasher.finalize());
    hex.chars().take(12).collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use super::CommandShellAdapter;
    use crate::domain::ports::{CommandContext, ShellAdapter};
    use async_trait::async_trait;
    use std::time::{Duration, Instant};
    use tokio::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    #[derive(Default)]
    struct MockCommandContext {
        cancel: CancellationToken,
        children: Mutex<HashMap<u32, String>>,
    }

    impl MockCommandContext {
        fn cancel(&self) {
            self.cancel.cancel();
        }
    }

    #[async_trait]
    impl CommandContext for MockCommandContext {
        fn is_cancelled(&self) -> bool {
            self.cancel.is_cancelled()
        }

        async fn cancelled(&self) {
            self.cancel.cancelled().await;
        }

        async fn register_child(&self, pid: u32, label: String) {
            self.children.lock().await.insert(pid, label);
        }

        async fn unregister_child(&self, pid: u32) {
            self.children.lock().await.remove(&pid);
        }
    }

    #[tokio::test]
    async fn command_line_cancelled_by_context_quickly() {
        let adapter = CommandShellAdapter::default();
        let ctx = Arc::new(MockCommandContext::default());
        #[cfg(target_os = "windows")]
        let command = "Start-Sleep -Seconds 8; Write-Output done";
        #[cfg(not(target_os = "windows"))]
        let command = "sleep 8; echo done";

        let started = Instant::now();
        let ((), result) = tokio::join!(
            async {
                tokio::time::sleep(Duration::from_millis(300)).await;
                ctx.cancel();
            },
            async { adapter.run_command_line(command, Some(ctx.as_ref())).await }
        );
        let elapsed = started.elapsed();

        assert!(result.is_err());
        let msg = format!("{:#}", result.err().expect("cancelled error"));
        assert!(msg.contains("operation cancelled by signal"));
        assert!(
            elapsed < Duration::from_secs(3),
            "cancel should be quick, elapsed={elapsed:?}"
        );
    }

    #[tokio::test]
    async fn command_line_success_without_cancellation() {
        let adapter = CommandShellAdapter::default();
        let output = adapter
            .run_command_line("echo prismflow", None)
            .await
            .expect("command should succeed");
        assert!(output.to_ascii_lowercase().contains("prismflow"));
    }
}
