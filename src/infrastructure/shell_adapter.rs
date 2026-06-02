use crate::domain::engine_output::STDOUT_FILE_MARKER_PREFIX;
use crate::domain::errors::DomainError;
use crate::domain::ports::{CommandContext, ProcessManager, ShellAdapter};
use anyhow::{Context, Result};
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::AsyncReadExt;
use tokio::process::Command as TokioCommand;
use tokio::time::{Duration, sleep};

#[derive(Debug, Clone, Default)]
pub struct CommandShellAdapter {
    shell_override: Option<String>,
}

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
        let command_fp = command_fingerprint(command_line);
        let stdout_path = prepare_stream_output_path(&command_fp, "stdout");
        let stderr_path = prepare_stream_output_path(&command_fp, "stderr");
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
            let path = stdout_path.clone();
            tokio::spawn(async move { read_stream_capture(&mut out, path).await })
        });
        let stderr_reader = child.stderr.take().map(|mut err| {
            let path = stderr_path.clone();
            tokio::spawn(async move { read_stream_capture(&mut err, path).await })
        });
        if let (Some(task_ctx), Some(pid)) = (ctx, pid) {
            task_ctx
                .register_child(
                    pid,
                    format!(
                        "command_fingerprint={} command={}",
                        command_fp, command_line
                    ),
                )
                .await;
        }
        let status = if let Some(task_ctx) = ctx {
            tokio::select! {
                out = child.wait() => out,
                _ = task_ctx.cancelled() => {
                    if let Some(p) = pid {
                        let pm = crate::infrastructure::process_manager::OsProcessManager;
                        pm.kill_process_tree(p);
                    }
                    let _ = child.kill().await;
                    if let (Some(task_ctx), Some(pid)) = (ctx, pid) {
                        task_ctx.unregister_child(pid).await;
                    }
                    anyhow::bail!(DomainError::CancelledBySignal);
                }
                _ = sleep(Duration::from_secs(COMMAND_TIMEOUT_SECS)) => {
                    if let Some(p) = pid {
                        let pm = crate::infrastructure::process_manager::OsProcessManager;
                        pm.kill_process_tree(p);
                    }
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
                    if let Some(p) = pid {
                        let pm = crate::infrastructure::process_manager::OsProcessManager;
                        pm.kill_process_tree(p);
                    }
                    let _ = child.kill().await;
                    anyhow::bail!("command line timed out after {}s", COMMAND_TIMEOUT_SECS);
                }
            }
        };
        let stdout_capture = match stdout_reader {
            Some(handle) => handle.await.unwrap_or_default(),
            None => StreamCapture::default(),
        };
        let stderr_capture = match stderr_reader {
            Some(handle) => handle.await.unwrap_or_default(),
            None => StreamCapture::default(),
        };
        if let (Some(task_ctx), Some(pid)) = (ctx, pid) {
            task_ctx.unregister_child(pid).await;
        }

        let stderr_text = format_stream_for_error(&stderr_capture);
        if !status.success() {
            anyhow::bail!(
                "command line exited with status {}: {}",
                status,
                stderr_text
            );
        }

        if is_empty_output_file(stdout_capture.full_output_path.as_ref())
            && is_non_empty_output_file(stderr_capture.full_output_path.as_ref())
        {
            let stderr_excerpt =
                read_output_excerpt(stderr_capture.full_output_path.as_ref(), 2000);
            anyhow::bail!(
                "command line produced empty stdout but non-empty stderr: {}",
                stderr_excerpt
            );
        }

        if let Some(path) = &stdout_capture.full_output_path {
            return Ok(format!("{STDOUT_FILE_MARKER_PREFIX}{}", path.display()));
        }
        Ok(String::new())
    }
}

#[derive(Debug, Default)]
struct StreamCapture {
    full_output_path: Option<PathBuf>,
}

async fn read_stream_capture<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    output_path: Option<PathBuf>,
) -> StreamCapture {
    let mut output_file = None::<fs::File>;
    let mut full_output_path = None::<PathBuf>;
    if let Some(path) = output_path {
        if let Ok(file) = fs::File::create(&path) {
            output_file = Some(file);
            full_output_path = Some(path);
        }
    }
    let mut chunk = [0u8; 8192];
    loop {
        let n = match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(v) => v,
            Err(_) => break,
        };
        if let Some(file) = output_file.as_mut() {
            let _ = file.write_all(&chunk[..n]);
        }
    }
    StreamCapture { full_output_path }
}

fn format_stream_for_error(capture: &StreamCapture) -> String {
    if let Some(path) = &capture.full_output_path {
        return format!("see stderr file: {}", path.display());
    }
    "stderr output file unavailable".to_string()
}

fn prepare_stream_output_path(command_fp: &str, stream_name: &str) -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    let root = cwd.join(".prismflow").join("tmp-diffs");
    if fs::create_dir_all(&root).is_err() {
        return None;
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some(root.join(format!(
        "engine-{}-{}-{}.log",
        stamp, command_fp, stream_name
    )))
}

fn is_empty_output_file(path: Option<&PathBuf>) -> bool {
    match path.and_then(|p| fs::metadata(p).ok()) {
        Some(meta) => meta.len() == 0,
        None => true,
    }
}

fn is_non_empty_output_file(path: Option<&PathBuf>) -> bool {
    match path.and_then(|p| fs::metadata(p).ok()) {
        Some(meta) => meta.len() > 0,
        None => false,
    }
}

fn read_output_excerpt(path: Option<&PathBuf>, max_chars: usize) -> String {
    let Some(path) = path else {
        return "stderr output file unavailable".to_string();
    };
    let mut file = match fs::File::open(path) {
        Ok(v) => v,
        Err(_) => return format!("stderr file exists but cannot be read: {}", path.display()),
    };
    let mut text = String::new();
    if file.read_to_string(&mut text).is_err() {
        return format!(
            "stderr file exists but cannot decode as UTF-8: {}",
            path.display()
        );
    }
    if text.len() <= max_chars {
        text
    } else {
        let mut cut = max_chars;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}...[truncated]", &text[..cut])
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
    use std::fs;
    use std::sync::Arc;

    use super::CommandShellAdapter;
    use crate::domain::engine_output::STDOUT_FILE_MARKER_PREFIX;
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
        assert!(output.starts_with(STDOUT_FILE_MARKER_PREFIX));
        let path = output.trim_start_matches(STDOUT_FILE_MARKER_PREFIX);
        let raw = fs::read_to_string(path).expect("read stdout file");
        assert!(raw.to_ascii_lowercase().contains("prismflow"));
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

        assert!(result.starts_with(STDOUT_FILE_MARKER_PREFIX));
        let path = result.trim_start_matches(STDOUT_FILE_MARKER_PREFIX);
        let raw = fs::read_to_string(path).expect("read stdout file");
        assert!(raw.contains("prismflow"));
    }
}
