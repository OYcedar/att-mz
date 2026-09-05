//! 目录候选请求的范围验证、来源覆盖与所有权 token。

use super::path_index::{RelativePathIndex, overlapping_later_paths};
use std::error::Error;
use std::fmt;
use std::path::{Component, Path, PathBuf};

/// 目录候选中的一棵冻结来源子树。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DirectorySourceMapping {
    source_directory: PathBuf,
    relative_target: PathBuf,
}

impl DirectorySourceMapping {
    /// 建立一项“来源目录 → 候选内相对目录”映射。
    pub(crate) fn new(
        source_directory: PathBuf,
        relative_target: PathBuf,
    ) -> Result<Self, DirectoryStageRequestError> {
        if source_directory.as_os_str().is_empty() {
            return Err(DirectoryStageRequestError::EmptySourceDirectory);
        }
        // 空目标只在来源映射中表示“把这棵来源目录作为候选根”。Overlay 和显式空目录
        // 仍必须是非空普通相对路径，不能借此表达候选根本身。
        if !relative_target.as_os_str().is_empty() {
            validate_stage_relative_path(&relative_target)?;
        }
        Ok(Self {
            source_directory,
            relative_target,
        })
    }

    pub(crate) fn source_directory(&self) -> &Path {
        &self.source_directory
    }

    pub(crate) fn relative_target(&self) -> &Path {
        &self.relative_target
    }
}

/// 覆盖目录候选中一个相对文件的确定字节。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DirectoryFileOverlay {
    relative_file: PathBuf,
    bytes: Vec<u8>,
}

impl DirectoryFileOverlay {
    pub(crate) fn new(
        relative_file: PathBuf,
        bytes: Vec<u8>,
    ) -> Result<Self, DirectoryStageRequestError> {
        validate_stage_relative_path(&relative_file)?;
        Ok(Self {
            relative_file,
            bytes,
        })
    }

    pub(crate) fn relative_file(&self) -> &Path {
        &self.relative_file
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// 目录候选准备完成后允许执行的唯一发布意图。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DirectoryPublishIntent {
    /// 目标必须尚不存在，并在同名并发中保证至多一个发布者成功。
    CreateNew,
    /// 目标必须是现存目录，并被候选整体替换。
    ReplaceExisting,
}

/// 一次可恢复目录发布的候选准备请求。
///
/// 来源映射构成冻结子树；文件覆盖既可以替换来源子树中的文件，
/// 也可以在来源子树之外建立独立文件。`empty_directories` 则要求候选中至少存在这些目录。
/// 暂存位置、复制策略、
/// 交换恢复、取消清理及资源背压全部属于根实现。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DirectoryStageRequest {
    target_root: PathBuf,
    publish_intent: DirectoryPublishIntent,
    source_mappings: Vec<DirectorySourceMapping>,
    overlays: Vec<DirectoryFileOverlay>,
    empty_directories: Vec<PathBuf>,
}

impl DirectoryStageRequest {
    pub(crate) fn new(
        target_root: PathBuf,
        publish_intent: DirectoryPublishIntent,
        source_mappings: Vec<DirectorySourceMapping>,
        overlays: Vec<DirectoryFileOverlay>,
        empty_directories: Vec<PathBuf>,
    ) -> Result<Self, DirectoryStageRequestError> {
        if target_root.as_os_str().is_empty() {
            return Err(DirectoryStageRequestError::EmptyTargetRoot);
        }
        if source_mappings.is_empty() {
            return Err(DirectoryStageRequestError::EmptySourceMappings);
        }

        let source_paths = source_mappings
            .iter()
            .map(DirectorySourceMapping::relative_target)
            .collect::<Vec<_>>();
        let source_overlaps = overlapping_later_paths(&source_paths);
        if let Some((first, second)) = source_overlaps
            .iter()
            .enumerate()
            .find_map(|(first, second)| second.map(|second| (first, second)))
        {
            return Err(DirectoryStageRequestError::OverlappingSourceTargets {
                first: source_paths[first].to_path_buf(),
                second: source_paths[second].to_path_buf(),
            });
        }
        let source_index = RelativePathIndex::from_paths(&source_paths);

        let overlay_paths = overlays
            .iter()
            .map(DirectoryFileOverlay::relative_file)
            .collect::<Vec<_>>();
        let overlay_overlaps = overlapping_later_paths(&overlay_paths);
        for (index, overlay) in overlays.iter().enumerate() {
            if let Some(other) = overlay_overlaps[index] {
                return Err(DirectoryStageRequestError::OverlappingOverlays {
                    first: overlay.relative_file().to_path_buf(),
                    second: overlays[other].relative_file().to_path_buf(),
                });
            }
            if source_index
                .first_strict_ancestor(overlay.relative_file())
                .is_none()
                && let Some(mapping) = source_index.first_overlapping(overlay.relative_file())
            {
                return Err(DirectoryStageRequestError::OverlayOverlapsSourceTarget {
                    overlay: overlay.relative_file().to_path_buf(),
                    source_target: source_mappings[mapping].relative_target().to_path_buf(),
                });
            }
        }
        let overlay_index = RelativePathIndex::from_paths(&overlay_paths);

        for directory in &empty_directories {
            validate_stage_relative_path(directory)?;
        }
        let empty_paths = empty_directories
            .iter()
            .map(PathBuf::as_path)
            .collect::<Vec<_>>();
        let empty_overlaps = overlapping_later_paths(&empty_paths);
        for (index, directory) in empty_directories.iter().enumerate() {
            if let Some(other) = empty_overlaps[index] {
                return Err(DirectoryStageRequestError::OverlappingEmptyDirectories {
                    first: directory.to_path_buf(),
                    second: empty_directories[other].to_path_buf(),
                });
            }
            if let Some(overlay) = overlay_index.first_overlapping(directory) {
                return Err(DirectoryStageRequestError::EmptyDirectoryOverlapsOverlay {
                    empty_directory: directory.to_path_buf(),
                    overlay: overlays[overlay].relative_file().to_path_buf(),
                });
            }
            if let Some(mapping) = source_index.first_overlapping(directory) {
                return Err(
                    DirectoryStageRequestError::EmptyDirectoryOverlapsSourceTarget {
                        empty_directory: directory.to_path_buf(),
                        source_target: source_mappings[mapping].relative_target().to_path_buf(),
                    },
                );
            }
        }

        Ok(Self {
            target_root,
            publish_intent,
            source_mappings,
            overlays,
            empty_directories,
        })
    }

    pub(crate) fn target_root(&self) -> &Path {
        &self.target_root
    }

    pub(crate) fn publish_intent(&self) -> DirectoryPublishIntent {
        self.publish_intent
    }

    pub(crate) fn source_mappings(&self) -> &[DirectorySourceMapping] {
        &self.source_mappings
    }

    pub(crate) fn overlays(&self) -> &[DirectoryFileOverlay] {
        &self.overlays
    }

    pub(crate) fn empty_directories(&self) -> &[PathBuf] {
        &self.empty_directories
    }
}

fn validate_stage_relative_path(path: &Path) -> Result<(), DirectoryStageRequestError> {
    if path.as_os_str().is_empty()
        || path
            .as_os_str()
            .to_string_lossy()
            .split(['/', '\\'])
            .any(|segment| matches!(segment, "." | ".."))
        || path.components().any(|component| {
            matches!(
                component,
                Component::Prefix(_)
                    | Component::RootDir
                    | Component::ParentDir
                    | Component::CurDir
            )
        })
    {
        return Err(DirectoryStageRequestError::InvalidRelativePath {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

/// 目录候选请求尚未到达根实现前发现的契约错误。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DirectoryStageRequestError {
    EmptyTargetRoot,
    EmptySourceDirectory,
    EmptySourceMappings,
    InvalidRelativePath {
        path: PathBuf,
    },
    OverlappingSourceTargets {
        first: PathBuf,
        second: PathBuf,
    },
    OverlappingOverlays {
        first: PathBuf,
        second: PathBuf,
    },
    OverlappingEmptyDirectories {
        first: PathBuf,
        second: PathBuf,
    },
    OverlayOverlapsSourceTarget {
        overlay: PathBuf,
        source_target: PathBuf,
    },
    EmptyDirectoryOverlapsSourceTarget {
        empty_directory: PathBuf,
        source_target: PathBuf,
    },
    EmptyDirectoryOverlapsOverlay {
        empty_directory: PathBuf,
        overlay: PathBuf,
    },
}

impl fmt::Display for DirectoryStageRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyTargetRoot => write!(formatter, "目录发布目标根目录不能为空"),
            Self::EmptySourceDirectory => write!(formatter, "目录发布来源目录不能为空"),
            Self::EmptySourceMappings => write!(formatter, "目录发布至少需要一棵来源子树"),
            Self::InvalidRelativePath { path } => {
                write!(
                    formatter,
                    "目录发布路径不是安全相对路径：{}",
                    path.display()
                )
            }
            Self::OverlappingSourceTargets { first, second } => write!(
                formatter,
                "目录发布的来源目标子树相互重叠：{} 与 {}",
                first.display(),
                second.display()
            ),
            Self::OverlappingOverlays { first, second } => write!(
                formatter,
                "目录发布的文件覆盖相互重叠：{} 与 {}",
                first.display(),
                second.display()
            ),
            Self::OverlappingEmptyDirectories { first, second } => write!(
                formatter,
                "目录发布的空目录相互重叠：{} 与 {}",
                first.display(),
                second.display()
            ),
            Self::OverlayOverlapsSourceTarget {
                overlay,
                source_target,
            } => write!(
                formatter,
                "目录发布的文件覆盖 {} 与来源目标子树 {} 冲突",
                overlay.display(),
                source_target.display()
            ),
            Self::EmptyDirectoryOverlapsSourceTarget {
                empty_directory,
                source_target,
            } => write!(
                formatter,
                "目录发布的空目录 {} 与来源目标子树 {} 重叠",
                empty_directory.display(),
                source_target.display()
            ),
            Self::EmptyDirectoryOverlapsOverlay {
                empty_directory,
                overlay,
            } => write!(
                formatter,
                "目录发布的空目录 {} 与文件覆盖 {} 重叠",
                empty_directory.display(),
                overlay.display()
            ),
        }
    }
}

impl Error for DirectoryStageRequestError {}

/// 一个已准备但尚未发布的目录候选。
///
/// token 不可复制，只能交回创建它的根实现并被 `publish` 或 `discard`
/// 消费一次。根实现应将实例身份纳入 `state` 并校验归属。
#[derive(Debug, Eq, PartialEq)]
#[must_use = "已准备的目录候选必须发布或丢弃"]
pub(crate) struct StagedDirectory<T> {
    target_root: PathBuf,
    staging_root: PathBuf,
    publish_intent: DirectoryPublishIntent,
    state: T,
}

impl<T> StagedDirectory<T> {
    /// 根实现在准备成功后建立所有权 token。
    pub(crate) fn new(
        target_root: PathBuf,
        staging_root: PathBuf,
        publish_intent: DirectoryPublishIntent,
        state: T,
    ) -> Self {
        Self {
            target_root,
            staging_root,
            publish_intent,
            state,
        }
    }

    pub(crate) fn target_root(&self) -> &Path {
        &self.target_root
    }

    pub(crate) fn staging_root(&self) -> &Path {
        &self.staging_root
    }

    #[cfg(test)]
    pub(crate) fn publish_intent(&self) -> DirectoryPublishIntent {
        self.publish_intent
    }

    /// 仅供创建 token 的根在交付失败时标记清理责任。
    pub(crate) fn state_mut(&mut self) -> &mut T {
        &mut self.state
    }

    /// 仅供与发布根共享候选身份的能力建立受限借用令牌。
    pub(crate) fn state(&self) -> &T {
        &self.state
    }

    pub(crate) fn into_parts(self) -> (PathBuf, PathBuf, DirectoryPublishIntent, T) {
        (
            self.target_root,
            self.staging_root,
            self.publish_intent,
            self.state,
        )
    }
}

#[cfg(test)]
mod tests;
