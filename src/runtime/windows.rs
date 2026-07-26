//! Windows 文件身份、卷约束、独占锁与无覆盖重命名。

use std::ffi::OsStr;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io;
use std::mem::{offset_of, size_of};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use windows_sys::Win32::Foundation::{ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION, HANDLE};
use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_CASE_SENSITIVE_INFO,
    FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO,
    FILE_READ_ATTRIBUTES, FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FileCaseSensitiveInfo, FileDispositionInfo, FileIdInfo, FileRenameInfo, GetDriveTypeW,
    GetFileInformationByHandle, GetFileInformationByHandleEx, GetVolumeInformationByHandleW,
    GetVolumePathNameW, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
    SetFileInformationByHandle, UnlockFileEx,
};
use windows_sys::Win32::System::IO::OVERLAPPED;
use windows_sys::Win32::System::SystemServices::FILE_CS_FLAG_CASE_SENSITIVE_DIR;
use windows_sys::Win32::System::WindowsProgramming::DRIVE_FIXED;

use crate::diagnostic::{
    DiagnosticAction, DiagnosticCode, DiagnosticFailureKind, DiagnosticImpact, DiagnosticReason,
    DiagnosticStage, DiagnosticSubject, RecoveryFact, SafeDiagnostic,
};

/// Win32 文件系统边界保留的精确失败。
#[derive(Debug)]
pub(crate) enum WindowsFsError {
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    ReparsePoint {
        path: PathBuf,
    },
    NonLocalVolume {
        path: PathBuf,
    },
    NonNtfsVolume {
        path: PathBuf,
        actual: String,
    },
    CaseSensitiveDirectory {
        path: PathBuf,
    },
    LockCancelled {
        path: PathBuf,
    },
    RenameTargetExists {
        path: PathBuf,
    },
    FileIdentityChanged {
        path: PathBuf,
    },
    Cryptography {
        operation: &'static str,
        status: i32,
    },
}

impl WindowsFsError {
    /// 在仍持有 Win32/NTSTATUS 类型化事实的位置建立公开诊断，不读取 `Display`。
    pub(crate) fn safe_diagnostic(
        &self,
        code: DiagnosticCode,
        stage: DiagnosticStage,
        impact: DiagnosticImpact,
        fallback_action: DiagnosticAction,
    ) -> SafeDiagnostic {
        match self {
            Self::Io {
                operation,
                path,
                source,
            } => SafeDiagnostic::io(
                code,
                stage,
                DiagnosticSubject::path(path),
                operation,
                source,
                impact,
                DiagnosticAction::CheckPathAndPermissions,
            ),
            Self::ReparsePoint { path } => SafeDiagnostic::new(
                code,
                stage,
                DiagnosticSubject::path(path),
                DiagnosticReason::failure(DiagnosticFailureKind::ReparsePointForbidden),
                impact,
                fallback_action,
            ),
            Self::NonLocalVolume { path } => SafeDiagnostic::new(
                code,
                stage,
                DiagnosticSubject::path(path),
                DiagnosticReason::failure(DiagnosticFailureKind::NonLocalVolume),
                impact,
                fallback_action,
            ),
            Self::NonNtfsVolume { path, actual } => SafeDiagnostic::new(
                code,
                stage,
                DiagnosticSubject::path(path),
                DiagnosticReason::failure(DiagnosticFailureKind::NonNtfsVolume),
                impact,
                fallback_action,
            )
            .with_recovery(RecoveryFact::component(format!("filesystem={actual}"))),
            Self::CaseSensitiveDirectory { path } => SafeDiagnostic::new(
                code,
                stage,
                DiagnosticSubject::path(path),
                DiagnosticReason::failure(DiagnosticFailureKind::CaseSensitiveDirectory),
                impact,
                fallback_action,
            ),
            Self::LockCancelled { path } => SafeDiagnostic::new(
                code,
                stage,
                DiagnosticSubject::path(path),
                DiagnosticReason::failure(DiagnosticFailureKind::LockCancelled),
                impact,
                DiagnosticAction::Retry,
            ),
            Self::RenameTargetExists { path } => SafeDiagnostic::new(
                code,
                stage,
                DiagnosticSubject::path(path),
                DiagnosticReason::failure(DiagnosticFailureKind::TargetAlreadyExists),
                impact,
                fallback_action,
            ),
            Self::FileIdentityChanged { path } => SafeDiagnostic::new(
                code,
                stage,
                DiagnosticSubject::path(path),
                DiagnosticReason::failure(DiagnosticFailureKind::FileIdentityChanged),
                impact,
                fallback_action,
            ),
            Self::Cryptography { operation, status } => SafeDiagnostic::new(
                code,
                stage,
                DiagnosticSubject::operation(operation),
                DiagnosticReason::WindowsStatus {
                    operation: (*operation).to_owned(),
                    status: *status,
                },
                impact,
                DiagnosticAction::Retry,
            ),
        }
    }
}

impl fmt::Display for WindowsFsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                operation,
                path,
                source,
            } => write!(formatter, "{operation} {} 失败：{source}", path.display()),
            Self::ReparsePoint { path } => {
                write!(
                    formatter,
                    "路径是被禁止的 reparse point：{}",
                    path.display()
                )
            }
            Self::NonLocalVolume { path } => {
                write!(formatter, "路径不位于本机固定卷：{}", path.display())
            }
            Self::NonNtfsVolume { path, actual } => write!(
                formatter,
                "路径不位于 NTFS 卷（实际：{actual}）：{}",
                path.display()
            ),
            Self::CaseSensitiveDirectory { path } => {
                write!(formatter, "目录启用了大小写敏感语义：{}", path.display())
            }
            Self::LockCancelled { path } => {
                write!(formatter, "等待文件锁已取消：{}", path.display())
            }
            Self::RenameTargetExists { path } => {
                write!(formatter, "无覆盖重命名的目标已经存在：{}", path.display())
            }
            Self::FileIdentityChanged { path } => {
                write!(
                    formatter,
                    "待重命名对象的文件身份已经变化：{}",
                    path.display()
                )
            }
            Self::Cryptography { operation, status } => {
                write!(formatter, "{operation} 失败（NTSTATUS {status:#010x}）")
            }
        }
    }
}

impl std::error::Error for WindowsFsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::ReparsePoint { .. }
            | Self::NonLocalVolume { .. }
            | Self::NonNtfsVolume { .. }
            | Self::CaseSensitiveDirectory { .. }
            | Self::LockCancelled { .. }
            | Self::RenameTargetExists { .. }
            | Self::FileIdentityChanged { .. }
            | Self::Cryptography { .. } => None,
        }
    }
}

/// 使用 Windows 系统首选 CSPRNG 生成可失败的 UUID v4。
pub(crate) fn secure_uuid_v4(operation: &'static str) -> Result<Uuid, WindowsFsError> {
    let mut bytes = [0_u8; 16];
    // SAFETY: 系统首选 RNG 不需要算法句柄，缓冲区在调用期间有效且长度准确。
    let status = unsafe {
        BCryptGenRandom(
            ptr::null_mut(),
            bytes.as_mut_ptr(),
            bytes.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status != 0 {
        return Err(WindowsFsError::Cryptography { operation, status });
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(Uuid::from_bytes(bytes))
}

fn io_error(operation: &'static str, path: &Path, source: io::Error) -> WindowsFsError {
    WindowsFsError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

fn wide_nul(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

fn handle(file: &File) -> HANDLE {
    file.as_raw_handle() as HANDLE
}

/// 一条已经逐组件打开并固定身份的现存路径。
///
/// `handles` 按根目录到最终对象的顺序持有无删除共享句柄。对象存活期间，路径链
/// 中的任一目录都不能被重命名或替换；最终对象也不能在规范化和后续检查之间被
/// 换成 reparse point。
pub(crate) struct PinnedPath {
    resolved_path: PathBuf,
    handles: Vec<File>,
}

impl PinnedPath {
    pub(crate) fn resolved_path(&self) -> &Path {
        &self.resolved_path
    }

    pub(crate) fn file(&self) -> &File {
        self.handles
            .last()
            .expect("受信的路径固定结果至少持有最终对象句柄")
    }

    pub(crate) fn file_mut(&mut self) -> &mut File {
        self.handles
            .last_mut()
            .expect("受信的路径固定结果至少持有最终对象句柄")
    }

    pub(crate) fn metadata(&self) -> Result<std::fs::Metadata, WindowsFsError> {
        self.file()
            .metadata()
            .map_err(|source| io_error("读取固定对象元数据", &self.resolved_path, source))
    }

    pub(crate) fn component_identities(&self) -> Result<Vec<FileIdentity>, WindowsFsError> {
        self.handles
            .iter()
            .map(|file| FileIdentity::of(file, &self.resolved_path))
            .collect()
    }
}

/// 一个卷内稳定的文件身份。
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub(crate) struct FileIdentity {
    volume_serial_number: u64,
    file_id: [u8; 16],
}

impl FileIdentity {
    #[cfg(test)]
    pub(crate) const fn from_parts(volume_serial_number: u64, file_id: [u8; 16]) -> Self {
        Self {
            volume_serial_number,
            file_id,
        }
    }

    pub(crate) fn stable_hex(&self) -> String {
        let mut value = format!("{:016x}", self.volume_serial_number);
        for byte in self.file_id {
            use std::fmt::Write as _;
            write!(&mut value, "{byte:02x}").expect("写入 String 不会失败");
        }
        value
    }

    pub(crate) fn of(file: &File, path: &Path) -> Result<Self, WindowsFsError> {
        let mut info = FILE_ID_INFO::default();
        // SAFETY: `file` 在调用期间保持有效；输出缓冲区具有正确类型、大小与对齐。
        let succeeded = unsafe {
            GetFileInformationByHandleEx(
                handle(file),
                FileIdInfo,
                (&raw mut info).cast(),
                size_of::<FILE_ID_INFO>() as u32,
            )
        };
        if succeeded == 0 {
            return Err(io_error("读取文件身份", path, io::Error::last_os_error()));
        }
        Ok(Self {
            volume_serial_number: info.VolumeSerialNumber,
            file_id: info.FileId.Identifier,
        })
    }
}

/// 读取已打开文件的硬链接数，不通过路径重新定位对象。
pub(crate) fn number_of_links(file: &File, path: &Path) -> Result<u32, WindowsFsError> {
    // SAFETY: `BY_HANDLE_FILE_INFORMATION` 是 Win32 的纯输出 POD 结构，零初始化对所有字段都有效。
    let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: 文件句柄在调用期间有效，输出指针指向完整可写结构。
    let succeeded =
        unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut information) };
    if succeeded == 0 {
        return Err(io_error(
            "读取文件硬链接数",
            path,
            io::Error::last_os_error(),
        ));
    }
    Ok(information.nNumberOfLinks)
}

/// 以不跟随 reparse point 的方式打开目录。
pub(crate) fn open_directory(path: &Path, share_delete: bool) -> Result<File, WindowsFsError> {
    let share_mode =
        FILE_SHARE_READ | FILE_SHARE_WRITE | (u32::from(share_delete) * FILE_SHARE_DELETE);
    let file = OpenOptions::new()
        .read(true)
        .share_mode(share_mode)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|source| io_error("打开目录", path, source))?;
    reject_reparse(&file, path)?;
    Ok(file)
}

/// 以不跟随 reparse point 的方式打开普通文件。
#[cfg(test)]
pub(crate) fn open_regular_file(path: &Path, share_delete: bool) -> Result<File, WindowsFsError> {
    let share_mode =
        FILE_SHARE_READ | FILE_SHARE_WRITE | (u32::from(share_delete) * FILE_SHARE_DELETE);
    let file = OpenOptions::new()
        .read(true)
        .share_mode(share_mode)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|source| io_error("打开文件", path, source))?;
    reject_reparse(&file, path)?;
    let metadata = file
        .metadata()
        .map_err(|source| io_error("读取文件元数据", path, source))?;
    if !metadata.is_file() {
        return Err(io_error(
            "确认普通文件",
            path,
            io::Error::new(io::ErrorKind::InvalidInput, "目标不是普通文件"),
        ));
    }
    Ok(file)
}

fn open_any_path(path: &Path, share_delete: bool) -> Result<File, WindowsFsError> {
    let share_mode =
        FILE_SHARE_READ | FILE_SHARE_WRITE | (u32::from(share_delete) * FILE_SHARE_DELETE);
    let file = OpenOptions::new()
        .read(true)
        .share_mode(share_mode)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|source| io_error("打开路径", path, source))?;
    reject_reparse(&file, path)?;
    Ok(file)
}

/// 从卷根开始逐组件打开路径，并拒绝路径链中任一 reparse point。
///
/// 所有组件均以不跟随 reparse 的方式打开，且不共享删除权限。返回值持续持有整条
/// 路径链的句柄，因此校验完成后到调用方完成本次同步操作前，不存在父目录被替换
/// 后再通过原字符串路径悄然穿越 junction、mount point 或符号链接的窗口。
pub(crate) fn pin_path_without_reparse(path: &Path) -> Result<PinnedPath, WindowsFsError> {
    pin_path_with_final_opener(path, open_shared_final_path)
}

fn open_shared_final_path(path: &Path) -> Result<File, WindowsFsError> {
    open_any_path(path, false)
}

/// 固定一条普通文件路径，并在句柄存活期间拒绝其他写入者和删除者。
///
/// 该能力用于建立内容指纹和复制目录候选；普通路径 pin 只固定身份，仍允许共享
/// 写入，不能承载稳定字节观察语义。
pub(crate) fn pin_regular_file_for_snapshot_read(
    path: &Path,
) -> Result<PinnedPath, WindowsFsError> {
    let pinned = pin_path_with_final_opener(path, open_snapshot_read_file)?;
    if !pinned.metadata()?.is_file() {
        return Err(io_error(
            "确认快照读取文件",
            pinned.resolved_path(),
            io::Error::new(io::ErrorKind::InvalidInput, "目标不是普通文件"),
        ));
    }
    Ok(pinned)
}

fn open_snapshot_read_file(path: &Path) -> Result<File, WindowsFsError> {
    let file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|source| io_error("打开稳定快照文件", path, source))?;
    reject_reparse(&file, path)?;
    Ok(file)
}

fn pin_path_with_final_opener(
    path: &Path,
    open_final: fn(&Path) -> Result<File, WindowsFsError>,
) -> Result<PinnedPath, WindowsFsError> {
    let absolute =
        std::path::absolute(path).map_err(|source| io_error("建立绝对路径", path, source))?;
    let components = absolute.ancestors().collect::<Vec<_>>();
    if components.is_empty() {
        return Err(io_error(
            "固定路径",
            &absolute,
            io::Error::new(io::ErrorKind::InvalidInput, "路径没有可打开组件"),
        ));
    }

    let mut handles = Vec::new();
    handles
        .try_reserve_exact(components.len())
        .map_err(|source| {
            io_error(
                "分配路径句柄",
                &absolute,
                io::Error::new(io::ErrorKind::OutOfMemory, source),
            )
        })?;
    for (index, component) in components.iter().rev().enumerate() {
        let is_final = index + 1 == components.len();
        let opened = if is_final {
            open_final(component)?
        } else {
            open_directory(component, false)?
        };
        handles.push(opened);
    }

    // 整条路径链及最终对象均已用无删除共享句柄固定，此时规范化不会穿越一个在
    // 校验后被替换的对象。
    let resolved_path = absolute
        .canonicalize()
        .map_err(|source| io_error("规范化固定路径", &absolute, source))?;
    Ok(PinnedPath {
        resolved_path,
        handles,
    })
}

/// 固定一条无 reparse 组件的目录路径。
pub(crate) fn pin_directory_without_reparse(path: &Path) -> Result<PinnedPath, WindowsFsError> {
    let pinned = pin_path_without_reparse(path)?;
    if !pinned.metadata()?.is_dir() {
        return Err(io_error(
            "确认固定目录",
            pinned.resolved_path(),
            io::Error::new(io::ErrorKind::InvalidInput, "目标不是目录"),
        ));
    }
    Ok(pinned)
}

/// 从现存卷根开始逐段建立目录，并拒绝路径链中的任一 reparse point。
///
/// 每个已经确认或新建的目录都会立即以无删除共享句柄固定，随后才处理下一段。
/// 因而缺失后缀不会先通过一个尚未检查的 junction、mount point 或符号链接被建立。
pub(crate) fn create_directories_without_reparse(
    path: &Path,
) -> Result<PinnedPath, WindowsFsError> {
    let absolute =
        std::path::absolute(path).map_err(|source| io_error("建立绝对目录路径", path, source))?;
    let components = absolute.ancestors().collect::<Vec<_>>();
    if components.is_empty() {
        return Err(io_error(
            "建立目录路径",
            &absolute,
            io::Error::new(io::ErrorKind::InvalidInput, "目录路径没有可建立组件"),
        ));
    }
    let mut handles = Vec::new();
    handles
        .try_reserve_exact(components.len())
        .map_err(|source| {
            io_error(
                "分配目录路径句柄",
                &absolute,
                io::Error::new(io::ErrorKind::OutOfMemory, source),
            )
        })?;
    for component in components.iter().rev() {
        let opened = match open_directory(component, false) {
            Ok(opened) => opened,
            Err(WindowsFsError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                match std::fs::create_dir(component) {
                    Ok(()) => {}
                    Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(source) => return Err(io_error("建立目录组件", component, source)),
                }
                open_directory(component, false)?
            }
            Err(source) => return Err(source),
        };
        handles.push(opened);
    }
    let resolved_path = absolute
        .canonicalize()
        .map_err(|source| io_error("规范化已建立目录", &absolute, source))?;
    Ok(PinnedPath {
        resolved_path,
        handles,
    })
}

/// 以读写方式打开或建立普通文件，并固定其无 reparse 父路径链。
///
/// 返回对象持有父目录和最终文件的无删除共享句柄。需要对文件执行轮转或重命名时，
/// 调用方必须先释放返回对象。
pub(crate) fn open_read_write_file_without_reparse(
    path: &Path,
    create: bool,
) -> Result<PinnedPath, WindowsFsError> {
    let absolute =
        std::path::absolute(path).map_err(|source| io_error("建立绝对文件路径", path, source))?;
    let parent = absolute.parent().ok_or_else(|| {
        io_error(
            "打开读写文件",
            &absolute,
            io::Error::new(io::ErrorKind::InvalidInput, "文件路径没有父目录"),
        )
    })?;
    let mut pinned_parent = pin_directory_without_reparse(parent)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&absolute)
        .map_err(|source| io_error("打开读写文件", &absolute, source))?;
    reject_reparse(&file, &absolute)?;
    let metadata = file
        .metadata()
        .map_err(|source| io_error("读取读写文件元数据", &absolute, source))?;
    if !metadata.is_file() {
        return Err(io_error(
            "确认读写普通文件",
            &absolute,
            io::Error::new(io::ErrorKind::InvalidInput, "目标不是普通文件"),
        ));
    }
    pinned_parent.handles.push(file);
    pinned_parent.resolved_path = absolute
        .canonicalize()
        .map_err(|source| io_error("规范化读写文件", &absolute, source))?;
    Ok(pinned_parent)
}

fn reject_reparse(file: &File, path: &Path) -> Result<(), WindowsFsError> {
    let metadata = file
        .metadata()
        .map_err(|source| io_error("读取文件属性", path, source))?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(WindowsFsError::ReparsePoint {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

/// 确认目录位于本机、非大小写敏感的 NTFS 固定卷。
///
/// 调用方必须先建立目标目录；校验通过后返回规范绝对路径。
pub(crate) fn validate_local_case_insensitive_ntfs_directory(
    path: &Path,
) -> Result<PathBuf, WindowsFsError> {
    let pinned = pin_directory_without_reparse(path)?;
    let resolved = pinned.resolved_path().to_path_buf();
    let directory = pinned.file();

    let mut volume_root = vec![0_u16; 32_768];
    let resolved_wide = wide_nul(resolved.as_os_str());
    // SAFETY: 输入以 NUL 终止，输出指向长度明确且可写的 UTF-16 缓冲区。
    let volume_ok = unsafe {
        GetVolumePathNameW(
            resolved_wide.as_ptr(),
            volume_root.as_mut_ptr(),
            volume_root.len() as u32,
        )
    };
    if volume_ok == 0 {
        return Err(io_error(
            "读取卷根目录",
            &resolved,
            io::Error::last_os_error(),
        ));
    }
    // SAFETY: `GetVolumePathNameW` 成功后保证缓冲区含 NUL 终止的卷根路径。
    let drive_type = unsafe { GetDriveTypeW(volume_root.as_ptr()) };
    if drive_type != DRIVE_FIXED {
        return Err(WindowsFsError::NonLocalVolume { path: resolved });
    }

    let mut file_system = vec![0_u16; 32];
    // SAFETY: 目录句柄有效，未使用的输出允许为空，文件系统名缓冲区大小正确。
    let information_ok = unsafe {
        GetVolumeInformationByHandleW(
            handle(directory),
            ptr::null_mut(),
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            file_system.as_mut_ptr(),
            file_system.len() as u32,
        )
    };
    if information_ok == 0 {
        return Err(io_error(
            "读取卷信息",
            &resolved,
            io::Error::last_os_error(),
        ));
    }
    let file_system_length = file_system
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(file_system.len());
    let actual = String::from_utf16_lossy(&file_system[..file_system_length]);
    if !actual.eq_ignore_ascii_case("NTFS") {
        return Err(WindowsFsError::NonNtfsVolume {
            path: resolved,
            actual,
        });
    }

    let mut case_info = FILE_CASE_SENSITIVE_INFO::default();
    // SAFETY: 目录句柄有效，输出缓冲区具有正确类型、大小与对齐。
    let case_ok = unsafe {
        GetFileInformationByHandleEx(
            handle(directory),
            FileCaseSensitiveInfo,
            (&raw mut case_info).cast(),
            size_of::<FILE_CASE_SENSITIVE_INFO>() as u32,
        )
    };
    if case_ok == 0 {
        return Err(io_error(
            "读取目录大小写语义",
            &resolved,
            io::Error::last_os_error(),
        ));
    }
    if case_info.Flags & FILE_CS_FLAG_CASE_SENSITIVE_DIR != 0 {
        return Err(WindowsFsError::CaseSensitiveDirectory { path: resolved });
    }

    Ok(resolved)
}

/// 持有一个跨进程 Win32 独占文件锁。
pub(crate) struct ExclusiveFileLock {
    _parent: PinnedPath,
    file: File,
    overlapped: OVERLAPPED,
}

// `OVERLAPPED` 仅作为同步 LockFileEx 的偏移描述，锁对象不在线程间并发修改它。
// SAFETY: `ExclusiveFileLock` 独占其 `OVERLAPPED` 与文件句柄，移动不会改变内核锁身份。
unsafe impl Send for ExclusiveFileLock {}

impl ExclusiveFileLock {
    pub(crate) fn acquire(
        path: &Path,
        continue_waiting: &AtomicBool,
    ) -> Result<Self, WindowsFsError> {
        let parent_path = path.parent().ok_or_else(|| {
            io_error(
                "打开锁文件",
                path,
                io::Error::new(io::ErrorKind::InvalidInput, "锁文件路径没有父目录"),
            )
        })?;
        let parent = pin_directory_without_reparse(parent_path)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .map_err(|source| io_error("打开锁文件", path, source))?;
        reject_reparse(&file, path)?;
        let mut overlapped = OVERLAPPED::default();
        loop {
            if !continue_waiting.load(Ordering::Acquire) {
                return Err(WindowsFsError::LockCancelled {
                    path: path.to_path_buf(),
                });
            }
            // SAFETY: 文件句柄与 `overlapped` 在锁的整个生命周期内有效；同步调用不保留指针。
            let locked = unsafe {
                LockFileEx(
                    handle(&file),
                    LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                    0,
                    1,
                    0,
                    &raw mut overlapped,
                )
            };
            if locked != 0 {
                return Ok(Self {
                    _parent: parent,
                    file,
                    overlapped,
                });
            }
            let source = io::Error::last_os_error();
            if !matches!(
                source.raw_os_error().map(|code| code as u32),
                Some(ERROR_LOCK_VIOLATION) | Some(ERROR_SHARING_VIOLATION)
            ) {
                return Err(io_error("取得文件锁", path, source));
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    pub(crate) fn identity(&self, path: &Path) -> Result<FileIdentity, WindowsFsError> {
        FileIdentity::of(&self.file, path)
    }
}

impl Drop for ExclusiveFileLock {
    fn drop(&mut self) {
        // SAFETY: 该句柄与 OVERLAPPED 正是成功加锁时持有的对象；失败时 OS 仍会在句柄关闭时释放锁。
        unsafe {
            UnlockFileEx(handle(&self.file), 0, 1, 0, &raw mut self.overlapped);
        }
    }
}

/// 仅当来源仍是调用方确认的 file ID 时执行无覆盖重命名。
pub(crate) fn rename_without_replace_if_identity(
    source: &Path,
    target: &Path,
    expected: FileIdentity,
) -> Result<(), WindowsFsError> {
    rename_without_replace_inner(source, target, Some(expected))
}

fn rename_without_replace_inner(
    source: &Path,
    target: &Path,
    expected: Option<FileIdentity>,
) -> Result<(), WindowsFsError> {
    let source_parent_path = source.parent().ok_or_else(|| {
        io_error(
            "无覆盖重命名",
            source,
            io::Error::new(io::ErrorKind::InvalidInput, "来源路径没有父目录"),
        )
    })?;
    let target_parent_path = target.parent().ok_or_else(|| {
        io_error(
            "无覆盖重命名",
            target,
            io::Error::new(io::ErrorKind::InvalidInput, "目标路径没有父目录"),
        )
    })?;
    let _source_parent = pin_directory_without_reparse(source_parent_path)?;
    let _target_parent = pin_directory_without_reparse(target_parent_path)?;
    let source_file = OpenOptions::new()
        .access_mode(DELETE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(source)
        .map_err(|source_error| io_error("打开待重命名对象", source, source_error))?;
    reject_reparse(&source_file, source)?;
    if let Some(expected) = expected {
        let actual = FileIdentity::of(&source_file, source)?;
        if actual != expected {
            return Err(WindowsFsError::FileIdentityChanged {
                path: source.to_path_buf(),
            });
        }
    }

    let target_wide: Vec<u16> = target.as_os_str().encode_wide().collect();
    let header_bytes = offset_of!(FILE_RENAME_INFO, FileName);
    // Windows 文档允许 FileNameLength 不包含 NUL，但内核中仍有文件系统驱动会读取
    // 结构体尾部的终止单元。分配大小包含一个零值 UTF-16 单元，但协议长度仍只记录
    // 实际目标名称字节数，与 Rust 标准库的 Win32 实现一致。
    let buffer_bytes = header_bytes + (target_wide.len() + 1) * size_of::<u16>();
    let word_count = buffer_bytes.div_ceil(size_of::<usize>());
    let mut storage = vec![0_usize; word_count];
    let info = storage.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    // SAFETY: `storage` 按 `FILE_RENAME_INFO` 对齐，容量覆盖结构头和全部 UTF-16 单元。
    unsafe {
        (*info).Anonymous.ReplaceIfExists = false;
        // SetFileInformationByHandle 在目标名使用 Win32 绝对路径时要求根句柄为空。
        // `_target_parent` 仍被持有，用于防止目标父链在该同步调用期间被替换。
        (*info).RootDirectory = ptr::null_mut();
        (*info).FileNameLength = (target_wide.len() * size_of::<u16>()) as u32;
        ptr::copy_nonoverlapping(
            target_wide.as_ptr(),
            (&raw mut (*info).FileName).cast::<u16>(),
            target_wide.len(),
        );
    }
    // SAFETY: 源句柄在同步调用期间保持有效，`storage` 的生命周期覆盖整个调用。
    let renamed = unsafe {
        SetFileInformationByHandle(
            handle(&source_file),
            FileRenameInfo,
            storage.as_ptr().cast(),
            buffer_bytes as u32,
        )
    };
    if renamed == 0 {
        let source_error = io::Error::last_os_error();
        if matches!(source_error.kind(), io::ErrorKind::AlreadyExists) {
            return Err(WindowsFsError::RenameTargetExists {
                path: target.to_path_buf(),
            });
        }
        return Err(io_error("无覆盖重命名", target, source_error));
    }
    Ok(())
}

/// 仅当路径仍指向调用方确认的普通文件时，删除该精确内核对象。
pub(crate) fn delete_regular_file_if_identity(
    path: &Path,
    expected: FileIdentity,
) -> Result<(), WindowsFsError> {
    delete_if_identity(path, expected, false)
}

/// 仅当路径仍指向调用方确认的空目录时，删除该精确内核对象。
pub(crate) fn delete_empty_directory_if_identity(
    path: &Path,
    expected: FileIdentity,
) -> Result<(), WindowsFsError> {
    delete_if_identity(path, expected, true)
}

fn delete_if_identity(
    path: &Path,
    expected: FileIdentity,
    directory: bool,
) -> Result<(), WindowsFsError> {
    let parent_path = path.parent().ok_or_else(|| {
        io_error(
            "按文件身份删除对象",
            path,
            io::Error::new(io::ErrorKind::InvalidInput, "待删除路径没有父目录"),
        )
    })?;
    let _parent = pin_directory_without_reparse(parent_path)?;
    let flags = FILE_FLAG_OPEN_REPARSE_POINT
        | if directory {
            FILE_FLAG_BACKUP_SEMANTICS
        } else {
            0
        };
    let file = OpenOptions::new()
        .access_mode(DELETE | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(flags)
        .open(path)
        .map_err(|source| io_error("打开待删除对象", path, source))?;
    reject_reparse(&file, path)?;
    let metadata = file
        .metadata()
        .map_err(|source| io_error("读取待删除对象元数据", path, source))?;
    if metadata.is_dir() != directory || (!directory && !metadata.is_file()) {
        return Err(io_error(
            "确认待删除对象类型",
            path,
            io::Error::new(io::ErrorKind::InvalidInput, "对象类型与删除意图不一致"),
        ));
    }
    if FileIdentity::of(&file, path)? != expected {
        return Err(WindowsFsError::FileIdentityChanged {
            path: path.to_path_buf(),
        });
    }
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: 文件句柄、结构体地址、大小与对齐在同步调用期间全部有效。
    let deleted = unsafe {
        SetFileInformationByHandle(
            handle(&file),
            FileDispositionInfo,
            (&raw const disposition).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    };
    if deleted == 0 {
        return Err(io_error(
            "按文件身份删除对象",
            path,
            io::Error::last_os_error(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn io_diagnostic_keeps_path_and_stable_os_facts_without_wrapped_text() {
        let error = WindowsFsError::Io {
            operation: "open_file",
            path: PathBuf::from("C:\\game\r\nforged\\Data.json"),
            source: io::Error::new(io::ErrorKind::PermissionDenied, "API_KEY_SECRET"),
        };
        let diagnostic = error.safe_diagnostic(
            DiagnosticCode::FileSystemOperation,
            DiagnosticStage::Extract,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckPathAndPermissions,
        );
        let serialized = serde_json::to_string(&diagnostic).expect("诊断应可序列化");

        assert!(!serialized.contains("API_KEY_SECRET"));
        assert!(!serialized.contains("\\r"));
        assert!(!serialized.contains("\\n"));
        assert!(serialized.contains("C:\\\\game forged\\\\Data.json"));
        assert!(serialized.contains("permission_denied"));
        assert!(serialized.contains("open_file"));
    }

    fn symlink_unavailable(error: &io::Error) -> bool {
        matches!(
            error.kind(),
            io::ErrorKind::PermissionDenied | io::ErrorKind::Unsupported
        ) || error.raw_os_error() == Some(1314)
    }

    #[test]
    fn file_identity_is_stable_for_same_open_file() {
        let temporary = tempfile::tempdir().expect("应该可创建临时目录");
        let file_path = temporary.path().join("identity-test");
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&file_path)
            .expect("测试文件应该可创建");
        let first = FileIdentity::of(&file, &file_path).expect("应该可读取文件身份");
        let second = FileIdentity::of(&file, &file_path).expect("应该可再次读取文件身份");
        assert_eq!(first, second);
        drop(file);
        let reopened = open_regular_file(&file_path, true).expect("应该可重新打开测试文件");
        assert_eq!(
            first,
            FileIdentity::of(&reopened, &file_path).expect("重新打开后应该可读取相同身份")
        );
    }

    #[test]
    fn directory_identity_is_stable_after_adding_children_and_reopening() {
        let temporary = tempfile::tempdir().expect("应该可创建临时目录");
        let directory = temporary.path().join("candidate");
        std::fs::create_dir(&directory).expect("应该可创建候选目录");
        let first_handle = open_directory(&directory, true).expect("应该可打开候选目录");
        let first = FileIdentity::of(&first_handle, &directory).expect("应该可读取候选身份");
        std::fs::write(directory.join("project.db"), b"database")
            .expect("应该可在候选中建立数据库");
        let reopened = open_directory(&directory, true).expect("应该可重新打开候选目录");
        assert_eq!(
            first,
            FileIdentity::of(&reopened, &directory).expect("应该可读取重开候选身份")
        );
    }

    #[test]
    fn parent_directory_reparse_component_is_rejected() {
        let temporary = tempfile::tempdir().expect("应该可创建临时目录");
        let real = temporary.path().join("real");
        let child = real.join("child");
        std::fs::create_dir_all(&child).expect("应该可创建真实目录");
        let link = temporary.path().join("parent-link");
        if let Err(error) = std::os::windows::fs::symlink_dir(&real, &link) {
            if symlink_unavailable(&error) {
                return;
            }
            panic!("应该可创建目录符号链接：{error}");
        }

        let error = match pin_directory_without_reparse(&link.join("child")) {
            Ok(_) => panic!("父路径中的 reparse point 必须被拒绝"),
            Err(error) => error,
        };
        assert!(matches!(error, WindowsFsError::ReparsePoint { path } if path == link));
    }

    #[test]
    fn directory_entry_replaced_by_reparse_after_enumeration_is_rejected() {
        let temporary = tempfile::tempdir().expect("应该可创建临时目录");
        let directory = temporary.path().join("listed");
        let target = temporary.path().join("target.txt");
        std::fs::create_dir(&directory).expect("应该可创建待列举目录");
        std::fs::write(&target, b"target").expect("应该可创建链接目标");
        let child = directory.join("child.txt");
        std::fs::write(&child, b"original").expect("应该可创建原目录项");

        let enumerated = std::fs::read_dir(&directory)
            .expect("应该可列举目录")
            .next()
            .expect("应该存在目录项")
            .expect("目录项应该可读取")
            .path();
        std::fs::remove_file(&enumerated).expect("应该可替换已列举目录项");
        if let Err(error) = std::os::windows::fs::symlink_file(&target, &enumerated) {
            if symlink_unavailable(&error) {
                return;
            }
            panic!("应该可把目录项替换为文件符号链接：{error}");
        }

        let error = match pin_path_without_reparse(&enumerated) {
            Ok(_) => panic!("列举后替换成的 reparse point 必须被拒绝"),
            Err(error) => error,
        };
        assert!(matches!(error, WindowsFsError::ReparsePoint { path } if path == enumerated));
    }

    #[test]
    fn missing_directory_suffix_is_created_one_checked_component_at_a_time() {
        let temporary = tempfile::tempdir().expect("应该可创建临时目录");
        let requested = temporary.path().join("日志/运行/当天");
        let pinned = create_directories_without_reparse(&requested)
            .expect("无 reparse 的缺失目录后缀应该可建立");
        assert!(pinned.resolved_path().is_absolute());
        assert!(requested.is_dir());
    }

    #[test]
    fn creating_a_missing_suffix_never_traverses_a_parent_reparse() {
        let temporary = tempfile::tempdir().expect("应该可创建临时目录");
        let real = temporary.path().join("real");
        std::fs::create_dir(&real).expect("应该可创建真实父目录");
        let link = temporary.path().join("parent-link");
        if let Err(error) = std::os::windows::fs::symlink_dir(&real, &link) {
            if symlink_unavailable(&error) {
                return;
            }
            panic!("应该可创建目录符号链接：{error}");
        }

        let requested = link.join("must-not-exist");
        let error = match create_directories_without_reparse(&requested) {
            Ok(_) => panic!("安全建目录必须拒绝父路径 reparse"),
            Err(error) => error,
        };
        assert!(matches!(error, WindowsFsError::ReparsePoint { path } if path == link));
        assert!(!real.join("must-not-exist").exists());
    }

    #[test]
    fn handle_disposition_deletes_only_the_expected_file_and_empty_directory() {
        let temporary = tempfile::tempdir().expect("应该可创建临时目录");
        let file_path = temporary.path().join("leaf.txt");
        fs::write(&file_path, b"leaf").expect("应该可创建待删除文件");
        let file = open_regular_file(&file_path, true).expect("应该可打开待删除文件");
        let file_identity = FileIdentity::of(&file, &file_path).expect("应该可读取文件身份");
        drop(file);

        delete_regular_file_if_identity(&file_path, file_identity)
            .expect("身份匹配的普通文件应该被删除");
        assert!(!file_path.exists());

        let directory = temporary.path().join("empty");
        fs::create_dir(&directory).expect("应该可创建待删除空目录");
        let handle = open_directory(&directory, true).expect("应该可打开待删除目录");
        let directory_identity = FileIdentity::of(&handle, &directory).expect("应该可读取目录身份");
        drop(handle);

        delete_empty_directory_if_identity(&directory, directory_identity)
            .expect("身份匹配的空目录应该被删除");
        assert!(!directory.exists());
    }

    #[test]
    fn handle_disposition_refuses_a_replacement_with_a_foreign_file_identity() {
        let temporary = tempfile::tempdir().expect("应该可创建临时目录");
        let path = temporary.path().join("replaceable.txt");
        let displaced = temporary.path().join("displaced.txt");
        fs::write(&path, b"original").expect("应该可创建原文件");
        let original = open_regular_file(&path, true).expect("应该可打开原文件");
        let original_identity = FileIdentity::of(&original, &path).expect("应该可读取原文件身份");
        drop(original);

        fs::rename(&path, &displaced).expect("应该可移开原文件");
        fs::write(&path, b"foreign").expect("应该可创建外来文件");

        assert!(matches!(
            delete_regular_file_if_identity(&path, original_identity),
            Err(WindowsFsError::FileIdentityChanged { path: changed }) if changed == path
        ));
        assert_eq!(fs::read(&path).expect("外来文件应该仍可读取"), b"foreign");
    }
}
