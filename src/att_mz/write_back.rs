//! MZ 数据库译文写回冻结项目副本的顶层编排。

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};

use super::ProjectName;
use super::project::{ExistingProjectOpener, MzWriteBackLayoutProfile, OpenedProject};
use crate::execution::{CooperativeCancellation, OperationCancelled};
use crate::observability::PersistentEventLog;

pub(crate) mod asset_reader;
pub(crate) mod lua;
pub(crate) mod publisher;
pub(crate) mod rewriter;
pub(crate) mod standard;

#[cfg(test)]
mod full_tree_tests;

/// 写回指定 MZ 项目所需的输入。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WriteBackInput {
    pub name: ProjectName,
    /// 可选的可信 Lua 写回程序；Lua 自己负责自己的文件和数据库事务。
    pub lua_script: Option<PathBuf>,
}

/// 一轮标准写回的正常业务汇总。
///
/// `manual_layout_units` 大于零仍表示写回成功：相应数据库译文会保持原样写入，
/// 调用方应把这些位置呈现为需要人工换行的诊断，而不是把它们升级为错误。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StandardWriteBackSummary {
    /// 已应用数据库译文的位置数。
    pub translated_locations: usize,
    /// 因数据库中没有译文而保留冻结原文的位置数。
    pub original_locations: usize,
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

/// 完成一个 MZ 项目文本写回用例。
pub trait WriteBackUseCase: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn execute(
        &self,
        input: WriteBackInput,
    ) -> impl Future<Output = Result<WriteBackOutput, Self::Error>> + Send;
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

/// 从项目数据库译文生成 Standard 文件候选。
///
/// 实现必须显式使用项目开启边界提供的三个区域行宽，并只对对话正文、滚动文本和
/// 帮助/说明框应用布局。帮助/说明框仅在原文已有换行时参与自动换行；译文已有换行
/// 始终作为人工硬边界保留。每个文本先自动换行，再为符合条件的续行补全角空格。
/// 布局无法安全处理某个完整文本时，必须撤销该文本的自动布局、原样写入数据库译文，
/// 并在正常报告中累计人工项，而不是返回技术错误。
pub(crate) trait StandardWriteBack: Send + Sync {
    type Documents: Send + 'static;
    type Error: Error + Send + Sync + 'static;

    fn prepare(
        &self,
        project: &OpenedProject,
        layout_profile: &MzWriteBackLayoutProfile,
    ) -> impl Future<
        Output = Result<standard::StandardWriteBackPreparation<Self::Documents>, Self::Error>,
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
    ) -> impl Future<Output = Result<PublishedWriteBack, Self::Error>> + Send;

    fn discard(
        &self,
        candidate: Self::Candidate,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
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
        script_path: PathBuf,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// 按固定业务顺序编排一次 MZ 文本写回。
///
/// 用例只打开一次项目，先准备完整候选，再让可选 Lua 修改同一候选，最后只发布一次。
/// 候选产生后的取消或 Lua 失败只丢弃该候选；发布根接管 token 后，上层不再清理。
pub(crate) struct WriteBackService<O, S, P, L, J> {
    project_opener: O,
    standard_write_back: S,
    publisher: P,
    lua_write_back: Option<L>,
    event_log: J,
    cancellation: CooperativeCancellation,
}

impl<O, S, P, L, J> WriteBackService<O, S, P, L, J> {
    pub(crate) fn new(
        project_opener: O,
        standard_write_back: S,
        publisher: P,
        lua_write_back: Option<L>,
        event_log: J,
        cancellation: CooperativeCancellation,
    ) -> Self {
        Self {
            project_opener,
            standard_write_back,
            publisher,
            lua_write_back,
            event_log,
            cancellation,
        }
    }
}

impl<O, S, P, L, J> WriteBackUseCase for WriteBackService<O, S, P, L, J>
where
    O: ExistingProjectOpener,
    S: StandardWriteBack,
    P: StandardWriteBackPublisher<S::Documents>,
    L: LuaWriteBack<P::Candidate>,
    J: PersistentEventLog<standard::WriteBackRunLog>,
{
    type Error = WriteBackServiceError<O::Error, S::Error, P::Error, L::Error, J::Error>;

    async fn execute(&self, input: WriteBackInput) -> Result<WriteBackOutput, Self::Error> {
        self.cancellation
            .check()
            .map_err(WriteBackServiceError::Cancelled)?;
        let WriteBackInput { name, lua_script } = input;
        if lua_script.is_some() && self.lua_write_back.is_none() {
            return Err(WriteBackServiceError::MissingLuaDependency);
        }
        let project = self
            .project_opener
            .open(&name)
            .await
            .map_err(WriteBackServiceError::OpenProject)?;
        self.cancellation
            .check()
            .map_err(WriteBackServiceError::Cancelled)?;

        let preparation = self
            .standard_write_back
            .prepare(&project, project.layout_profile())
            .await
            .map_err(WriteBackServiceError::Standard)?;
        let (documents, standard, manual_layout_diagnostics) = preparation.into_parts();

        self.cancellation
            .check()
            .map_err(WriteBackServiceError::Cancelled)?;
        let candidate = self
            .publisher
            .prepare(&project, documents)
            .await
            .map_err(WriteBackServiceError::PrepareCandidate)?;

        if let Err(cancellation) = self.cancellation.check() {
            let candidate_root = candidate.candidate_root().to_path_buf();
            return match self.publisher.discard(candidate).await {
                Ok(()) => Err(WriteBackServiceError::Cancelled(cancellation)),
                Err(discard) => Err(WriteBackServiceError::CancelledAndDiscard {
                    cancellation,
                    candidate_root,
                    discard,
                }),
            };
        }

        let lua_executed = if let Some(script_path) = lua_script {
            let error_script_path = script_path.clone();
            let lua = self
                .lua_write_back
                .as_ref()
                .expect("Lua 依赖已在产生候选前验证");
            if let Err(source) = lua.run(&project, &candidate, script_path).await {
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
            true
        } else {
            false
        };

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

        if let Err(cancellation) = self.cancellation.check() {
            let candidate_root = candidate.candidate_root().to_path_buf();
            return match self.publisher.discard(candidate).await {
                Ok(()) => Err(WriteBackServiceError::Cancelled(cancellation)),
                Err(discard) => Err(WriteBackServiceError::CancelledAndDiscard {
                    cancellation,
                    candidate_root,
                    discard,
                }),
            };
        }

        // 借用式业务校验已经通过；`publish` 现在按值接管 token，并在实际目标交换前
        // 再次复核完整候选以覆盖检查与使用之间的变化。从此边界开始，无论根返回何种
        // 终态，上层都不得再次尝试 discard。
        let published = self
            .publisher
            .publish(candidate)
            .await
            .map_err(WriteBackServiceError::Publish)?;
        let output_root = published.output_root().to_path_buf();

        self.event_log
            .append(standard::WriteBackRunLog::new(
                &project,
                *project.layout_profile(),
                standard,
                manual_layout_diagnostics,
                lua_executed,
            ))
            .await
            .map_err(|source| WriteBackServiceError::RecordPublishedRun {
                output_root: output_root.clone(),
                lua_executed,
                source,
            })?;

        Ok(WriteBackOutput {
            name: project.name().clone(),
            output_root,
            standard,
            lua_executed,
        })
    }
}

/// WriteBack 顶层用例在打开、准备、候选终结、Lua 与日志边界遇到的阶段失败。
#[derive(Debug)]
pub(crate) enum WriteBackServiceError<OE, SE, PE, LE, JE> {
    Cancelled(OperationCancelled),
    CancelledAndDiscard {
        cancellation: OperationCancelled,
        candidate_root: PathBuf,
        discard: PE,
    },
    OpenProject(OE),
    Standard(SE),
    PrepareCandidate(PE),
    MissingLuaDependency,
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
    Publish(PE),
    RecordPublishedRun {
        output_root: PathBuf,
        lua_executed: bool,
        source: JE,
    },
}

impl<OE, SE, PE, LE, JE> fmt::Display for WriteBackServiceError<OE, SE, PE, LE, JE>
where
    OE: Error,
    SE: Error,
    PE: Error,
    LE: Error,
    JE: Error,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled(error) => error.fmt(formatter),
            Self::CancelledAndDiscard {
                cancellation,
                candidate_root,
                discard,
            } => write!(
                formatter,
                "{cancellation}，且无法丢弃写回候选 {}：{discard}",
                candidate_root.display()
            ),
            Self::OpenProject(source) => write!(formatter, "打开项目失败：{source}"),
            Self::Standard(source) => write!(formatter, "准备 Standard 写回失败：{source}"),
            Self::PrepareCandidate(source) => write!(formatter, "准备完整写回候选失败：{source}"),
            Self::MissingLuaDependency => {
                formatter.write_str("本次选择了 Lua 写回，但未构造 Lua Runtime")
            }
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
            Self::Publish(source) => write!(formatter, "发布完整写回候选失败：{source}"),
            Self::RecordPublishedRun {
                output_root,
                lua_executed,
                source,
            } => write!(
                formatter,
                "完整写回输出已发布到 {}（Lua 已执行：{lua_executed}），但持久日志记录失败：{source}",
                output_root.display()
            ),
        }
    }
}

impl<OE, SE, PE, LE, JE> Error for WriteBackServiceError<OE, SE, PE, LE, JE>
where
    OE: Error + 'static,
    SE: Error + 'static,
    PE: Error + 'static,
    LE: Error + 'static,
    JE: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Cancelled(error) => Some(error),
            Self::CancelledAndDiscard { cancellation, .. } => Some(cancellation),
            Self::OpenProject(source) => Some(source),
            Self::Standard(source) => Some(source),
            Self::PrepareCandidate(source)
            | Self::ValidateCandidate { source, .. }
            | Self::ValidateCandidateAndDiscard { source, .. }
            | Self::Publish(source) => Some(source),
            Self::MissingLuaDependency => None,
            Self::Lua { source, .. } | Self::LuaAndDiscard { source, .. } => Some(source),
            Self::RecordPublishedRun { source, .. } => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::att_mz::project::{MaxFullwidthChars, MzWriteBackLayoutProfile};

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum Event {
        Open(ProjectName),
        Standard(MzWriteBackLayoutProfile),
        PrepareCandidate,
        Lua {
            script_path: PathBuf,
            candidate_root: PathBuf,
        },
        ValidateCandidate,
        Publish,
        Discard,
        Log {
            lua_executed: bool,
        },
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
            layout_profile: &MzWriteBackLayoutProfile,
        ) -> Result<standard::StandardWriteBackPreparation<Self::Documents>, Self::Error> {
            self.events
                .lock()
                .expect("事件锁不应中毒")
                .push(Event::Standard(*layout_profile));
            if self.fail {
                Err(FakeError("standard"))
            } else {
                Ok(standard::StandardWriteBackPreparation::new(
                    (),
                    standard_summary(),
                    Vec::new(),
                ))
            }
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
        ) -> Result<PublishedWriteBack, Self::Error> {
            self.events
                .lock()
                .expect("事件锁不应中毒")
                .push(Event::Publish);
            if self.failing_stage == Some("publish") {
                return Err(FakeError("publish"));
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
                Some("discard" | "lua-discard" | "validate-discard" | "cancel-discard")
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
    }

    impl LuaWriteBack<FakeCandidate> for FakeLuaWriteBack {
        type Error = FakeError;

        async fn run(
            &self,
            _: &OpenedProject,
            candidate: &FakeCandidate,
            script_path: PathBuf,
        ) -> Result<(), Self::Error> {
            self.events
                .lock()
                .expect("事件锁不应中毒")
                .push(Event::Lua {
                    script_path,
                    candidate_root: candidate.candidate_root().to_path_buf(),
                });
            if self.fail {
                Err(FakeError("lua"))
            } else {
                Ok(())
            }
        }
    }

    #[derive(Clone)]
    struct FakeEventLog {
        events: Arc<Mutex<Vec<Event>>>,
        fail: bool,
    }

    impl PersistentEventLog<standard::WriteBackRunLog> for FakeEventLog {
        type Error = FakeError;

        async fn append(&self, event: standard::WriteBackRunLog) -> Result<(), Self::Error> {
            self.events
                .lock()
                .expect("事件锁不应中毒")
                .push(Event::Log {
                    lua_executed: event.lua_executed(),
                });
            if self.fail {
                Err(FakeError("log"))
            } else {
                Ok(())
            }
        }
    }

    type Service = WriteBackService<
        FakeOpener,
        FakeStandardWriteBack,
        FakePublisher,
        FakeLuaWriteBack,
        FakeEventLog,
    >;

    fn service(events: Arc<Mutex<Vec<Event>>>, failing_stage: Option<&'static str>) -> Service {
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
            Some(FakeLuaWriteBack {
                events: Arc::clone(&events),
                fail: matches!(failing_stage, Some("lua" | "lua-discard")),
            }),
            FakeEventLog {
                events,
                fail: failing_stage == Some("log"),
            },
            cancellation,
        )
    }

    fn project_name() -> ProjectName {
        "alice".parse().expect("测试项目名应合法")
    }

    fn max_fullwidth_chars(value: u32) -> MaxFullwidthChars {
        MaxFullwidthChars::new(value).expect("测试行宽应为正整数")
    }

    fn layout_profile() -> MzWriteBackLayoutProfile {
        MzWriteBackLayoutProfile::new(
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
            translated_locations: 31,
            original_locations: 7,
            auto_wrapped_units: 5,
            inserted_line_breaks: 8,
            inserted_fullwidth_indents: 4,
            manual_layout_units: 0,
        }
    }

    fn input(lua_script: Option<&str>) -> WriteBackInput {
        WriteBackInput {
            name: project_name(),
            lua_script: lua_script.map(PathBuf::from),
        }
    }

    fn events(recorded: &Arc<Mutex<Vec<Event>>>) -> Vec<Event> {
        recorded.lock().expect("事件锁不应中毒").clone()
    }

    #[tokio::test]
    async fn without_lua_still_validates_before_the_single_publish() {
        let recorded = Arc::new(Mutex::new(Vec::new()));

        let output = service(Arc::clone(&recorded), None)
            .execute(input(None))
            .await
            .expect("Standard 写回应成功");

        assert_eq!(
            output,
            WriteBackOutput {
                name: project_name(),
                output_root: output_root(),
                standard: standard_summary(),
                lua_executed: false,
            }
        );
        assert_eq!(
            events(&recorded),
            vec![
                Event::Open(project_name()),
                Event::Standard(layout_profile()),
                Event::PrepareCandidate,
                Event::ValidateCandidate,
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

        let output = service(Arc::clone(&recorded), None)
            .execute(input(Some("scripts/write_back.lua")))
            .await
            .expect("Lua 应在候选发布前完成");

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
                Event::Publish,
                Event::Log { lua_executed: true },
            ]
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
                    Event::Publish,
                ],
            ),
        ];

        for (stage, expected_events) in cases {
            let recorded = Arc::new(Mutex::new(Vec::new()));
            let error = service(Arc::clone(&recorded), Some(stage))
                .execute(input(Some("scripts/write_back.lua")))
                .await
                .expect_err("指定技术阶段应失败");

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
                "cancel-after-prepare" | "cancel-after-validate" => {
                    assert!(matches!(error, WriteBackServiceError::Cancelled(_)))
                }
                "validate" => assert!(matches!(
                    error,
                    WriteBackServiceError::ValidateCandidate {
                        source: FakeError("validate"),
                        ..
                    }
                )),
                "publish" => assert!(matches!(
                    error,
                    WriteBackServiceError::Publish(FakeError("publish"))
                )),
                _ => unreachable!("测试只包含已知阶段"),
            }
            assert_eq!(events(&recorded), expected_events);
        }
    }

    #[tokio::test]
    async fn lua_failure_discards_candidate_and_preserves_primary_source() {
        let recorded = Arc::new(Mutex::new(Vec::new()));

        let error = service(Arc::clone(&recorded), Some("lua"))
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

        let error = service(Arc::clone(&recorded), Some("validate"))
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
    async fn log_failure_reports_that_publish_already_took_effect() {
        let recorded = Arc::new(Mutex::new(Vec::new()));

        let error = service(Arc::clone(&recorded), Some("log"))
            .execute(input(Some("write.lua")))
            .await
            .expect_err("日志失败必须显式传播");

        assert!(matches!(
            error,
            WriteBackServiceError::RecordPublishedRun {
                output_root: published_root,
                lua_executed: true,
                source: FakeError("log"),
            } if published_root == output_root()
        ));
        let recorded = events(&recorded);
        assert_eq!(
            recorded
                .iter()
                .filter(|event| matches!(event, Event::Publish))
                .count(),
            1
        );
        assert!(!recorded.contains(&Event::Discard));
    }

    #[tokio::test]
    async fn lua_and_discard_failure_preserves_both_errors() {
        let recorded = Arc::new(Mutex::new(Vec::new()));

        let error = service(Arc::clone(&recorded), Some("lua-discard"))
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

        let error = service(Arc::clone(&recorded), Some("validate-discard"))
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
    async fn missing_lua_dependency_is_rejected_before_opening_the_project() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let service: Service = WriteBackService::new(
            FakeOpener {
                events: Arc::clone(&recorded),
                fail: false,
            },
            FakeStandardWriteBack {
                events: Arc::clone(&recorded),
                fail: false,
            },
            FakePublisher {
                events: Arc::clone(&recorded),
                failing_stage: None,
                cancel_after_prepare: None,
                cancel_after_validate: None,
            },
            None,
            FakeEventLog {
                events: Arc::clone(&recorded),
                fail: false,
            },
            CooperativeCancellation::default(),
        );

        let error = service
            .execute(input(Some("write.lua")))
            .await
            .expect_err("缺少 Lua 根必须在任何业务副作用前失败");

        assert!(matches!(error, WriteBackServiceError::MissingLuaDependency));
        assert!(events(&recorded).is_empty());
    }

    #[tokio::test]
    async fn cancellation_and_discard_failure_preserves_both_outcomes() {
        let recorded = Arc::new(Mutex::new(Vec::new()));

        let error = service(Arc::clone(&recorded), Some("cancel-discard"))
            .execute(input(None))
            .await
            .expect_err("取消后的清理失败必须同时保留");

        assert!(matches!(
            error,
            WriteBackServiceError::CancelledAndDiscard {
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

        let service = service(Arc::new(Mutex::new(Vec::new())), None);
        assert_send(service.execute(input(None)));
    }
}
