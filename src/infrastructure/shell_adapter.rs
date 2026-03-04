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
use tokio::time::{Duration, sleep};

#[derive(Debug, Clone, Default)]
pub struct CommandShellAdapter {
    shell_override: Option<String>,
}

const MAX_CAPTURE_BYTES: usize = 1024 * 1024;
const COMMAND_TIMEOUT_SECS: u64 = 10 * 60;

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
        if ctx.map(|task_ctx| task_ctx.is_cancelled()).unwrap_or(false) {
            anyhow::bail!(DomainError::CancelledBySignal);
        }
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
        let stdout_reader = child.stdout.take().map(|mut out| {
            tokio::spawn(async move {
                read_limited(&mut out, MAX_CAPTURE_BYTES).await
            })
        });
        let stderr_reader = child.stderr.take().map(|mut err| {
            tokio::spawn(async move {
                read_limited(&mut err, MAX_CAPTURE_BYTES).await
            })
        });
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
                _ = sleep(Duration::from_secs(COMMAND_TIMEOUT_SECS)) => {
                    let _ = child.kill().await;
                    if let (Some(task_ctx), Some(pid)) = (ctx, pid) {
                        task_ctx.unregister_child(pid).await;
                    }
                    anyhow::bail!("command line timed out after {}s", COMMAND_TIMEOUT_SECS);
                }
            }?
        } else {
            tokio::select! {
                out = child.wait() => out?,
                _ = sleep(Duration::from_secs(COMMAND_TIMEOUT_SECS)) => {
                    let _ = child.kill().await;
                    anyhow::bail!("command line timed out after {}s", COMMAND_TIMEOUT_SECS);
                }
            }
        };
        let (stdout_buf, stdout_truncated) = match stdout_reader {
            Some(handle) => handle.await.unwrap_or_default(),
            None => (Vec::new(), false),
        };
        let (stderr_buf, stderr_truncated) = match stderr_reader {
            Some(handle) => handle.await.unwrap_or_default(),
            None => (Vec::new(), false),
        };
        if let (Some(task_ctx), Some(pid)) = (ctx, pid) {
            task_ctx.unregister_child(pid).await;
        }

        let stderr_text = maybe_truncated_text(&stderr_buf, stderr_truncated);
        if !status.success() {
            anyhow::bail!(
                "command line exited with status {}: {}",
                status,
                stderr_text
            );
        }

        let stdout_text = maybe_truncated_text(&stdout_buf, stdout_truncated);
        Ok(stdout_text.trim().to_string())
    }
}

async fn read_limited<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> (Vec<u8>, bool) {
    let mut out = Vec::<u8>::new();
    let mut truncated = false;
    let mut chunk = [0u8; 8192];
    loop {
        let n = match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(v) => v,
            Err(_) => break,
        };
        if out.len() < limit {
            let remaining = limit - out.len();
            let to_copy = remaining.min(n);
            out.extend_from_slice(&chunk[..to_copy]);
            if to_copy < n {
                truncated = true;
            }
        } else {
            truncated = true;
        }
    }
    (out, truncated)
}

fn maybe_truncated_text(buf: &[u8], truncated: bool) -> String {
    let mut text = String::from_utf8_lossy(buf).into_owned();
    if truncated {
        text.push_str("\n[truncated output]");
    }
    text
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

    #[tokio::test]
    async fn command_line_large_output_does_not_deadlock() {
        let adapter = CommandShellAdapter::default();
        #[cfg(target_os = "windows")]
        let command = "1..12000 | ForEach-Object { 'prismflow' }";
        #[cfg(not(target_os = "windows"))]
        let command = "for i in $(seq 1 12000); do echo prismflow; done";

        let result = tokio::time::timeout(
            Duration::from_secs(8),
            adapter.run_command_line(command, None),
        )
        .await
        .expect("command should not hang")
        .expect("command should succeed");

        assert!(result.contains("prismflow"));
    }
}
