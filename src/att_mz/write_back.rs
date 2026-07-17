//! MZ 数据库译文写回冻结项目副本的顶层编排。

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};

use super::ProjectName;
use super::project::{ExistingProjectOpener, MzWriteBackLayoutProfile, OpenedProject};
use crate::execution::{CooperativeCancellation, OperationCancelled};

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

/// Standard 阶段已经完整发布、可继续交给可信 Lua 修改的输出。
///
/// 构造此值意味着 Standard 阶段的发布事务已经成功；顶层用例不会因后续 Lua
/// 失败而回滚它。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PublishedWriteBack {
    project_name: ProjectName,
    workspace_root: PathBuf,
    output_root: PathBuf,
}

impl PublishedWriteBack {
    /// 为已经成功发布 Standard 输出的项目建立交接令牌。
    ///
    /// 输出位置只能来自受信项目上下文，调用方不能用任意路径伪造已发布结果。
    pub(crate) fn new(project: &OpenedProject) -> Self {
        Self {
            project_name: project.name().clone(),
            workspace_root: project.workspace_root().to_path_buf(),
            output_root: project.write_back_root().to_path_buf(),
        }
    }

    pub(crate) fn belongs_to(&self, project: &OpenedProject) -> bool {
        self.project_name == *project.name()
            && self.workspace_root == project.workspace_root()
            && self.output_root == project.write_back_root()
    }

    pub(crate) fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub(crate) fn output_root(&self) -> &Path {
        &self.output_root
    }
}

/// Standard 写回成功后交给顶层编排的已发布输出及业务汇总。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StandardWriteBackReport {
    published: PublishedWriteBack,
    summary: StandardWriteBackSummary,
}

impl StandardWriteBackReport {
    pub(crate) fn new(published: PublishedWriteBack, summary: StandardWriteBackSummary) -> Self {
        Self { published, summary }
    }

    pub(crate) fn into_parts(self) -> (PublishedWriteBack, StandardWriteBackSummary) {
        (self.published, self.summary)
    }
}

/// 从项目数据库译文生成并发布固定最新 `data/js` 输出。
///
/// 实现必须显式使用项目开启边界提供的三个区域行宽，并只对对话正文、滚动文本和
/// 帮助/说明框应用布局。帮助/说明框仅在原文已有换行时参与自动换行；译文已有换行
/// 始终作为人工硬边界保留。每个文本先自动换行，再为符合条件的续行补全角空格。
/// 布局无法安全处理某个完整文本时，必须撤销该文本的自动布局、原样写入数据库译文，
/// 并在正常报告中累计人工项，而不是返回技术错误。
pub(crate) trait StandardWriteBack: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn run(
        &self,
        project: &OpenedProject,
        layout_profile: &MzWriteBackLayoutProfile,
    ) -> impl Future<Output = Result<StandardWriteBackReport, Self::Error>> + Send;
}

/// 在 Standard 已发布输出上运行一个可信 Lua 写回程序。
///
/// Lua 实现完整拥有自己的协议、文件修改和数据库事务；本层只规定它必须接收与
/// Standard 报告相同的已发布输出。
pub(crate) trait LuaWriteBack: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn run(
        &self,
        project: &OpenedProject,
        published: &PublishedWriteBack,
        script_path: PathBuf,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// 按固定业务顺序编排一次 MZ 文本写回。
///
/// 用例只打开一次项目，随后先运行 Standard，再运行可选 Lua。首个技术失败阻止
/// 后续阶段；已经成功发布的 Standard 输出不会因 Lua 失败而被组合回滚。
pub(crate) struct WriteBackService<O, S, L> {
    project_opener: O,
    standard_write_back: S,
    lua_write_back: Option<L>,
    cancellation: CooperativeCancellation,
}

impl<O, S, L> WriteBackService<O, S, L> {
    pub(crate) fn new(
        project_opener: O,
        standard_write_back: S,
        lua_write_back: Option<L>,
        cancellation: CooperativeCancellation,
    ) -> Self {
        Self {
            project_opener,
            standard_write_back,
            lua_write_back,
            cancellation,
        }
    }
}

impl<O, S, L> WriteBackUseCase for WriteBackService<O, S, L>
where
    O: ExistingProjectOpener,
    S: StandardWriteBack,
    L: LuaWriteBack,
{
    type Error = WriteBackServiceError<O::Error, S::Error, L::Error>;

    async fn execute(&self, input: WriteBackInput) -> Result<WriteBackOutput, Self::Error> {
        self.cancellation
            .check()
            .map_err(WriteBackServiceError::Cancelled)?;
        let WriteBackInput { name, lua_script } = input;
        let project = self
            .project_opener
            .open(&name)
            .await
            .map_err(WriteBackServiceError::OpenProject)?;
        self.cancellation
            .check()
            .map_err(WriteBackServiceError::Cancelled)?;

        let report = self
            .standard_write_back
            .run(&project, project.layout_profile())
            .await
            .map_err(WriteBackServiceError::Standard)?;
        let (published, standard) = report.into_parts();
        let output_root = published.output_root().to_path_buf();

        self.cancellation
            .check()
            .map_err(WriteBackServiceError::Cancelled)?;

        let lua_executed = if let Some(script_path) = lua_script {
            let error_script_path = script_path.clone();
            self.lua_write_back
                .as_ref()
                .ok_or(WriteBackServiceError::MissingLuaDependency)?
                .run(&project, &published, script_path)
                .await
                .map_err(|source| WriteBackServiceError::Lua {
                    script_path: error_script_path,
                    output_root: output_root.clone(),
                    source,
                })?;
            true
        } else {
            false
        };

        Ok(WriteBackOutput {
            name: project.name().clone(),
            output_root,
            standard,
            lua_executed,
        })
    }
}

/// WriteBack 顶层用例在三个直接依赖边界上遇到的阶段失败。
#[derive(Debug)]
pub(crate) enum WriteBackServiceError<OE, SE, LE> {
    Cancelled(OperationCancelled),
    OpenProject(OE),
    Standard(SE),
    MissingLuaDependency,
    Lua {
        script_path: PathBuf,
        output_root: PathBuf,
        source: LE,
    },
}

impl<OE, SE, LE> fmt::Display for WriteBackServiceError<OE, SE, LE>
where
    OE: Error,
    SE: Error,
    LE: Error,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled(error) => error.fmt(formatter),
            Self::OpenProject(source) => write!(formatter, "打开项目失败：{source}"),
            Self::Standard(source) => write!(formatter, "Standard 写回失败：{source}"),
            Self::MissingLuaDependency => {
                formatter.write_str("本次选择了 Lua 写回，但未构造 Lua Runtime")
            }
            Self::Lua {
                script_path,
                output_root,
                source,
            } => write!(
                formatter,
                "Lua 写回失败（脚本：{}，已发布输出：{}）：{source}",
                script_path.display(),
                output_root.display()
            ),
        }
    }
}

impl<OE, SE, LE> Error for WriteBackServiceError<OE, SE, LE>
where
    OE: Error + 'static,
    SE: Error + 'static,
    LE: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Cancelled(error) => Some(error),
            Self::OpenProject(source) => Some(source),
            Self::Standard(source) => Some(source),
            Self::MissingLuaDependency => None,
            Self::Lua { source, .. } => Some(source),
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
        Lua {
            script_path: PathBuf,
            output_root: PathBuf,
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
        type Error = FakeError;

        async fn run(
            &self,
            project: &OpenedProject,
            layout_profile: &MzWriteBackLayoutProfile,
        ) -> Result<StandardWriteBackReport, Self::Error> {
            self.events
                .lock()
                .expect("事件锁不应中毒")
                .push(Event::Standard(*layout_profile));
            if self.fail {
                Err(FakeError("standard"))
            } else {
                Ok(standard_report(project))
            }
        }
    }

    #[derive(Clone)]
    struct FakeLuaWriteBack {
        events: Arc<Mutex<Vec<Event>>>,
        fail: bool,
    }

    impl LuaWriteBack for FakeLuaWriteBack {
        type Error = FakeError;

        async fn run(
            &self,
            _: &OpenedProject,
            published: &PublishedWriteBack,
            script_path: PathBuf,
        ) -> Result<(), Self::Error> {
            self.events
                .lock()
                .expect("事件锁不应中毒")
                .push(Event::Lua {
                    script_path,
                    output_root: published.output_root().to_path_buf(),
                });
            if self.fail {
                Err(FakeError("lua"))
            } else {
                Ok(())
            }
        }
    }

    type Service = WriteBackService<FakeOpener, FakeStandardWriteBack, FakeLuaWriteBack>;

    fn service(events: Arc<Mutex<Vec<Event>>>, failing_stage: Option<&str>) -> Service {
        WriteBackService::new(
            FakeOpener {
                events: Arc::clone(&events),
                fail: failing_stage == Some("open"),
            },
            FakeStandardWriteBack {
                events: Arc::clone(&events),
                fail: failing_stage == Some("standard"),
            },
            Some(FakeLuaWriteBack {
                events,
                fail: failing_stage == Some("lua"),
            }),
            CooperativeCancellation::default(),
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

    fn standard_summary() -> StandardWriteBackSummary {
        StandardWriteBackSummary {
            translated_locations: 31,
            original_locations: 7,
            auto_wrapped_units: 5,
            inserted_line_breaks: 8,
            inserted_fullwidth_indents: 4,
            manual_layout_units: 2,
        }
    }

    fn standard_report(project: &OpenedProject) -> StandardWriteBackReport {
        StandardWriteBackReport::new(PublishedWriteBack::new(project), standard_summary())
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
    async fn without_lua_returns_the_published_standard_result() {
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
            ]
        );
    }

    #[tokio::test]
    async fn manual_layout_diagnostics_do_not_block_lua() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let script_path = PathBuf::from("scripts/write_back.lua");

        let output = service(Arc::clone(&recorded), None)
            .execute(input(Some("scripts/write_back.lua")))
            .await
            .expect("人工布局项不应让写回失败");

        assert_eq!(output.standard.manual_layout_units, 2);
        assert!(output.lua_executed);
        assert_eq!(output.output_root, output_root());
        assert_eq!(
            events(&recorded),
            vec![
                Event::Open(project_name()),
                Event::Standard(layout_profile()),
                Event::Lua {
                    script_path,
                    output_root: output_root(),
                },
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
                "lua",
                vec![
                    Event::Open(project_name()),
                    Event::Standard(layout_profile()),
                    Event::Lua {
                        script_path: PathBuf::from("scripts/write_back.lua"),
                        output_root: output_root(),
                    },
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
                "lua" => assert!(matches!(
                    error,
                    WriteBackServiceError::Lua {
                        source: FakeError("lua"),
                        ..
                    }
                )),
                _ => unreachable!("测试只包含已知阶段"),
            }
            assert_eq!(events(&recorded), expected_events);
        }
    }

    #[tokio::test]
    async fn lua_failure_keeps_script_published_output_and_source() {
        let recorded = Arc::new(Mutex::new(Vec::new()));

        let error = service(recorded, Some("lua"))
            .execute(input(Some("custom/write_back.lua")))
            .await
            .expect_err("Lua 技术失败应返回错误");

        assert!(matches!(
            &error,
            WriteBackServiceError::Lua {
                script_path,
                output_root: published_output,
                source: FakeError("lua"),
            } if script_path == &PathBuf::from("custom/write_back.lua")
                && published_output == &output_root()
        ));
        assert_eq!(
            error.source().and_then(|source| source.downcast_ref()),
            Some(&FakeError("lua"))
        );
        let message = error.to_string();
        assert!(message.contains("custom/write_back.lua"), "{message}");
        assert!(
            message.contains(&output_root().display().to_string()),
            "{message}"
        );
    }

    #[test]
    fn execution_future_is_send() {
        fn assert_send(_: impl Send) {}

        let service = service(Arc::new(Mutex::new(Vec::new())), None);
        assert_send(service.execute(input(None)));
    }
}
