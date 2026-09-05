use super::error::SystemFileSystemError;
use super::workspace::publication_workspace_root;
use super::{DirectoryPublisherConfig, SystemDirectoryPublisher, SystemFileSystem};
use crate::diagnostic::{
    DiagnosticReport, FileSystemDiagnosticContext, FileSystemDiagnosticStage, FileSystemOperation,
    StateEffect,
};
use crate::runtime::performance::RunPerformanceCounters;
use crate::storage::file_system::{
    DirectoryPublishIntent, DirectorySourceMapping, DirectoryStageRequest,
};
use std::io;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(super) fn single_managed_artifact(target: &Path, name: &str) -> PathBuf {
    let workspace = publication_workspace_root(
        target.parent().expect("测试目标必有父目录"),
        target.file_name().expect("测试目标必有名称"),
    );
    let artifact = workspace.join(name);
    assert!(artifact.exists(), "测试现场应存在 {name} 产物");
    artifact
}

#[derive(Clone)]
pub(super) struct TestDirectoryPublisher {
    pub(super) file_system: SystemFileSystem,
    publisher: SystemDirectoryPublisher,
    performance: Arc<RunPerformanceCounters>,
}

impl TestDirectoryPublisher {
    pub(super) fn new() -> Self {
        Self::with_publisher_config(2, publisher_config())
    }

    pub(super) fn with_publisher_config(
        worker_threads: usize,
        publisher_config: DirectoryPublisherConfig,
    ) -> Self {
        let performance = Arc::new(RunPerformanceCounters::default());
        let file_system =
            SystemFileSystem::new_with_worker_threads(worker_threads, Arc::clone(&performance))
                .expect("应该可建立文件系统根");
        let publisher = file_system.directory_publisher(publisher_config);
        Self {
            file_system,
            publisher,
            performance,
        }
    }

    pub(super) fn candidate_validation_counts(&self) -> (u64, u64) {
        let count = self.performance.snapshot().candidate_validations;
        (count.started, count.completed)
    }

    pub(super) async fn shutdown(&self) -> Result<(), SystemFileSystemError> {
        self.file_system.shutdown().await
    }
}

impl Deref for TestDirectoryPublisher {
    type Target = SystemDirectoryPublisher;

    fn deref(&self) -> &Self::Target {
        &self.publisher
    }
}

pub(super) fn symlink_unavailable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::PermissionDenied | io::ErrorKind::Unsupported
    ) || error.raw_os_error() == Some(1314)
}

pub(super) fn publisher_config() -> DirectoryPublisherConfig {
    publisher_config_for_lock_directory(
        std::env::temp_dir().join("filesystem-test-publisher-locks"),
    )
}

pub(super) fn publisher_config_for_lock_directory(
    lock_directory: PathBuf,
) -> DirectoryPublisherConfig {
    DirectoryPublisherConfig::production(lock_directory).expect("测试发布配置应该合法")
}

pub(super) fn stage_request(
    target: PathBuf,
    source: PathBuf,
    intent: DirectoryPublishIntent,
) -> DirectoryStageRequest {
    DirectoryStageRequest::new(
        target,
        intent,
        vec![
            DirectorySourceMapping::new(source, PathBuf::from("snapshot/content"))
                .expect("测试来源映射应该合法"),
        ],
        Vec::new(),
        vec![PathBuf::from("empty")],
    )
    .expect("测试候选请求应该合法")
}

pub(super) fn canonical_target(path: &Path) -> PathBuf {
    path.parent()
        .expect("测试目标应有父目录")
        .canonicalize()
        .expect("测试目标父目录应可规范化")
        .join(path.file_name().expect("测试目标应有名称"))
}

pub(super) fn subprocess_command(
    mode: &str,
    target: &Path,
    source: &Path,
) -> std::process::Command {
    let mut command =
        std::process::Command::new(std::env::current_exe().expect("应可定位当前测试进程"));
    command
        .arg("--exact")
        .arg("runtime::filesystem::publication::tests::publisher_subprocess_entrypoint")
        .arg("--nocapture")
        .env("FILESYSTEM_PUBLISHER_CHILD_MODE", mode)
        .env("FILESYSTEM_PUBLISHER_CHILD_TARGET", target)
        .env("FILESYSTEM_PUBLISHER_CHILD_SOURCE", source)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    command
}

pub(super) fn init_recovery_report(error: &SystemFileSystemError) -> DiagnosticReport {
    error.diagnostic_report(
        FileSystemDiagnosticContext::new(
            FileSystemDiagnosticStage::Init,
            FileSystemOperation::RecoverTarget,
        ),
        StateEffect::Unchanged,
    )
}
