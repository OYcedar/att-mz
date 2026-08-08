//! 项目工作区中的自然运行序号分配器。

use std::fs::{self, File, OpenOptions};
use std::io;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use crate::observability::RunId;

/// 扫描既有日志和任务记录，并以 `create_new` 原子保留下一份日志文件。
pub(crate) fn reserve_run_log(project_workspace: &Path) -> io::Result<(RunId, PathBuf, File)> {
    let logs_root = project_workspace.join("logs");
    let task_records_root = project_workspace.join("task-records");
    fs::create_dir_all(&logs_root)?;

    let mut next = maximum_sequence(&logs_root, RunArtifactKind::Log)?
        .max(maximum_sequence(
            &task_records_root,
            RunArtifactKind::TaskRecordDirectory,
        )?)
        .checked_add(1)
        .ok_or_else(|| io::Error::other("运行序号已经用尽"))?;

    loop {
        let sequence =
            NonZeroU64::new(next).ok_or_else(|| io::Error::other("运行序号必须大于零"))?;
        let run_id = RunId::from_sequence(sequence);
        let path = logs_root.join(format!("{run_id}.jsonl"));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((run_id, path, file)),
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                next = next
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("运行序号已经用尽"))?;
            }
            Err(source) => return Err(source),
        }
    }
}

#[derive(Clone, Copy)]
enum RunArtifactKind {
    Log,
    TaskRecordDirectory,
}

fn maximum_sequence(root: &Path, kind: RunArtifactKind) -> io::Result<u64> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(source)
            if source.kind() == io::ErrorKind::NotADirectory
                && matches!(kind, RunArtifactKind::TaskRecordDirectory) =>
        {
            // 任务记录由 Translate 在真正有模型任务时建立；普通文件只会让该
            // 记录器报告自己的故障，不应阻止本次运行日志建立。
            return Ok(0);
        }
        Err(source) => return Err(source),
    };
    let mut maximum = 0;
    for entry in entries {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        let run_text = match kind {
            RunArtifactKind::Log => file_name.strip_suffix(".jsonl"),
            RunArtifactKind::TaskRecordDirectory => Some(file_name),
        };
        let Some(run_id) = run_text.and_then(RunId::parse) else {
            continue;
        };
        maximum = maximum.max(run_id.sequence().get());
    }
    Ok(maximum)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn reserves_the_next_sequence_across_logs_and_task_records() {
        let temporary = tempfile::tempdir().expect("应建立测试目录");
        let project = temporary.path();
        fs::create_dir_all(project.join("logs")).expect("应建立日志目录");
        fs::create_dir_all(project.join("task-records/run-000004")).expect("应建立任务记录目录");
        fs::write(project.join("logs/run-000002.jsonl"), b"existing").expect("应建立既有日志");

        let (first, first_path, first_file) = reserve_run_log(project).expect("应保留下一序号");
        drop(first_file);
        let (second, second_path, second_file) = reserve_run_log(project).expect("应继续递增");
        drop(second_file);

        assert_eq!(first.to_string(), "run-000005");
        assert_eq!(second.to_string(), "run-000006");
        assert_eq!(first_path.file_name().unwrap(), "run-000005.jsonl");
        assert_eq!(second_path.file_name().unwrap(), "run-000006.jsonl");
    }

    #[test]
    fn task_record_file_does_not_block_log_reservation() {
        let temporary = tempfile::tempdir().expect("应建立测试目录");
        let project = temporary.path();
        fs::write(project.join("task-records"), b"not-a-directory")
            .expect("应建立任务记录故障现场");

        let (run_id, path, file) = reserve_run_log(project).expect("日志仍应建立");
        drop(file);

        assert_eq!(run_id.to_string(), "run-000001");
        assert_eq!(path.file_name().unwrap(), "run-000001.jsonl");
    }
}
