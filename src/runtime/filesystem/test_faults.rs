//! 真实文件系统边界的测试故障注入。

use super::error::{SystemFileSystemError, io_error};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TestPublishFaultPoint {
    BeforeOriginalMove,
    AfterOriginalJournal,
    AfterOriginalMove,
    AfterCandidateIntent,
    BeforeCandidateMove,
    AfterCandidateMove,
    AfterCandidateVisible,
    BeforeRestoreMove,
    BeforeBackupCleanup,
    BeforeJournalCleanup,
    BeforeRecoveryCleanup,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TestPublishFaultAction {
    Error,
    Abort,
}

type TestPublishFaultQueue = VecDeque<(TestPublishFaultPoint, TestPublishFaultAction)>;

static TEST_PUBLISH_FAULTS: OnceLock<Mutex<HashMap<PathBuf, TestPublishFaultQueue>>> =
    OnceLock::new();

static TEST_CANCEL_CANDIDATE_COPY_AFTER_CHUNK: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum TestObservationFaultPoint {
    AfterPartialWrite,
    BeforeFlush,
    BeforeSync,
    BeforeRename,
    BeforeCleanup,
}

static TEST_OBSERVATION_FAULTS: OnceLock<
    Mutex<HashMap<PathBuf, HashSet<TestObservationFaultPoint>>>,
> = OnceLock::new();

pub(crate) fn register_test_observation_faults(
    path: PathBuf,
    faults: impl IntoIterator<Item = TestObservationFaultPoint>,
) {
    TEST_OBSERVATION_FAULTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("可观测性故障测试锁不应中毒")
        .insert(path, faults.into_iter().collect());
}

pub(super) fn hit_test_observation_fault(path: &Path, point: TestObservationFaultPoint) -> bool {
    let Some(faults) = TEST_OBSERVATION_FAULTS.get() else {
        return false;
    };
    let mut faults = faults.lock().expect("可观测性故障测试锁不应中毒");
    let Some(points) = faults.get_mut(path) else {
        return false;
    };
    let hit = points.remove(&point);
    if points.is_empty() {
        faults.remove(path);
    }
    hit
}

pub(super) fn register_test_candidate_copy_cancellation(source: PathBuf) {
    TEST_CANCEL_CANDIDATE_COPY_AFTER_CHUNK
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(source);
}

pub(super) fn cancel_test_candidate_copy_after_chunk(source: &Path, cancellation: &AtomicBool) {
    let Some(sources) = TEST_CANCEL_CANDIDATE_COPY_AFTER_CHUNK.get() else {
        return;
    };
    if sources
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(source)
    {
        cancellation.store(false, Ordering::Release);
    }
}

pub(crate) fn register_test_publish_faults(
    target_root: PathBuf,
    faults: impl IntoIterator<Item = (TestPublishFaultPoint, TestPublishFaultAction)>,
) {
    TEST_PUBLISH_FAULTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("目录发布故障测试锁不应中毒")
        .insert(target_root, faults.into_iter().collect());
}

pub(super) fn hit_test_publish_fault(target_root: &Path, point: TestPublishFaultPoint) -> bool {
    let Some(faults) = TEST_PUBLISH_FAULTS.get() else {
        return false;
    };
    let mut faults = faults.lock().expect("目录发布故障测试锁不应中毒");
    let Some(queue) = faults.get_mut(target_root) else {
        return false;
    };
    let Some((expected, action)) = queue.front().copied() else {
        faults.remove(target_root);
        return false;
    };
    if expected != point {
        return false;
    }
    queue.pop_front();
    if queue.is_empty() {
        faults.remove(target_root);
    }
    drop(faults);
    match action {
        TestPublishFaultAction::Error => true,
        TestPublishFaultAction::Abort => std::process::abort(),
    }
}

pub(super) fn injected_publish_error(
    operation: &'static str,
    path: &Path,
) -> SystemFileSystemError {
    io_error(operation, path, io::Error::other("测试注入的目录发布故障"))
}
