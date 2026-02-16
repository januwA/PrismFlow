use std::process::Command;

use crate::domain::ports::ProcessManager;

#[derive(Debug, Default, Clone, Copy)]
pub struct OsProcessManager;

impl ProcessManager for OsProcessManager {
    fn kill_process_tree(&self, pid: u32) -> bool {
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
}
