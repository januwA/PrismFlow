use std::path::PathBuf;

use anyhow::Result;
use chrono::Utc;
use serde::Serialize;

use super::review_workflow::RepoReviewStats;
use crate::domain::ports::FileSystem;

/// 审查报告数据
#[derive(Debug, Serialize)]
struct ReviewReport<'a> {
    generated_at: String,
    mode: &'a str,
    total_processed: usize,
    total_failed_retryable: usize,
    total_failed_fatal: usize,
    repos: &'a [RepoReviewStats],
}

/// 审查报告服务
pub struct ReviewReportService<'a> {
    fs: &'a dyn FileSystem,
}

impl<'a> ReviewReportService<'a> {
    pub fn new(fs: &'a dyn FileSystem) -> Self {
        Self { fs }
    }

    /// 写入审查报告
    pub fn write_review_report(
        &self,
        mode: &str,
        stats: &[RepoReviewStats],
        archive_on_failure_only: bool,
    ) -> Result<PathBuf> {
        let report = ReviewReport {
            generated_at: Utc::now().to_rfc3339(),
            mode,
            total_processed: stats.iter().map(|s| s.processed).sum(),
            total_failed_retryable: stats.iter().map(|s| s.failed_retryable).sum(),
            total_failed_fatal: stats.iter().map(|s| s.failed_fatal).sum(),
            repos: stats,
        };

        let root = self.fs.current_dir()?.join(".prismflow").join("reports");
        self.fs.create_dir_all(&root)?;

        let latest = root.join("latest-review-report.json");
        let timestamped = root.join(format!(
            "review-report-{}.json",
            Utc::now().format("%Y%m%d-%H%M%S")
        ));

        let raw = serde_json::to_string_pretty(&report)?;
        self.fs.write(&latest, raw.as_bytes())?;

        if self.should_archive(stats, archive_on_failure_only) {
            self.fs.write(&timestamped, raw.as_bytes())?;
            return Ok(timestamped);
        }

        Ok(latest)
    }

    fn should_archive(&self, stats: &[RepoReviewStats], archive_on_failure_only: bool) -> bool {
        !archive_on_failure_only || self.has_review_failures(stats)
    }

    fn has_review_failures(&self, stats: &[RepoReviewStats]) -> bool {
        stats
            .iter()
            .any(|s| s.failed_retryable > 0 || s.failed_fatal > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashMap, path::Path};

    struct MockFileSystem {
        files: HashMap<PathBuf, Vec<u8>>,
        current_dir: PathBuf,
    }

    impl MockFileSystem {
        fn new() -> Self {
            Self {
                files: HashMap::new(),
                current_dir: PathBuf::from("/test"),
            }
        }
    }

    impl FileSystem for MockFileSystem {
        fn create_dir_all(&self, _path: &Path) -> Result<()> {
            Ok(())
        }

        fn write(&self, path: &Path, content: &[u8]) -> Result<()> {
            use std::sync::Mutex;
            // 简单实现，实际测试中不需要真正写入
            Ok(())
        }

        fn read_to_string(&self, _path: &Path) -> Result<String> {
            Ok(String::new())
        }

        fn current_dir(&self) -> Result<PathBuf> {
            Ok(self.current_dir.clone())
        }

        fn config_dir(&self) -> Option<PathBuf> {
            Some(PathBuf::from("/config"))
        }

        fn exists(&self, _path: &Path) -> bool {
            false
        }
    }

    #[test]
    fn test_has_review_failures_detects_no_failure() {
        let stats = vec![RepoReviewStats {
            repo: "owner/repo".to_string(),
            processed: 1,
            ..RepoReviewStats::default()
        }];

        let fs = MockFileSystem::new();
        let service = ReviewReportService::new(&fs);
        assert!(!service.has_review_failures(&stats));
    }

    #[test]
    fn test_has_review_failures_detects_retryable_or_fatal() {
        let retryable = vec![RepoReviewStats {
            repo: "owner/repo".to_string(),
            failed_retryable: 1,
            ..RepoReviewStats::default()
        }];

        let fs = MockFileSystem::new();
        let service = ReviewReportService::new(&fs);
        assert!(service.has_review_failures(&retryable));

        let fatal = vec![RepoReviewStats {
            repo: "owner/repo".to_string(),
            failed_fatal: 1,
            ..RepoReviewStats::default()
        }];
        assert!(service.has_review_failures(&fatal));
    }
}
