use std::collections::HashMap;
use std::process::Command;
use std::time::SystemTime;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[allow(dead_code)]
#[derive(Debug)]
pub struct TaskContext {
    run_id: String,
    started_at: SystemTime,
    deadline: Option<SystemTime>,
    cancel: CancellationToken,
    children: Mutex<HashMap<u32, String>>,
}

#[allow(dead_code)]
impl TaskContext {
    pub fn new(run_tag: &str) -> Self {
        let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
        Self {
            run_id: format!("{run_tag}-{ts}"),
            started_at: SystemTime::now(),
            deadline: None,
            cancel: CancellationToken::new(),
            children: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_deadline(run_tag: &str, deadline: SystemTime) -> Self {
        let mut ctx = Self::new(run_tag);
        ctx.deadline = Some(deadline);
        ctx
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn started_at(&self) -> SystemTime {
        self.started_at
    }

    pub fn deadline(&self) -> Option<SystemTime> {
        self.deadline
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    pub async fn cancelled(&self) {
        self.cancel.cancelled().await;
    }

    pub async fn register_child(&self, pid: u32, label: impl Into<String>) {
        let mut guard = self.children.lock().await;
        guard.insert(pid, label.into());
    }

    pub async fn unregister_child(&self, pid: u32) {
        let mut guard = self.children.lock().await;
        guard.remove(&pid);
    }

    pub async fn child_count(&self) -> usize {
        self.children.lock().await.len()
    }

    pub async fn list_children(&self) -> Vec<(u32, String)> {
        let guard = self.children.lock().await;
        guard.iter().map(|(pid, label)| (*pid, label.clone())).collect()
    }

    pub async fn kill_all_children(&self) -> usize {
        let pids = {
            let guard = self.children.lock().await;
            guard.keys().copied().collect::<Vec<_>>()
        };

        let mut killed = 0usize;
        for pid in pids {
            let ok = terminate_process(pid);
            if ok {
                killed += 1;
                self.unregister_child(pid).await;
            }
        }
        killed
    }
}

#[allow(dead_code)]
fn terminate_process(pid: u32) -> bool {
    #[cfg(target_os = "windows")]
    {
        Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "windows"))]
    {
        Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::TaskContext;
    use std::process::Command;
    use std::time::Duration;

    #[tokio::test]
    async fn cancel_flag_works() {
        let ctx = TaskContext::new("test");
        assert!(!ctx.is_cancelled());
        ctx.cancel();
        assert!(ctx.is_cancelled());
    }

    #[tokio::test]
    async fn child_registry_works() {
        let ctx = TaskContext::new("test");
        ctx.register_child(1001, "cmd-a").await;
        ctx.register_child(1002, "cmd-b").await;
        assert_eq!(ctx.child_count().await, 2);
        ctx.unregister_child(1001).await;
        assert_eq!(ctx.child_count().await, 1);
    }

    #[tokio::test]
    async fn kill_all_children_terminates_registered_processes() {
        let ctx = TaskContext::new("kill-children-test");
        #[cfg(target_os = "windows")]
        let mut child = Command::new("powershell")
            .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 20"])
            .spawn()
            .expect("spawn child");
        #[cfg(not(target_os = "windows"))]
        let mut child = Command::new("sh")
            .args(["-lc", "sleep 20"])
            .spawn()
            .expect("spawn child");

        let pid = child.id();
        ctx.register_child(pid, "test-sleep").await;
        assert_eq!(ctx.child_count().await, 1);

        let killed = ctx.kill_all_children().await;
        assert!(killed >= 1);
        assert_eq!(ctx.child_count().await, 0);

        for _ in 0..20 {
            if child.try_wait().expect("try_wait").is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("child process did not exit after kill_all_children");
    }
}
