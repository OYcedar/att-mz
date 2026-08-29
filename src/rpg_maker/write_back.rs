//! RPG Maker 数据库译文写回冻结项目副本的顶层编排。

use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::project::OpenedProject;
use crate::diagnostic::ReportedFailure;
use crate::execution::{CooperativeCancellation, OperationCompletion};
use crate::progress::{NoopProgressObserver, ProgressObserver, ProgressSnapshot};
use crate::project_name::ProjectName;
use crate::rpg_maker::RpgMakerLayout;
use crate::storage::file_system::ScopedDirectoryScope;

pub(crate) mod asset_reader;
pub(crate) mod planner;
pub(crate) mod publisher;
pub(crate) mod rewriter;

fn rpg_maker_output_scope(layout: RpgMakerLayout) -> ScopedDirectoryScope {
    let roots = match layout.content_directory() {
        Some(directory) => vec![OsString::from(directory)],
        None => vec![OsString::from("data"), OsString::from("js")],
    };
    ScopedDirectoryScope::new(roots).expect("固定 RPG Maker 写回顶层目录必须能建立候选编辑范围")
}

/// WriteBack 当前可被真实观测的业务阶段；只有存在权威分母的阶段才发布数量进度。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WriteBackProgressPhase {
    ReadingAssets,
    PlanningTranslations,
    RewritingDocuments,
    PreparingCandidate,
    ValidatingCandidate,
    Publishing,
}

/// 一轮 RPG Maker 写回的正常业务汇总。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RpgMakerWriteBackSummary {
    /// 已应用数据库译文的语义单元数。
    pub translated_units: usize,
    /// 因数据库中没有译文而保留冻结原文的语义单元数。
    pub original_units: usize,
}

/// 写回命令正常完成后交还给 CLI 的结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WriteBackOutput {
    pub name: ProjectName,
    /// 本轮已经发布、供后续封包消费的固定最新输出根目录。
    pub output_root: PathBuf,
    pub summary: RpgMakerWriteBackSummary,
}

/// 完整候选已经成功发布后的固定输出身份。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PublishedWriteBack {
    output_root: PathBuf,
}

impl PublishedWriteBack {
    fn new(output_root: PathBuf) -> Self {
        Self { output_root }
    }

    pub(crate) fn output_root(&self) -> &Path {
        &self.output_root
    }
}

/// 发布根已经确认的写回失败终态；调用方据此记录事实，不能从错误文本猜测。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum WriteBackPublishFailureState {
    NotPublished {
        output_root: PathBuf,
        residual_paths: Vec<PathBuf>,
    },
    PublishedWithResiduals {
        output_root: PathBuf,
        residual_paths: Vec<PathBuf>,
    },
    RecoveryRequired {
        output_root: PathBuf,
        recovery_artifacts: Vec<PathBuf>,
    },
    OutcomeUnknown {
        output_root: PathBuf,
        recovery_artifacts: Vec<PathBuf>,
    },
}

/// 发布错误与其精确终态必须作为一个不可分割的结果返回。
#[derive(Debug)]
pub(crate) struct WriteBackPublishFailure<E> {
    state: WriteBackPublishFailureState,
    source: E,
}

/// WriteBack 业务服务交给外层可观测性适配器的不可失败事实。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum WriteBackLogEvent {
    PublicationStarted {
        output_root: PathBuf,
    },
    PublicationFinished {
        output_root: PathBuf,
        outcome: WriteBackLogPublicationOutcome,
    },
}

/// 发布边界已经确认的终态；日志适配器不得再从错误文本推断结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WriteBackLogPublicationOutcome {
    Published { summary: RpgMakerWriteBackSummary },
    NotPublished,
    PublishedWithResiduals,
    RecoveryRequired,
    OutcomeUnknown,
}

/// 同步、不可失败的 WriteBack 观察入口。
///
/// 实现只能接收事实；队列拥塞、文件损坏或关闭失败均不得反馈给业务流程。
pub(crate) trait WriteBackLog: Send + Sync {
    fn emit(&self, event: WriteBackLogEvent);
}

impl<E> WriteBackPublishFailure<E> {
    pub(crate) fn new(state: WriteBackPublishFailureState, source: E) -> Self {
        Self { state, source }
    }

    fn into_parts(self) -> (WriteBackPublishFailureState, E) {
        (self.state, self.source)
    }
}

/// 从项目数据库译文生成 RPG Maker 文件候选。
///
/// 实现校验当前项目快照，再按正文开关与项目排版规则建立候选。
pub(crate) trait RpgMakerWriteBack: Send + Sync {
    type Documents: Send + 'static;
    type Error: Error + Send + Sync + 'static;

    fn prepare(
        &self,
        project: &OpenedProject,
    ) -> impl Future<
        Output = Result<
            OperationCompletion<planner::RpgMakerWriteBackPreparation<Self::Documents>>,
            Self::Error,
        >,
    > + Send;
}

/// 已准备但尚未发布的完整写回目录候选。
pub(crate) trait PreparedWriteBackCandidate: Send + 'static {
    fn candidate_root(&self) -> &Path;
}

/// 准备、借用式校验、发布或丢弃唯一写回候选的能力。
pub(crate) trait RpgMakerWriteBackPublisher<D>: Send + Sync
where
    D: Send + 'static,
{
    type Candidate: PreparedWriteBackCandidate;
    type Error: Error + Send + Sync + 'static;

    fn prepare(
        &self,
        project: &OpenedProject,
        documents: D,
    ) -> impl Future<Output = Result<Self::Candidate, Self::Error>> + Send;

    /// 在任何可见发布前，以当前物理事实复核完整候选。
    ///
    /// 该调用借用候选，不取得终结权；失败后调用方仍必须恰好一次丢弃候选。
    fn validate<'a>(
        &'a self,
        candidate: &Self::Candidate,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + use<'a, Self, D>;

    fn publish(
        &self,
        candidate: Self::Candidate,
    ) -> impl Future<Output = Result<PublishedWriteBack, WriteBackPublishFailure<Self::Error>>> + Send;

    fn discard(
        &self,
        candidate: Self::Candidate,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// 命令边界只消费 Publisher 主动提供的安全投影，不解析任意错误链。
pub(crate) trait WriteBackPublishingDiagnostic: Sized {
    fn into_write_back_failure_report(self) -> ReportedFailure;
}

/// 按固定业务顺序编排一次 RPG Maker 文本写回。
///
/// Application 交入已经打开且持有排他租约的项目；本服务准备并验证完整候选，最后只发布一次。发布根接管 token 后，
/// 上层不再清理。
pub(crate) struct WriteBackService<S, P, J> {
    rpg_maker_write_back: S,
    publisher: P,
    event_log: J,
    cancellation: CooperativeCancellation,
    progress: Arc<dyn ProgressObserver<WriteBackProgressPhase>>,
}

impl<S, P, J> WriteBackService<S, P, J> {
    pub(crate) fn new(
        rpg_maker_write_back: S,
        publisher: P,
        event_log: J,
        cancellation: CooperativeCancellation,
    ) -> Self {
        Self {
            rpg_maker_write_back,
            publisher,
            event_log,
            cancellation,
            progress: Arc::new(NoopProgressObserver),
        }
    }

    /// 为本次 WriteBack 绑定同步、不可失败的业务进度观察者。
    pub(crate) fn with_progress<Q>(mut self, progress: Q) -> Self
    where
        Q: ProgressObserver<WriteBackProgressPhase> + 'static,
    {
        self.progress = Arc::new(progress);
        self
    }

    fn start_phase(&self, phase: WriteBackProgressPhase) {
        self.progress
            .observe(ProgressSnapshot::determinate(phase, 0, 1));
    }

    fn complete_phase(&self, phase: WriteBackProgressPhase) {
        self.progress
            .observe(ProgressSnapshot::determinate(phase, 1, 1));
    }
}

impl<S, P, J> WriteBackService<S, P, J>
where
    S: RpgMakerWriteBack,
    P: RpgMakerWriteBackPublisher<S::Documents>,
    J: WriteBackLog,
{
    pub(crate) async fn execute(
        &self,
        project: &OpenedProject,
    ) -> Result<OperationCompletion<WriteBackOutput>, WriteBackServiceError<S::Error, P::Error>>
    {
        if self.cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }

        let preparation = self
            .rpg_maker_write_back
            .prepare(project)
            .await
            .map_err(WriteBackServiceError::Prepare)?;
        let OperationCompletion::Completed(preparation) = preparation else {
            return Ok(OperationCompletion::Cancelled);
        };
        let (documents, summary) = preparation.into_parts();

        if self.cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }
        self.start_phase(WriteBackProgressPhase::PreparingCandidate);
        let candidate = self
            .publisher
            .prepare(project, documents)
            .await
            .map_err(WriteBackServiceError::PrepareCandidate)?;
        self.complete_phase(WriteBackProgressPhase::PreparingCandidate);

        if self.cancellation.is_requested() {
            let candidate_root = candidate.candidate_root().to_path_buf();
            return match self.publisher.discard(candidate).await {
                Ok(()) => Ok(OperationCompletion::Cancelled),
                Err(discard) => Err(WriteBackServiceError::CancellationDiscard {
                    candidate_root,
                    discard,
                }),
            };
        }

        self.start_phase(WriteBackProgressPhase::ValidatingCandidate);
        if let Err(source) = self.publisher.validate(&candidate).await {
            let candidate_root = candidate.candidate_root().to_path_buf();
            return match self.publisher.discard(candidate).await {
                Ok(()) => Err(WriteBackServiceError::ValidateCandidate {
                    candidate_root,
                    source,
                }),
                Err(discard) => Err(WriteBackServiceError::ValidateCandidateAndDiscard {
                    candidate_root,
                    source,
                    discard,
                }),
            };
        }
        self.complete_phase(WriteBackProgressPhase::ValidatingCandidate);

        if self.cancellation.is_requested() {
            let candidate_root = candidate.candidate_root().to_path_buf();
            return match self.publisher.discard(candidate).await {
                Ok(()) => Ok(OperationCompletion::Cancelled),
                Err(discard) => Err(WriteBackServiceError::CancellationDiscard {
                    candidate_root,
                    discard,
                }),
            };
        }

        // 借用式业务校验已经通过；`publish` 现在按值接管 token，并在实际目标交换前
        // 再次复核完整候选以覆盖检查与使用之间的变化。从此边界开始，无论根返回何种
        // 终态，上层都不得再次尝试 discard。观察入口不可失败，因此不会成为发布门槛。
        self.start_phase(WriteBackProgressPhase::Publishing);
        let intended_output_root = project.write_back_root().to_path_buf();
        self.event_log.emit(WriteBackLogEvent::PublicationStarted {
            output_root: intended_output_root,
        });
        let published = match self.publisher.publish(candidate).await {
            Ok(published) => published,
            Err(failure) => {
                let (state, source) = failure.into_parts();
                let (output_root, outcome) = match &state {
                    WriteBackPublishFailureState::NotPublished { output_root, .. } => (
                        output_root.clone(),
                        WriteBackLogPublicationOutcome::NotPublished,
                    ),
                    WriteBackPublishFailureState::PublishedWithResiduals {
                        output_root, ..
                    } => (
                        output_root.clone(),
                        WriteBackLogPublicationOutcome::PublishedWithResiduals,
                    ),
                    WriteBackPublishFailureState::RecoveryRequired { output_root, .. } => (
                        output_root.clone(),
                        WriteBackLogPublicationOutcome::RecoveryRequired,
                    ),
                    WriteBackPublishFailureState::OutcomeUnknown { output_root, .. } => (
                        output_root.clone(),
                        WriteBackLogPublicationOutcome::OutcomeUnknown,
                    ),
                };
                self.event_log.emit(WriteBackLogEvent::PublicationFinished {
                    output_root,
                    outcome,
                });
                return Err(WriteBackServiceError::Publish { state, source });
            }
        };
        let output_root = published.output_root().to_path_buf();
        self.event_log.emit(WriteBackLogEvent::PublicationFinished {
            output_root: output_root.clone(),
            outcome: WriteBackLogPublicationOutcome::Published { summary },
        });
        self.complete_phase(WriteBackProgressPhase::Publishing);

        Ok(OperationCompletion::Completed(WriteBackOutput {
            name: project.name().clone(),
            output_root,
            summary,
        }))
    }
}

/// WriteBack 顶层用例在准备与候选终结边界遇到的阶段失败。
#[derive(Debug)]
pub(crate) enum WriteBackServiceError<SE, PE> {
    CancellationDiscard {
        candidate_root: PathBuf,
        discard: PE,
    },
    Prepare(SE),
    PrepareCandidate(PE),
    ValidateCandidate {
        candidate_root: PathBuf,
        source: PE,
    },
    ValidateCandidateAndDiscard {
        candidate_root: PathBuf,
        source: PE,
        discard: PE,
    },
    Publish {
        state: WriteBackPublishFailureState,
        source: PE,
    },
}

impl<SE, PE> fmt::Display for WriteBackServiceError<SE, PE>
where
    SE: Error,
    PE: Error,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CancellationDiscard {
                candidate_root,
                discard,
            } => write!(
                formatter,
                "取消后无法丢弃写回候选 {}：{discard}",
                candidate_root.display()
            ),
            Self::Prepare(source) => write!(formatter, "准备 RPG Maker 写回失败：{source}"),
            Self::PrepareCandidate(source) => write!(formatter, "准备完整写回候选失败：{source}"),
            Self::ValidateCandidate {
                candidate_root,
                source,
            } => write!(
                formatter,
                "写回候选未通过发布前完整校验（候选：{}）：{source}",
                candidate_root.display()
            ),
            Self::ValidateCandidateAndDiscard {
                candidate_root,
                source,
                discard,
            } => write!(
                formatter,
                "写回候选未通过发布前完整校验（候选：{}）：{source}；随后丢弃候选失败：{discard}",
                candidate_root.display()
            ),
            Self::Publish { state, source } => {
                write!(
                    formatter,
                    "发布完整写回候选失败（终态：{state:?}）：{source}"
                )
            }
        }
    }
}

impl<SE, PE> Error for WriteBackServiceError<SE, PE>
where
    SE: Error + 'static,
    PE: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CancellationDiscard { discard, .. } => Some(discard),
            Self::Prepare(source) => Some(source),
            Self::PrepareCandidate(source)
            | Self::ValidateCandidate { source, .. }
            | Self::ValidateCandidateAndDiscard { source, .. }
            | Self::Publish { source, .. } => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::progress::ProgressAmount;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakeError;

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("fake error")
        }
    }

    impl Error for FakeError {}

    type Events = Arc<Mutex<Vec<&'static str>>>;

    fn record(events: &Events, event: &'static str) {
        events.lock().expect("事件记录锁不应中毒").push(event);
    }

    #[derive(Clone, Default)]
    struct RecordingProgress {
        snapshots: Arc<Mutex<Vec<ProgressSnapshot<WriteBackProgressPhase>>>>,
    }

    impl RecordingProgress {
        fn snapshots(&self) -> Vec<ProgressSnapshot<WriteBackProgressPhase>> {
            self.snapshots.lock().expect("进度记录锁不应中毒").clone()
        }
    }

    impl ProgressObserver<WriteBackProgressPhase> for RecordingProgress {
        fn observe(&self, snapshot: ProgressSnapshot<WriteBackProgressPhase>) {
            self.snapshots
                .lock()
                .expect("进度记录锁不应中毒")
                .push(snapshot);
        }
    }

    struct FakeWriteBack {
        events: Events,
        summary: RpgMakerWriteBackSummary,
    }

    impl RpgMakerWriteBack for FakeWriteBack {
        type Documents = ();
        type Error = FakeError;

        async fn prepare(
            &self,
            _project: &OpenedProject,
        ) -> Result<
            OperationCompletion<planner::RpgMakerWriteBackPreparation<Self::Documents>>,
            Self::Error,
        > {
            record(&self.events, "prepare");
            Ok(OperationCompletion::Completed(
                planner::RpgMakerWriteBackPreparation::new((), self.summary),
            ))
        }
    }

    struct FakeCandidate {
        candidate_root: PathBuf,
        output_root: PathBuf,
    }

    impl PreparedWriteBackCandidate for FakeCandidate {
        fn candidate_root(&self) -> &Path {
            &self.candidate_root
        }
    }

    struct FakePublisher {
        events: Events,
    }

    impl RpgMakerWriteBackPublisher<()> for FakePublisher {
        type Candidate = FakeCandidate;
        type Error = FakeError;

        async fn prepare(
            &self,
            project: &OpenedProject,
            (): (),
        ) -> Result<Self::Candidate, Self::Error> {
            record(&self.events, "prepare_candidate");
            Ok(FakeCandidate {
                candidate_root: project.workspace_root().join("candidate"),
                output_root: project.write_back_root().to_path_buf(),
            })
        }

        fn validate<'a>(
            &'a self,
            candidate: &Self::Candidate,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send + use<'a> {
            assert!(candidate.candidate_root().ends_with("candidate"));
            record(&self.events, "validate");
            std::future::ready(Ok(()))
        }

        async fn publish(
            &self,
            candidate: Self::Candidate,
        ) -> Result<PublishedWriteBack, WriteBackPublishFailure<Self::Error>> {
            record(&self.events, "publish");
            Ok(PublishedWriteBack::new(candidate.output_root))
        }

        async fn discard(&self, _candidate: Self::Candidate) -> Result<(), Self::Error> {
            record(&self.events, "discard");
            Ok(())
        }
    }

    struct FakeLog {
        events: Events,
    }

    impl WriteBackLog for FakeLog {
        fn emit(&self, event: WriteBackLogEvent) {
            let name = match event {
                WriteBackLogEvent::PublicationStarted { .. } => "publication_started",
                WriteBackLogEvent::PublicationFinished { .. } => "publication_finished",
            };
            record(&self.events, name);
        }
    }

    fn project() -> OpenedProject {
        OpenedProject::new(
            "demo".parse().expect("项目名应合法"),
            PathBuf::from("C:/projects/demo"),
            PathBuf::from("C:/projects/demo/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
        )
    }

    fn service(
        events: Events,
        cancellation: CooperativeCancellation,
    ) -> WriteBackService<FakeWriteBack, FakePublisher, FakeLog> {
        let summary = RpgMakerWriteBackSummary {
            translated_units: 2,
            original_units: 1,
        };
        WriteBackService::new(
            FakeWriteBack {
                events: Arc::clone(&events),
                summary,
            },
            FakePublisher {
                events: Arc::clone(&events),
            },
            FakeLog {
                events: Arc::clone(&events),
            },
            cancellation,
        )
    }

    #[tokio::test]
    async fn prepares_validates_and_publishes_one_rpg_maker_candidate() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let project = project();
        let completion = service(Arc::clone(&events), CooperativeCancellation::default())
            .execute(&project)
            .await
            .expect("写回应成功");
        let OperationCompletion::Completed(output) = completion else {
            panic!("未取消的写回应完成")
        };

        assert_eq!(output.summary.translated_units, 2);
        assert_eq!(output.summary.original_units, 1);
        assert_eq!(
            events.lock().expect("事件记录锁不应中毒").as_slice(),
            [
                "prepare",
                "prepare_candidate",
                "validate",
                "publication_started",
                "publish",
                "publication_finished",
            ]
        );
    }

    #[tokio::test]
    async fn candidate_lifecycle_phases_have_explicit_start_and_completion() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let progress = RecordingProgress::default();
        let project = project();

        service(Arc::clone(&events), CooperativeCancellation::default())
            .with_progress(progress.clone())
            .execute(&project)
            .await
            .expect("写回应成功");

        let candidate_phases = progress
            .snapshots()
            .into_iter()
            .filter(|snapshot| {
                matches!(
                    snapshot.phase,
                    WriteBackProgressPhase::PreparingCandidate
                        | WriteBackProgressPhase::ValidatingCandidate
                        | WriteBackProgressPhase::Publishing
                )
            })
            .map(|snapshot| (snapshot.phase, snapshot.amount))
            .collect::<Vec<_>>();
        assert_eq!(
            candidate_phases,
            [
                (
                    WriteBackProgressPhase::PreparingCandidate,
                    ProgressAmount::Determinate {
                        completed: 0,
                        total: 1,
                    },
                ),
                (
                    WriteBackProgressPhase::PreparingCandidate,
                    ProgressAmount::Determinate {
                        completed: 1,
                        total: 1,
                    },
                ),
                (
                    WriteBackProgressPhase::ValidatingCandidate,
                    ProgressAmount::Determinate {
                        completed: 0,
                        total: 1,
                    },
                ),
                (
                    WriteBackProgressPhase::ValidatingCandidate,
                    ProgressAmount::Determinate {
                        completed: 1,
                        total: 1,
                    },
                ),
                (
                    WriteBackProgressPhase::Publishing,
                    ProgressAmount::Determinate {
                        completed: 0,
                        total: 1,
                    },
                ),
                (
                    WriteBackProgressPhase::Publishing,
                    ProgressAmount::Determinate {
                        completed: 1,
                        total: 1,
                    },
                ),
            ],
            "候选阶段必须各自形成 started -> completed，不能留下 active_phase"
        );
    }

    #[tokio::test]
    async fn cancellation_before_preparation_leaves_everything_untouched() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let cancellation = CooperativeCancellation::default();
        cancellation.request();
        let project = project();

        let completion = service(Arc::clone(&events), cancellation)
            .execute(&project)
            .await
            .expect("预先取消应返回正常结果");

        assert!(matches!(completion, OperationCompletion::Cancelled));
        assert!(events.lock().expect("事件记录锁不应中毒").is_empty());
    }
}
