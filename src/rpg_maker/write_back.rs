//! RPG Maker 数据库译文写回冻结项目副本的顶层编排。

use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::ProjectName;
use super::SelectedLua;
use super::project::{ExistingProjectOpener, OpenedProject, RpgMakerWriteBackLayoutProfile};
use super::project_lease::{ProjectCommandLeaseError, ProjectCommandLeaseProvider};
use crate::diagnostic::{DiagnosticImpact, DiagnosticStage, FailureReport};
use crate::execution::{CooperativeCancellation, OperationCompletion};
use crate::progress::{NoopProgressObserver, ProgressObserver, ProgressSnapshot};
use crate::rpg_maker::RpgMakerLayout;
use crate::rpg_maker::lua::runtime::OwnedLuaProgram;
use crate::storage::file_system::ScopedDirectoryScope;

pub(crate) mod asset_reader;
pub(crate) mod lua;
pub(crate) mod publisher;
pub(crate) mod rewriter;
pub(crate) mod standard;

fn rpg_maker_output_scope(layout: RpgMakerLayout) -> ScopedDirectoryScope {
    let roots = match layout.content_directory() {
        Some(directory) => vec![OsString::from(directory)],
        None => vec![OsString::from("data"), OsString::from("js")],
    };
    ScopedDirectoryScope::new(roots).expect("固定 RPG Maker 写回顶层目录必须能建立候选编辑范围")
}

#[cfg(test)]
mod full_tree_tests;

/// 写回指定 RPG Maker 项目所需的输入。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WriteBackInput {
    pub name: ProjectName,
}

/// WriteBack 当前可被真实观测的业务阶段；只有存在权威分母的阶段才发布数量进度。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WriteBackProgressPhase {
    ReadingAssets,
    PlanningStandard,
    RewritingDocuments,
    PreparingCandidate,
    RunningLua,
    ValidatingCandidate,
    Publishing,
}

/// 一轮标准写回的正常业务汇总。
///
/// `manual_layout_units` 大于零仍表示写回成功：相应数据库译文会保持原样写入，
/// 调用方应把这些文本单元呈现为需要人工换行的诊断，而不是把它们升级为错误。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StandardWriteBackSummary {
    /// 已应用数据库译文的语义单元数。
    pub translated_units: usize,
    /// 因数据库中没有译文而保留冻结原文的语义单元数。
    pub original_units: usize,
    /// 成功应用自动换行的文本单元数。
    pub auto_wrapped_units: usize,
    /// 自动换行新增的换行符数。
    pub inserted_line_breaks: usize,
    /// 为续行新增的全角空格数。
    pub inserted_fullwidth_indents: usize,
    /// 保守布局无法安全处理、需要人工换行的文本单元数。
    pub manual_layout_units: usize,
}

/// 写回命令正常完成后交还给 CLI 的结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WriteBackOutput {
    pub name: ProjectName,
    /// 本轮已经发布、供后续封包消费的固定最新输出根目录。
    pub output_root: PathBuf,
    pub standard: StandardWriteBackSummary,
    pub lua_executed: bool,
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
    Published {
        standard: StandardWriteBackSummary,
        lua_executed: bool,
    },
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

/// 从项目数据库译文生成 Standard 文件候选。
///
/// 实现必须显式使用项目开启边界提供的三个区域行宽，并只对对话正文、滚动文本和
/// 帮助/说明框应用布局。模型给出的语义换行始终作为人工硬边界保留；只有超过对应
/// 区域行宽的语义行才参与兜底自动换行。每个文本先自动换行，再为符合条件的续行补全角空格。
/// 布局无法安全处理某个完整文本时，必须撤销该文本的自动布局、原样写入数据库译文，
/// 并在正常报告中累计人工项，而不是返回技术错误。
pub(crate) trait StandardWriteBack: Send + Sync {
    type Documents: Send + 'static;
    type Error: Error + Send + Sync + 'static;

    fn prepare(
        &self,
        project: &OpenedProject,
        layout_profile: &RpgMakerWriteBackLayoutProfile,
    ) -> impl Future<
        Output = Result<
            OperationCompletion<standard::StandardWriteBackPreparation<Self::Documents>>,
            Self::Error,
        >,
    > + Send;
}

/// 已准备但尚未发布的完整写回目录候选。
pub(crate) trait PreparedWriteBackCandidate: Send + 'static {
    fn belongs_to(&self, project: &OpenedProject) -> bool;
    fn candidate_root(&self) -> &Path;
}

/// 准备、借用式校验、发布或丢弃唯一写回候选的能力。
pub(crate) trait StandardWriteBackPublisher<D>: Send + Sync
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
    /// 该调用借用候选，不取得终结权；失败后调用方仍必须恰好一次丢弃候选。即使本轮
    /// 没有 Lua，调用方也必须执行此校验。
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
    fn into_write_back_failure_report(
        self,
        stage: DiagnosticStage,
        impact: DiagnosticImpact,
    ) -> FailureReport;
}

/// Lua 写回实现保留脚本、候选与 Host 子阶段后提供的安全投影。
pub(crate) trait WriteBackLuaDiagnostic: Sized {
    fn into_write_back_failure_report(self) -> FailureReport;
}

/// 在尚未发布的完整候选上运行一个可信 Lua 写回程序。
pub(crate) trait LuaWriteBack<C>: Send + Sync
where
    C: PreparedWriteBackCandidate,
{
    type Error: Error + Send + Sync + 'static;

    fn run(
        &self,
        project: &OpenedProject,
        candidate: &C,
        program: OwnedLuaProgram,
    ) -> impl Future<Output = Result<OperationCompletion<()>, Self::Error>> + Send;
}

/// 按固定业务顺序编排一次 RPG Maker 文本写回。
///
/// 用例只打开一次项目，先准备完整候选，再让可选 Lua 修改同一候选，最后只发布一次。
/// 候选产生后的取消或 Lua 失败只丢弃该候选；发布根接管 token 后，上层不再清理。
pub(crate) struct WriteBackService<O, S, P, L, J, K> {
    project_opener: O,
    standard_write_back: S,
    publisher: P,
    selected_lua: Option<SelectedLua<L>>,
    event_log: J,
    project_lease: K,
    cancellation: CooperativeCancellation,
    progress: Arc<dyn ProgressObserver<WriteBackProgressPhase>>,
}

impl<O, S, P, L, J, K> WriteBackService<O, S, P, L, J, K> {
    pub(crate) fn new(
        project_opener: O,
        standard_write_back: S,
        publisher: P,
        selected_lua: Option<SelectedLua<L>>,
        event_log: J,
        project_lease: K,
        cancellation: CooperativeCancellation,
    ) -> Self {
        Self {
            project_opener,
            standard_write_back,
            publisher,
            selected_lua,
            event_log,
            project_lease,
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

    fn observe(&self, phase: WriteBackProgressPhase) {
        self.progress
            .observe(ProgressSnapshot::indeterminate(phase));
    }
}

impl<O, S, P, L, J, K> WriteBackService<O, S, P, L, J, K>
where
    O: ExistingProjectOpener,
    S: StandardWriteBack,
    P: StandardWriteBackPublisher<S::Documents>,
    L: LuaWriteBack<P::Candidate>,
    J: WriteBackLog,
    K: ProjectCommandLeaseProvider,
{
    pub(crate) async fn execute(
        &self,
        input: WriteBackInput,
    ) -> Result<
        OperationCompletion<WriteBackOutput>,
        WriteBackServiceError<O::Error, S::Error, P::Error, L::Error, K::Error>,
    > {
        if self.cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }
        let WriteBackInput { name } = input;
        let _lease = self
            .project_lease
            .acquire(&name)
            .await
            .map_err(WriteBackServiceError::ProjectLease)?;
        if self.cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }
        let project = self
            .project_opener
            .open(&name)
            .await
            .map_err(WriteBackServiceError::OpenProject)?;
        if self.cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }

        let preparation = self
            .standard_write_back
            .prepare(&project, project.layout_profile())
            .await
            .map_err(WriteBackServiceError::Standard)?;
        let OperationCompletion::Completed(preparation) = preparation else {
            return Ok(OperationCompletion::Cancelled);
        };
        let (documents, standard, _) = preparation.into_parts();

        if self.cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }
        self.observe(WriteBackProgressPhase::PreparingCandidate);
        let candidate = self
            .publisher
            .prepare(&project, documents)
            .await
            .map_err(WriteBackServiceError::PrepareCandidate)?;

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

        let lua_executed = if let Some(selected_lua) = &self.selected_lua {
            self.observe(WriteBackProgressPhase::RunningLua);
            let error_script_path = selected_lua.script_path().to_path_buf();
            let lua_completion = selected_lua
                .executor()
                .run(&project, &candidate, selected_lua.program().clone())
                .await;
            match lua_completion {
                Ok(OperationCompletion::Completed(())) => true,
                Ok(OperationCompletion::Cancelled) => {
                    let candidate_root = candidate.candidate_root().to_path_buf();
                    return match self.publisher.discard(candidate).await {
                        Ok(()) => Ok(OperationCompletion::Cancelled),
                        Err(discard) => Err(WriteBackServiceError::CancellationDiscard {
                            candidate_root,
                            discard,
                        }),
                    };
                }
                Err(source) => {
                    let candidate_root = candidate.candidate_root().to_path_buf();
                    return match self.publisher.discard(candidate).await {
                        Ok(()) => Err(WriteBackServiceError::Lua {
                            script_path: error_script_path,
                            candidate_root,
                            source,
                        }),
                        Err(discard) => Err(WriteBackServiceError::LuaAndDiscard {
                            script_path: error_script_path,
                            candidate_root,
                            source,
                            discard,
                        }),
                    };
                }
            }
        } else {
            false
        };

        self.observe(WriteBackProgressPhase::ValidatingCandidate);
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
        self.observe(WriteBackProgressPhase::Publishing);
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
            outcome: WriteBackLogPublicationOutcome::Published {
                standard,
                lua_executed,
            },
        });

        Ok(OperationCompletion::Completed(WriteBackOutput {
            name: project.name().clone(),
            output_root,
            standard,
            lua_executed,
        }))
    }
}

/// WriteBack 顶层用例在打开、准备、候选终结与 Lua 边界遇到的阶段失败。
#[derive(Debug)]
pub(crate) enum WriteBackServiceError<OE, SE, PE, LE, KE> {
    ProjectLease(ProjectCommandLeaseError<KE>),
    CancellationDiscard {
        candidate_root: PathBuf,
        discard: PE,
    },
    OpenProject(OE),
    Standard(SE),
    PrepareCandidate(PE),
    Lua {
        script_path: PathBuf,
        candidate_root: PathBuf,
        source: LE,
    },
    LuaAndDiscard {
        script_path: PathBuf,
        candidate_root: PathBuf,
        source: LE,
        discard: PE,
    },
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

/// 写回失败已经造成的最高层用户影响。
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WriteBackFailureImpact {
    ProjectUnavailable,
    ProjectState,
    StateAppliedButFinalizationFailed,
    OutcomeUnknown,
    Internal,
}

#[cfg(test)]
impl<OE, SE, PE, LE, KE> WriteBackServiceError<OE, SE, PE, LE, KE> {
    /// 将候选与目录发布终态归并为命令边界可以准确呈现的用户影响。
    pub(crate) fn failure_impact(&self) -> WriteBackFailureImpact {
        use WriteBackFailureImpact as Impact;

        match self {
            Self::ProjectLease(_) => Impact::ProjectUnavailable,
            Self::CancellationDiscard { .. } => Impact::Internal,
            Self::OpenProject(_)
            | Self::Standard(_)
            | Self::PrepareCandidate(_)
            | Self::Lua { .. }
            | Self::LuaAndDiscard { .. }
            | Self::ValidateCandidate { .. }
            | Self::ValidateCandidateAndDiscard { .. } => Impact::ProjectState,
            Self::Publish { state, .. } => match state {
                WriteBackPublishFailureState::NotPublished { .. } => Impact::ProjectUnavailable,
                WriteBackPublishFailureState::PublishedWithResiduals { .. } => {
                    Impact::StateAppliedButFinalizationFailed
                }
                WriteBackPublishFailureState::RecoveryRequired { .. }
                | WriteBackPublishFailureState::OutcomeUnknown { .. } => Impact::OutcomeUnknown,
            },
        }
    }
}

impl<OE, SE, PE, LE, KE> fmt::Display for WriteBackServiceError<OE, SE, PE, LE, KE>
where
    OE: Error,
    SE: Error,
    PE: Error,
    LE: Error,
    KE: Error,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProjectLease(error) => error.fmt(formatter),
            Self::CancellationDiscard {
                candidate_root,
                discard,
            } => write!(
                formatter,
                "取消后无法丢弃写回候选 {}：{discard}",
                candidate_root.display()
            ),
            Self::OpenProject(source) => write!(formatter, "打开项目失败：{source}"),
            Self::Standard(source) => write!(formatter, "准备 Standard 写回失败：{source}"),
            Self::PrepareCandidate(source) => write!(formatter, "准备完整写回候选失败：{source}"),
            Self::Lua {
                script_path,
                candidate_root,
                source,
            } => write!(
                formatter,
                "Lua 写回候选失败（脚本：{}，候选：{}）：{source}",
                script_path.display(),
                candidate_root.display()
            ),
            Self::LuaAndDiscard {
                script_path,
                candidate_root,
                source,
                discard,
            } => write!(
                formatter,
                "Lua 写回候选失败（脚本：{}，候选：{}）：{source}；随后丢弃候选失败：{discard}",
                script_path.display(),
                candidate_root.display()
            ),
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

impl<OE, SE, PE, LE, KE> Error for WriteBackServiceError<OE, SE, PE, LE, KE>
where
    OE: Error + 'static,
    SE: Error + 'static,
    PE: Error + 'static,
    LE: Error + 'static,
    KE: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ProjectLease(error) => Some(error),
            Self::CancellationDiscard { discard, .. } => Some(discard),
            Self::OpenProject(source) => Some(source),
            Self::Standard(source) => Some(source),
            Self::PrepareCandidate(source)
            | Self::ValidateCandidate { source, .. }
            | Self::ValidateCandidateAndDiscard { source, .. }
            | Self::Publish { source, .. } => Some(source),
            Self::Lua { source, .. } | Self::LuaAndDiscard { source, .. } => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::progress::{ProgressObserver, ProgressSnapshot};
    use crate::rpg_maker::project::{MaxFullwidthChars, RpgMakerWriteBackLayoutProfile};

    #[derive(Clone, Default)]
    struct RecordingProgress(Arc<Mutex<Vec<ProgressSnapshot<WriteBackProgressPhase>>>>);

    impl ProgressObserver<WriteBackProgressPhase> for RecordingProgress {
        fn observe(&self, snapshot: ProgressSnapshot<WriteBackProgressPhase>) {
            self.0.lock().expect("进度记录锁不应中毒").push(snapshot);
        }
    }

    impl RecordingProgress {
        fn snapshots(&self) -> Vec<ProgressSnapshot<WriteBackProgressPhase>> {
            self.0.lock().expect("进度记录锁不应中毒").clone()
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum Event {
        Open(ProjectName),
        Standard(RpgMakerWriteBackLayoutProfile),
        PrepareCandidate,
        Lua {
            script_path: PathBuf,
            candidate_root: PathBuf,
        },
        ValidateCandidate,
        PublicationStarted,
        Publish,
        Discard,
        Log {
            lua_executed: bool,
        },
        LogPublishFailure,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakeError(&'static str);

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl Error for FakeError {}

    #[derive(Clone)]
    struct FakeOpener {
        events: Arc<Mutex<Vec<Event>>>,
        fail: bool,
    }

    impl ExistingProjectOpener for FakeOpener {
        type Error = FakeError;

        async fn open(&self, name: &ProjectName) -> Result<OpenedProject, Self::Error> {
            self.events
                .lock()
                .expect("事件锁不应中毒")
                .push(Event::Open(name.clone()));
            if self.fail {
                Err(FakeError("open"))
            } else {
                Ok(opened_project())
            }
        }
    }

    #[derive(Clone)]
    struct FakeStandardWriteBack {
        events: Arc<Mutex<Vec<Event>>>,
        fail: bool,
    }

    impl StandardWriteBack for FakeStandardWriteBack {
        type Documents = ();
        type Error = FakeError;

        async fn prepare(
            &self,
            _: &OpenedProject,
            layout_profile: &RpgMakerWriteBackLayoutProfile,
        ) -> Result<
            OperationCompletion<standard::StandardWriteBackPreparation<Self::Documents>>,
            Self::Error,
        > {
            self.events
                .lock()
                .expect("事件锁不应中毒")
                .push(Event::Standard(*layout_profile));
            if self.fail {
                Err(FakeError("standard"))
            } else {
                Ok(OperationCompletion::Completed(
                    standard::StandardWriteBackPreparation::new((), standard_summary(), Vec::new()),
                ))
            }
        }
    }

    #[derive(Clone, Copy)]
    struct FakeProjectLease;

    impl ProjectCommandLeaseProvider for FakeProjectLease {
        type Error = FakeError;
        type LeaseState = ();

        async fn acquire(
            &self,
            _: &ProjectName,
        ) -> Result<
            crate::rpg_maker::project_lease::ProjectCommandLease<Self::LeaseState>,
            ProjectCommandLeaseError<Self::Error>,
        > {
            Ok(crate::rpg_maker::project_lease::ProjectCommandLease::for_test(()))
        }
    }

    struct FakeCandidate {
        project_name: ProjectName,
        workspace_root: PathBuf,
        candidate_root: PathBuf,
        output_root: PathBuf,
    }

    impl PreparedWriteBackCandidate for FakeCandidate {
        fn belongs_to(&self, project: &OpenedProject) -> bool {
            self.project_name == *project.name()
                && self.workspace_root == project.workspace_root()
                && self.output_root == project.write_back_root()
        }

        fn candidate_root(&self) -> &Path {
            &self.candidate_root
        }
    }

    #[derive(Clone)]
    struct FakePublisher {
        events: Arc<Mutex<Vec<Event>>>,
        failing_stage: Option<&'static str>,
        cancel_after_prepare: Option<CooperativeCancellation>,
        cancel_after_validate: Option<CooperativeCancellation>,
    }

    impl StandardWriteBackPublisher<()> for FakePublisher {
        type Candidate = FakeCandidate;
        type Error = FakeError;

        async fn prepare(
            &self,
            project: &OpenedProject,
            (): (),
        ) -> Result<Self::Candidate, Self::Error> {
            self.events
                .lock()
                .expect("事件锁不应中毒")
                .push(Event::PrepareCandidate);
            if self.failing_stage == Some("prepare") {
                return Err(FakeError("prepare"));
            }
            if let Some(cancellation) = &self.cancel_after_prepare {
                cancellation.request();
            }
            Ok(FakeCandidate {
                project_name: project.name().clone(),
                workspace_root: project.workspace_root().to_path_buf(),
                candidate_root: project.workspace_root().join(".write_back-stage"),
                output_root: project.write_back_root().to_path_buf(),
            })
        }

        async fn publish(
            &self,
            candidate: Self::Candidate,
        ) -> Result<PublishedWriteBack, WriteBackPublishFailure<Self::Error>> {
            self.events
                .lock()
                .expect("事件锁不应中毒")
                .push(Event::Publish);
            if self.failing_stage == Some("publish") {
                return Err(WriteBackPublishFailure::new(
                    WriteBackPublishFailureState::NotPublished {
                        output_root: candidate.output_root.clone(),
                        residual_paths: Vec::new(),
                    },
                    FakeError("publish"),
                ));
            }
            Ok(PublishedWriteBack::new(candidate.output_root))
        }

        fn validate<'a>(
            &'a self,
            _: &Self::Candidate,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send + use<'a> {
            self.events
                .lock()
                .expect("事件锁不应中毒")
                .push(Event::ValidateCandidate);
            if let Some(cancellation) = &self.cancel_after_validate {
                cancellation.request();
            }
            let result = if matches!(self.failing_stage, Some("validate" | "validate-discard")) {
                Err(FakeError("validate"))
            } else {
                Ok(())
            };
            async move { result }
        }

        async fn discard(&self, _: Self::Candidate) -> Result<(), Self::Error> {
            self.events
                .lock()
                .expect("事件锁不应中毒")
                .push(Event::Discard);
            if matches!(
                self.failing_stage,
                Some(
                    "discard"
                        | "lua-discard"
                        | "validate-discard"
                        | "cancel-discard"
                        | "cancel-lua-discard"
                )
            ) {
                Err(FakeError("discard"))
            } else {
                Ok(())
            }
        }
    }

    #[derive(Clone)]
    struct FakeLuaWriteBack {
        events: Arc<Mutex<Vec<Event>>>,
        fail: bool,
        cancelled: bool,
    }

    impl LuaWriteBack<FakeCandidate> for FakeLuaWriteBack {
        type Error = FakeError;

        async fn run(
            &self,
            _: &OpenedProject,
            candidate: &FakeCandidate,
            program: OwnedLuaProgram,
        ) -> Result<OperationCompletion<()>, Self::Error> {
            self.events
                .lock()
                .expect("事件锁不应中毒")
                .push(Event::Lua {
                    script_path: program.main_script_path().to_path_buf(),
                    candidate_root: candidate.candidate_root().to_path_buf(),
                });
            if self.fail {
                Err(FakeError("lua"))
            } else if self.cancelled {
                Ok(OperationCompletion::Cancelled)
            } else {
                Ok(OperationCompletion::Completed(()))
            }
        }
    }

    #[derive(Clone)]
    struct FakeEventLog {
        events: Arc<Mutex<Vec<Event>>>,
    }

    impl WriteBackLog for FakeEventLog {
        fn emit(&self, event: WriteBackLogEvent) {
            let recorded = match event {
                WriteBackLogEvent::PublicationStarted { .. } => {
                    self.events
                        .lock()
                        .expect("事件锁不应中毒")
                        .push(Event::PublicationStarted);
                    return;
                }
                WriteBackLogEvent::PublicationFinished {
                    outcome: WriteBackLogPublicationOutcome::Published { lua_executed, .. },
                    ..
                } => Event::Log { lua_executed },
                WriteBackLogEvent::PublicationFinished { .. } => Event::LogPublishFailure,
            };
            self.events.lock().expect("事件锁不应中毒").push(recorded);
        }
    }

    type Service = WriteBackService<
        FakeOpener,
        FakeStandardWriteBack,
        FakePublisher,
        FakeLuaWriteBack,
        FakeEventLog,
        FakeProjectLease,
    >;

    fn service(
        events: Arc<Mutex<Vec<Event>>>,
        failing_stage: Option<&'static str>,
        lua_script: Option<&str>,
    ) -> Service {
        let cancellation = CooperativeCancellation::default();
        WriteBackService::new(
            FakeOpener {
                events: Arc::clone(&events),
                fail: failing_stage == Some("open"),
            },
            FakeStandardWriteBack {
                events: Arc::clone(&events),
                fail: failing_stage == Some("standard"),
            },
            FakePublisher {
                events: Arc::clone(&events),
                failing_stage,
                cancel_after_prepare: matches!(
                    failing_stage,
                    Some("cancel-after-prepare" | "cancel-discard")
                )
                .then(|| cancellation.clone()),
                cancel_after_validate: (failing_stage == Some("cancel-after-validate"))
                    .then(|| cancellation.clone()),
            },
            lua_script.map(|path| {
                SelectedLua::new(
                    OwnedLuaProgram::new(PathBuf::from(path), b"return nil".to_vec()),
                    FakeLuaWriteBack {
                        events: Arc::clone(&events),
                        fail: matches!(failing_stage, Some("lua" | "lua-discard")),
                        cancelled: matches!(
                            failing_stage,
                            Some("cancel-lua" | "cancel-lua-discard")
                        ),
                    },
                )
            }),
            FakeEventLog { events },
            FakeProjectLease,
            cancellation,
        )
    }

    fn project_name() -> ProjectName {
        "alice".parse().expect("测试项目名应合法")
    }

    fn max_fullwidth_chars(value: u32) -> MaxFullwidthChars {
        MaxFullwidthChars::new(value).expect("测试行宽应为正整数")
    }

    fn layout_profile() -> RpgMakerWriteBackLayoutProfile {
        RpgMakerWriteBackLayoutProfile::new(
            max_fullwidth_chars(24),
            max_fullwidth_chars(32),
            max_fullwidth_chars(18),
        )
    }

    fn opened_project() -> OpenedProject {
        OpenedProject::new(
            project_name(),
            PathBuf::from("C:/att/projects/alice"),
            PathBuf::from("C:/att/projects/alice/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            layout_profile(),
        )
    }

    fn output_root() -> PathBuf {
        PathBuf::from("C:/att/projects/alice").join("write_back")
    }

    fn candidate_root() -> PathBuf {
        PathBuf::from("C:/att/projects/alice").join(".write_back-stage")
    }

    fn standard_summary() -> StandardWriteBackSummary {
        StandardWriteBackSummary {
            translated_units: 31,
            original_units: 7,
            auto_wrapped_units: 5,
            inserted_line_breaks: 8,
            inserted_fullwidth_indents: 4,
            manual_layout_units: 0,
        }
    }

    fn input(_: Option<&str>) -> WriteBackInput {
        WriteBackInput {
            name: project_name(),
        }
    }

    fn events(recorded: &Arc<Mutex<Vec<Event>>>) -> Vec<Event> {
        recorded.lock().expect("事件锁不应中毒").clone()
    }

    #[tokio::test]
    async fn without_lua_still_validates_before_the_single_publish() {
        let recorded = Arc::new(Mutex::new(Vec::new()));

        let output = service(Arc::clone(&recorded), None, None)
            .execute(input(None))
            .await
            .expect("Standard 写回应成功");

        assert_eq!(
            output,
            OperationCompletion::Completed(WriteBackOutput {
                name: project_name(),
                output_root: output_root(),
                standard: standard_summary(),
                lua_executed: false,
            })
        );
        assert_eq!(
            events(&recorded),
            vec![
                Event::Open(project_name()),
                Event::Standard(layout_profile()),
                Event::PrepareCandidate,
                Event::ValidateCandidate,
                Event::PublicationStarted,
                Event::Publish,
                Event::Log {
                    lua_executed: false,
                },
            ]
        );
    }

    #[tokio::test]
    async fn lua_modifies_candidate_before_validation_and_the_single_publish() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let script_path = PathBuf::from("scripts/write_back.lua");

        let output = service(Arc::clone(&recorded), None, Some("scripts/write_back.lua"))
            .execute(input(Some("scripts/write_back.lua")))
            .await
            .expect("Lua 应在候选发布前完成");

        let OperationCompletion::Completed(output) = output else {
            panic!("写回应正常完成")
        };
        assert_eq!(output.standard.manual_layout_units, 0);
        assert!(output.lua_executed);
        assert_eq!(output.output_root, output_root());
        assert_eq!(
            events(&recorded),
            vec![
                Event::Open(project_name()),
                Event::Standard(layout_profile()),
                Event::PrepareCandidate,
                Event::Lua {
                    script_path,
                    candidate_root: candidate_root(),
                },
                Event::ValidateCandidate,
                Event::PublicationStarted,
                Event::Publish,
                Event::Log { lua_executed: true },
            ]
        );
    }

    #[tokio::test]
    async fn progress_reports_only_started_top_level_phases_in_business_order() {
        let progress = RecordingProgress::default();
        service(
            Arc::new(Mutex::new(Vec::new())),
            None,
            Some("scripts/write_back.lua"),
        )
        .with_progress(progress.clone())
        .execute(input(Some("scripts/write_back.lua")))
        .await
        .expect("带 Lua 的写回应成功");

        assert_eq!(
            progress.snapshots(),
            vec![
                ProgressSnapshot::indeterminate(WriteBackProgressPhase::PreparingCandidate),
                ProgressSnapshot::indeterminate(WriteBackProgressPhase::RunningLua),
                ProgressSnapshot::indeterminate(WriteBackProgressPhase::ValidatingCandidate),
                ProgressSnapshot::indeterminate(WriteBackProgressPhase::Publishing),
            ]
        );
    }

    #[tokio::test]
    async fn failed_or_cancelled_top_level_phase_does_not_start_later_phases() {
        for stage in ["lua", "cancel-after-prepare"] {
            let progress = RecordingProgress::default();
            let result = service(
                Arc::new(Mutex::new(Vec::new())),
                Some(stage),
                Some("scripts/write_back.lua"),
            )
            .with_progress(progress.clone())
            .execute(input(Some("scripts/write_back.lua")))
            .await;

            if stage == "lua" {
                result.expect_err("Lua 失败必须传播");
                assert_eq!(
                    progress.snapshots(),
                    vec![
                        ProgressSnapshot::indeterminate(WriteBackProgressPhase::PreparingCandidate,),
                        ProgressSnapshot::indeterminate(WriteBackProgressPhase::RunningLua),
                    ]
                );
            } else {
                assert_eq!(
                    result.expect("候选准备后取消应为正常结果"),
                    OperationCompletion::Cancelled
                );
                assert_eq!(
                    progress.snapshots(),
                    vec![ProgressSnapshot::indeterminate(
                        WriteBackProgressPhase::PreparingCandidate,
                    )]
                );
            }
        }
    }

    #[tokio::test]
    async fn lua_cancellation_discards_the_candidate_and_returns_normal_cancellation() {
        let recorded = Arc::new(Mutex::new(Vec::new()));

        let completion = service(
            Arc::clone(&recorded),
            Some("cancel-lua"),
            Some("scripts/write_back.lua"),
        )
        .execute(input(Some("scripts/write_back.lua")))
        .await
        .expect("Lua 取消且候选清理成功应是正常结果");

        assert_eq!(completion, OperationCompletion::Cancelled);
        assert_eq!(
            events(&recorded),
            vec![
                Event::Open(project_name()),
                Event::Standard(layout_profile()),
                Event::PrepareCandidate,
                Event::Lua {
                    script_path: PathBuf::from("scripts/write_back.lua"),
                    candidate_root: candidate_root(),
                },
                Event::Discard,
            ]
        );
    }

    #[tokio::test]
    async fn lua_cancellation_and_discard_failure_preserve_both_facts() {
        let recorded = Arc::new(Mutex::new(Vec::new()));

        let error = service(
            Arc::clone(&recorded),
            Some("cancel-lua-discard"),
            Some("scripts/write_back.lua"),
        )
        .execute(input(Some("scripts/write_back.lua")))
        .await
        .expect_err("取消后的候选清理失败必须显式返回");

        assert!(matches!(
            error,
            WriteBackServiceError::CancellationDiscard {
                candidate_root: failed_candidate,
                discard: FakeError("discard"),
            } if failed_candidate == candidate_root()
        ));
        assert_eq!(
            events(&recorded)
                .into_iter()
                .filter(|event| matches!(event, Event::Discard))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn first_technical_failure_stops_every_later_stage() {
        let cases = [
            ("open", vec![Event::Open(project_name())]),
            (
                "standard",
                vec![
                    Event::Open(project_name()),
                    Event::Standard(layout_profile()),
                ],
            ),
            (
                "prepare",
                vec![
                    Event::Open(project_name()),
                    Event::Standard(layout_profile()),
                    Event::PrepareCandidate,
                ],
            ),
            (
                "lua",
                vec![
                    Event::Open(project_name()),
                    Event::Standard(layout_profile()),
                    Event::PrepareCandidate,
                    Event::Lua {
                        script_path: PathBuf::from("scripts/write_back.lua"),
                        candidate_root: candidate_root(),
                    },
                    Event::Discard,
                ],
            ),
            (
                "cancel-after-prepare",
                vec![
                    Event::Open(project_name()),
                    Event::Standard(layout_profile()),
                    Event::PrepareCandidate,
                    Event::Discard,
                ],
            ),
            (
                "validate",
                vec![
                    Event::Open(project_name()),
                    Event::Standard(layout_profile()),
                    Event::PrepareCandidate,
                    Event::Lua {
                        script_path: PathBuf::from("scripts/write_back.lua"),
                        candidate_root: candidate_root(),
                    },
                    Event::ValidateCandidate,
                    Event::Discard,
                ],
            ),
            (
                "cancel-after-validate",
                vec![
                    Event::Open(project_name()),
                    Event::Standard(layout_profile()),
                    Event::PrepareCandidate,
                    Event::Lua {
                        script_path: PathBuf::from("scripts/write_back.lua"),
                        candidate_root: candidate_root(),
                    },
                    Event::ValidateCandidate,
                    Event::Discard,
                ],
            ),
            (
                "publish",
                vec![
                    Event::Open(project_name()),
                    Event::Standard(layout_profile()),
                    Event::PrepareCandidate,
                    Event::Lua {
                        script_path: PathBuf::from("scripts/write_back.lua"),
                        candidate_root: candidate_root(),
                    },
                    Event::ValidateCandidate,
                    Event::PublicationStarted,
                    Event::Publish,
                    Event::LogPublishFailure,
                ],
            ),
        ];

        for (stage, expected_events) in cases {
            let recorded = Arc::new(Mutex::new(Vec::new()));
            let result = service(
                Arc::clone(&recorded),
                Some(stage),
                Some("scripts/write_back.lua"),
            )
            .execute(input(Some("scripts/write_back.lua")))
            .await;

            if matches!(stage, "cancel-after-prepare" | "cancel-after-validate") {
                assert_eq!(
                    result.expect("取消应是正常结果"),
                    OperationCompletion::Cancelled
                );
                assert_eq!(events(&recorded), expected_events);
                continue;
            }
            let error = result.expect_err("指定技术阶段应失败");

            match stage {
                "open" => assert!(matches!(
                    error,
                    WriteBackServiceError::OpenProject(FakeError("open"))
                )),
                "standard" => assert!(matches!(
                    error,
                    WriteBackServiceError::Standard(FakeError("standard"))
                )),
                "prepare" => assert!(matches!(
                    error,
                    WriteBackServiceError::PrepareCandidate(FakeError("prepare"))
                )),
                "lua" => assert!(matches!(
                    error,
                    WriteBackServiceError::Lua {
                        source: FakeError("lua"),
                        ..
                    }
                )),
                "validate" => assert!(matches!(
                    error,
                    WriteBackServiceError::ValidateCandidate {
                        source: FakeError("validate"),
                        ..
                    }
                )),
                "publish" => {
                    assert!(matches!(
                        &error,
                        WriteBackServiceError::Publish {
                            state: WriteBackPublishFailureState::NotPublished { .. },
                            source: FakeError("publish"),
                        }
                    ));
                    assert_eq!(
                        error.failure_impact(),
                        WriteBackFailureImpact::ProjectUnavailable
                    );
                }
                _ => unreachable!("测试只包含已知阶段"),
            }
            assert_eq!(events(&recorded), expected_events);
        }
    }

    #[tokio::test]
    async fn lua_failure_discards_candidate_and_preserves_primary_source() {
        let recorded = Arc::new(Mutex::new(Vec::new()));

        let error = service(
            Arc::clone(&recorded),
            Some("lua"),
            Some("custom/write_back.lua"),
        )
        .execute(input(Some("custom/write_back.lua")))
        .await
        .expect_err("Lua 技术失败应返回错误");

        assert!(matches!(
            &error,
            WriteBackServiceError::Lua {
                script_path,
                candidate_root: failed_candidate,
                source: FakeError("lua"),
            } if script_path == &PathBuf::from("custom/write_back.lua")
                && failed_candidate == &candidate_root()
        ));
        assert_eq!(
            error.source().and_then(|source| source.downcast_ref()),
            Some(&FakeError("lua"))
        );
        let message = error.to_string();
        assert!(message.contains("custom/write_back.lua"), "{message}");
        assert!(
            message.contains(&candidate_root().display().to_string()),
            "{message}"
        );
        assert_eq!(
            events(&recorded)
                .into_iter()
                .filter(|event| matches!(event, Event::Discard))
                .count(),
            1
        );
        assert!(!events(&recorded).contains(&Event::Publish));
        assert!(
            !events(&recorded)
                .iter()
                .any(|event| matches!(event, Event::Log { .. }))
        );
    }

    #[tokio::test]
    async fn validation_failure_without_lua_discards_once_and_never_publishes() {
        let recorded = Arc::new(Mutex::new(Vec::new()));

        let error = service(Arc::clone(&recorded), Some("validate"), None)
            .execute(input(None))
            .await
            .expect_err("无 Lua 的完整候选也必须通过发布前校验");

        assert!(matches!(
            &error,
            WriteBackServiceError::ValidateCandidate {
                candidate_root: failed_candidate,
                source: FakeError("validate"),
            } if failed_candidate == &candidate_root()
        ));
        assert_eq!(
            error.source().and_then(|source| source.downcast_ref()),
            Some(&FakeError("validate"))
        );
        assert_eq!(
            events(&recorded),
            vec![
                Event::Open(project_name()),
                Event::Standard(layout_profile()),
                Event::PrepareCandidate,
                Event::ValidateCandidate,
                Event::Discard,
            ]
        );
    }

    #[tokio::test]
    async fn lua_and_discard_failure_preserves_both_errors() {
        let recorded = Arc::new(Mutex::new(Vec::new()));

        let error = service(
            Arc::clone(&recorded),
            Some("lua-discard"),
            Some("broken.lua"),
        )
        .execute(input(Some("broken.lua")))
        .await
        .expect_err("Lua 与清理双重失败必须同时保留");

        assert!(matches!(
            error,
            WriteBackServiceError::LuaAndDiscard {
                source: FakeError("lua"),
                discard: FakeError("discard"),
                ..
            }
        ));
        let recorded = events(&recorded);
        assert_eq!(
            recorded
                .iter()
                .filter(|event| matches!(event, Event::Discard))
                .count(),
            1
        );
        assert!(!recorded.contains(&Event::Publish));
    }

    #[tokio::test]
    async fn validation_and_discard_failure_preserves_both_errors() {
        let recorded = Arc::new(Mutex::new(Vec::new()));

        let error = service(
            Arc::clone(&recorded),
            Some("validate-discard"),
            Some("write.lua"),
        )
        .execute(input(Some("write.lua")))
        .await
        .expect_err("完整候选校验与清理双重失败必须同时保留");

        assert!(matches!(
            error,
            WriteBackServiceError::ValidateCandidateAndDiscard {
                source: FakeError("validate"),
                discard: FakeError("discard"),
                ..
            }
        ));
        assert_eq!(
            events(&recorded),
            vec![
                Event::Open(project_name()),
                Event::Standard(layout_profile()),
                Event::PrepareCandidate,
                Event::Lua {
                    script_path: PathBuf::from("write.lua"),
                    candidate_root: candidate_root(),
                },
                Event::ValidateCandidate,
                Event::Discard,
            ]
        );
    }

    #[tokio::test]
    async fn cancellation_and_discard_failure_preserves_both_outcomes() {
        let recorded = Arc::new(Mutex::new(Vec::new()));

        let error = service(Arc::clone(&recorded), Some("cancel-discard"), None)
            .execute(input(None))
            .await
            .expect_err("取消后的清理失败必须同时保留");

        assert!(matches!(
            error,
            WriteBackServiceError::CancellationDiscard {
                discard: FakeError("discard"),
                ..
            }
        ));
        assert_eq!(
            events(&recorded)
                .iter()
                .filter(|event| matches!(event, Event::Discard))
                .count(),
            1
        );
    }

    #[test]
    fn execution_future_is_send() {
        fn assert_send(_: impl Send) {}

        let service = service(Arc::new(Mutex::new(Vec::new())), None, None);
        assert_send(service.execute(input(None)));
    }
}
