use std::{
    fs,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::domain::ports::FileSystem;

const CACHE_SCAN_DIRS: [&str; 2] = ["tmp-diffs", "tmp-ci"];
const REPO_CACHE_DIRS: [&str; 2] = ["ci-repo-cache", "repo-cache"];

/// 缓存清理统计信息
#[derive(Debug, Default, Clone, Copy)]
pub struct CacheCleanupStats {
    pub checked_files: usize,
    pub stale_files: usize,
    pub removed_files: usize,
    pub remove_failures: usize,
    pub checked_repo_entries: usize,
    pub stale_repo_entries: usize,
    pub removed_repo_entries: usize,
    pub repo_remove_failures: usize,
}

impl CacheCleanupStats {
    pub fn has_changes(&self) -> bool {
        self.stale_files > 0
            || self.remove_failures > 0
            || self.stale_repo_entries > 0
            || self.repo_remove_failures > 0
    }

    pub fn to_log_string(&self, root: &Path, cache_cleanup_hours: u64) -> String {
        format!(
            "cache_check root={} cleanup_hours={} checked_files={} stale_files={} removed_files={} remove_failures={} checked_repo_entries={} stale_repo_entries={} removed_repo_entries={} repo_remove_failures={}",
            root.join(".prismflow").display(),
            cache_cleanup_hours,
            self.checked_files,
            self.stale_files,
            self.removed_files,
            self.remove_failures,
            self.checked_repo_entries,
            self.stale_repo_entries,
            self.removed_repo_entries,
            self.repo_remove_failures
        )
    }
}

/// 缓存清理服务
pub struct CacheCleanupService<'a> {
    fs: &'a dyn FileSystem,
}

impl<'a> CacheCleanupService<'a> {
    pub fn new(fs: &'a dyn FileSystem) -> Self {
        Self { fs }
    }

    /// 检查并清理过期缓存
    pub fn cleanup_if_needed(&self, cache_cleanup_hours: u64) {
        let cwd = match self.fs.current_dir() {
            Ok(v) => v,
            Err(_) => return,
        };

        let stale_after =
            Duration::from_secs(cache_cleanup_hours.saturating_mul(60).saturating_mul(60));

        let stats = self.cleanup_stale_cache_under(&cwd, stale_after, SystemTime::now());

        if stats.has_changes() {
            println!("{}", stats.to_log_string(&cwd, cache_cleanup_hours));
        }
    }

    fn cleanup_stale_cache_under(
        &self,
        cwd: &Path,
        stale_after: Duration,
        now: SystemTime,
    ) -> CacheCleanupStats {
        let root = cwd.join(".prismflow");
        let mut stats = CacheCleanupStats::default();

        for dir in CACHE_SCAN_DIRS {
            self.walk_and_cleanup_stale_files(&root.join(dir), stale_after, now, &mut stats);
        }

        for dir in REPO_CACHE_DIRS {
            self.cleanup_stale_repo_cache_entries(&root.join(dir), stale_after, now, &mut stats);
        }

        stats
    }

    fn walk_and_cleanup_stale_files(
        &self,
        dir: &Path,
        stale_after: Duration,
        now: SystemTime,
        stats: &mut CacheCleanupStats,
    ) {
        let entries = match fs::read_dir(dir) {
            Ok(v) => v,
            Err(_) => return,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(v) => v,
                Err(_) => continue,
            };

            if file_type.is_dir() {
                self.walk_and_cleanup_stale_files(&path, stale_after, now, stats);
                continue;
            }

            if !file_type.is_file() {
                continue;
            }

            stats.checked_files += 1;

            let metadata = match entry.metadata() {
                Ok(v) => v,
                Err(_) => continue,
            };

            let modified = match metadata.modified() {
                Ok(v) => v,
                Err(_) => continue,
            };

            let age = match now.duration_since(modified) {
                Ok(v) => v,
                Err(_) => continue,
            };

            if age < stale_after {
                continue;
            }

            stats.stale_files += 1;
            if fs::remove_file(&path).is_ok() {
                stats.removed_files += 1;
            } else {
                stats.remove_failures += 1;
            }
        }
    }

    fn cleanup_stale_repo_cache_entries(
        &self,
        root: &Path,
        stale_after: Duration,
        now: SystemTime,
        stats: &mut CacheCleanupStats,
    ) {
        let entries = match fs::read_dir(root) {
            Ok(v) => v,
            Err(_) => return,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(v) => v,
                Err(_) => continue,
            };

            if !(file_type.is_file() || file_type.is_dir()) {
                continue;
            }

            stats.checked_repo_entries += 1;

            let modified = self
                .cache_entry_timestamp(&path)
                .or_else(|| entry.metadata().ok().and_then(|m| m.modified().ok()));

            let Some(modified) = modified else {
                continue;
            };

            let age = match now.duration_since(modified) {
                Ok(v) => v,
                Err(_) => continue,
            };

            if age < stale_after {
                continue;
            }

            stats.stale_repo_entries += 1;
            let removed = if file_type.is_dir() {
                fs::remove_dir_all(&path).is_ok()
            } else {
                fs::remove_file(&path).is_ok()
            };

            if removed {
                stats.removed_repo_entries += 1;
            } else {
                stats.repo_remove_failures += 1;
            }
        }
    }

    fn cache_entry_timestamp(&self, path: &Path) -> Option<SystemTime> {
        let name = path.file_name()?.to_str()?;
        let (_, ts_part) = name.rsplit_once('_')?;
        let ts = ts_part.parse::<u64>().ok()?;
        UNIX_EPOCH.checked_add(Duration::from_secs(ts))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_cache_cleanup_stats_has_changes() {
        let mut stats = CacheCleanupStats::default();
        assert!(!stats.has_changes());

        stats.stale_files = 1;
        assert!(stats.has_changes());
    }

    #[test]
    fn test_cache_cleanup_stats_to_log_string() {
        let stats = CacheCleanupStats::default();
        let root = PathBuf::from("/test");
        let log = stats.to_log_string(&root, 48);
        assert!(log.contains("cleanup_hours=48"));
    }
}
