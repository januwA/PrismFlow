use std::{path::Path, process::Stdio};

use anyhow::Result;
use async_trait::async_trait;
use tokio::process::Command;

use crate::domain::{
    errors::DomainError,
    ports::{CommandContext, GitService},
};

#[derive(Debug, Clone, Default)]
pub struct LocalGitAdapter;

#[async_trait]
impl GitService for LocalGitAdapter {
    async fn clone_repo(
        &self,
        url: &str,
        target_dir: &Path,
        ctx: Option<&dyn CommandContext>,
    ) -> Result<()> {
        run_git_command(
            ctx,
            &["clone", "--no-checkout", url, &target_dir.to_string_lossy()],
            None,
            "git clone",
        )
        .await?;
        Ok(())
    }

    async fn fetch(
        &self,
        target_dir: &Path,
        remote: &str,
        refspec: &str,
        depth: usize,
        ctx: Option<&dyn CommandContext>,
    ) -> Result<()> {
        let depth_str = depth.max(1).to_string();
        run_git_command(
            ctx,
            &[
                "-C",
                &target_dir.to_string_lossy(),
                "fetch",
                "--depth",
                &depth_str,
                remote,
                refspec,
            ],
            None,
            "git fetch",
        )
        .await?;
        Ok(())
    }

    async fn checkout(
        &self,
        target_dir: &Path,
        rev: &str,
        ctx: Option<&dyn CommandContext>,
    ) -> Result<()> {
        run_git_command(
            ctx,
            &[
                "-C",
                &target_dir.to_string_lossy(),
                "checkout",
                "--force",
                rev,
            ],
            None,
            "git checkout",
        )
        .await?;
        Ok(())
    }
}

async fn run_git_command(
    task_ctx: Option<&dyn CommandContext>,
    args: &[&str],
    workdir: Option<&str>,
    label: &str,
) -> Result<std::process::ExitStatus> {
    let mut cmd = Command::new("git");
    cmd.args(args);
    if let Some(dir) = workdir {
        cmd.current_dir(dir);
    }
    // We might want to capture output or pipe it, but for now just wait
    // We pipe stdout/stderr to null or inherit based on need. Ideally quiet.
    cmd.stdout(Stdio::null());
    // keeping stderr for errors might be useful, but let's stick to simple execution for now.

    let mut child = cmd.spawn()?;
    let pid = child.id();

    if let (Some(ctx), Some(pid)) = (task_ctx, pid) {
        // We don't have access to fingerprint here easily unless we duplicate logic,
        // or just pass a label. For simplicity, just use the label.
        ctx.register_child(pid, label.to_string()).await;
    }

    let status = if let Some(ctx) = task_ctx {
        tokio::select! {
            s = child.wait() => s?,
            _ = ctx.cancelled() => {
                let _ = child.kill().await;
                if let (Some(ctx), Some(pid)) = (task_ctx, pid) {
                    ctx.unregister_child(pid).await;
                }
                anyhow::bail!(DomainError::CancelledBySignal);
            }
        }
    } else {
        child.wait().await?
    };

    if let (Some(ctx), Some(pid)) = (task_ctx, pid) {
        ctx.unregister_child(pid).await;
    }

    if !status.success() {
        anyhow::bail!("git command failed: {} (status={})", label, status);
    }

    Ok(status)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::Arc,
        time::{Duration, Instant},
    };

    use super::run_git_command;
    use crate::domain::ports::CommandContext;

    use super::LocalGitAdapter;
    use crate::domain::ports::GitService;
    use async_trait::async_trait;
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

        async fn child_count(&self) -> usize {
            self.children.lock().await.len()
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
    async fn run_git_command_cancelled_and_registry_cleaned() {
        #[cfg(target_os = "windows")]
        let args_owned = vec![
            "-c".to_string(),
            "alias.wait=!powershell -NoProfile -Command \"Start-Sleep -Seconds 8\"".to_string(),
            "wait".to_string(),
        ];
        #[cfg(not(target_os = "windows"))]
        let args_owned = vec![
            "-c".to_string(),
            "alias.wait=!sleep 8".to_string(),
            "wait".to_string(),
        ];
        let args = args_owned.iter().map(|s| s.as_str()).collect::<Vec<_>>();

        let ctx = Arc::new(MockCommandContext::default());
        let started = Instant::now();
        let ((), result) = tokio::join!(
            async {
                tokio::time::sleep(Duration::from_millis(300)).await;
                ctx.cancel();
            },
            async { run_git_command(Some(ctx.as_ref()), &args, None, "git wait").await }
        );
        let elapsed = started.elapsed();

        assert!(result.is_err());
        let msg = format!("{:#}", result.err().expect("cancelled"));
        assert!(msg.contains("operation cancelled by signal"));
        assert!(elapsed < Duration::from_secs(3));
        assert_eq!(ctx.child_count().await, 0);
    }

    #[tokio::test]
    async fn fetch_returns_error_when_git_fails() {
        let adapter = LocalGitAdapter;
        let tmp = std::env::temp_dir().join("prismflow-git-adapter-test");
        let result = adapter
            .fetch(&tmp, "origin", "definitely-not-a-valid-ref", 1, None)
            .await;
        assert!(result.is_err());
    }
}
