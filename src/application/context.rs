use std::collections::HashMap;
use std::time::SystemTime;

use async_trait::async_trait;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::domain::ports::{CommandContext, ProcessManager};

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
        guard
            .iter()
            .map(|(pid, label)| (*pid, label.clone()))
            .collect()
    }

    pub async fn kill_all_children(&self, process_manager: &dyn ProcessManager) -> usize {
        let pids = {
            let guard = self.children.lock().await;
            guard.keys().copied().collect::<Vec<_>>()
        };

        let mut killed = 0usize;
        for pid in pids {
            let ok = process_manager.kill_process_tree(pid);
            if ok {
                killed += 1;
                self.unregister_child(pid).await;
            }
        }
        killed
    }
}

#[async_trait]
impl CommandContext for TaskContext {
    fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    async fn cancelled(&self) {
        self.cancel.cancelled().await;
    }

    async fn register_child(&self, pid: u32, label: String) {
        let mut guard = self.children.lock().await;
        guard.insert(pid, label);
    }

    async fn unregister_child(&self, pid: u32) {
        let mut guard = self.children.lock().await;
        guard.remove(&pid);
    }
}

#[cfg(test)]
mod tests {
    use super::TaskContext;
    use crate::domain::ports::ProcessManager;
    use std::sync::Mutex;

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

    #[derive(Default)]
    struct MockProcessManager {
        calls: Mutex<Vec<u32>>,
    }

    impl ProcessManager for MockProcessManager {
        fn kill_process_tree(&self, pid: u32) -> bool {
            self.calls.lock().expect("lock calls").push(pid);
            true
        }
    }

    #[tokio::test]
    async fn kill_all_children_uses_process_manager_and_cleans_registry() {
        let ctx = TaskContext::new("kill-children-test");
        let pm = MockProcessManager::default();
        ctx.register_child(2001, "a").await;
        ctx.register_child(2002, "b").await;
        assert_eq!(ctx.child_count().await, 2);

        let killed = ctx.kill_all_children(&pm).await;
        assert_eq!(killed, 2);
        assert_eq!(ctx.child_count().await, 0);
        let calls = pm.calls.lock().expect("lock calls").clone();
        assert_eq!(calls.len(), 2);
        assert!(calls.contains(&2001));
        assert!(calls.contains(&2002));
    }
}
