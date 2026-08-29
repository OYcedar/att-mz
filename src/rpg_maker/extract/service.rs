use std::error::Error;
use std::fmt;
use std::path::PathBuf;

use super::builtin::BuiltInExtraction;
use super::rules::RulesExtraction;
use super::{ExtractOutput, ExtractProgress, ExtractProgressPhase, SelectedRules};
use crate::execution::{CooperativeCancellation, OperationCompletion};
use crate::progress::ProgressObserver;
use crate::rpg_maker::asset::RpgMakerAssetOwner;
use crate::rpg_maker::project::OpenedProject;

/// 按固定业务顺序编排一次 RPG Maker 文本提取。
///
/// Application 交入已经打开且持有排他租约的项目，随后按 Builtin、Rules 执行被选择的阶段。首个失败会
/// 阻止后续阶段，已经成功提交的前序阶段不由本层做组合回滚。
pub(crate) struct ExtractService<B, R> {
    built_in_extraction: Option<B>,
    selected_rules: Option<SelectedRules<R>>,
    cancellation: CooperativeCancellation,
    progress: ExtractProgress,
}

impl<B, R> ExtractService<B, R> {
    pub(crate) fn new(
        built_in_extraction: Option<B>,
        selected_rules: Option<SelectedRules<R>>,
        cancellation: CooperativeCancellation,
    ) -> Self {
        Self {
            built_in_extraction,
            selected_rules,
            cancellation,
            progress: ExtractProgress::default(),
        }
    }

    /// 为本次 Extract 绑定同步、不可失败的业务进度观察者。
    pub(crate) fn with_progress<Q>(mut self, progress: Q) -> Self
    where
        Q: ProgressObserver<ExtractProgressPhase> + 'static,
    {
        self.progress = ExtractProgress::new(progress);
        self
    }

    fn observe_owner(&self, phase: ExtractProgressPhase, completed: u64, total: u64) {
        self.progress.determinate(phase, completed, total);
    }
}

impl<B, R> ExtractService<B, R>
where
    B: BuiltInExtraction,
    R: RulesExtraction,
{
    pub(crate) async fn execute(
        &self,
        project: &OpenedProject,
    ) -> Result<OperationCompletion<ExtractOutput>, ExtractServiceError<B::Error, R::Error>> {
        if self.cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }

        let mut committed_owners = Vec::new();
        let mut rules_warnings = Vec::new();

        if let Some(built_in_extraction) = &self.built_in_extraction {
            // Builtin 与 Rules 是两个独立的生命周期阶段。不能用跨 owner 的累计
            // 分母，否则 Builtin 的完成快照会被误解为 Rules 尚未开始。
            self.observe_owner(ExtractProgressPhase::Builtin, 0, 1);
            built_in_extraction
                .refresh(project, self.progress.clone())
                .await
                .map_err(ExtractServiceError::BuiltIn)?;
            committed_owners.push(RpgMakerAssetOwner::Builtin);
            self.observe_owner(ExtractProgressPhase::Builtin, 1, 1);
            if self.selected_rules.is_some() && self.cancellation.is_requested() {
                return Ok(OperationCompletion::Cancelled);
            }
        }

        if let Some(selected_rules) = &self.selected_rules {
            self.observe_owner(ExtractProgressPhase::Rules, 0, 1);
            let error_path = selected_rules.program().diagnostic_path().to_path_buf();
            let rules_output = selected_rules
                .executor()
                .replace(
                    project,
                    selected_rules.program().clone(),
                    self.progress.clone(),
                )
                .await
                .map_err(|source| ExtractServiceError::Rules {
                    rules_path: error_path,
                    completed_owners: committed_owners.clone(),
                    source,
                })?;
            rules_warnings = rules_output.warnings;
            self.observe_owner(ExtractProgressPhase::Rules, 1, 1);
        }

        Ok(OperationCompletion::Completed(ExtractOutput {
            name: project.name().clone(),
            rules_warnings,
        }))
    }
}

/// 提取用例在直接依赖边界上遇到的阶段失败。
#[derive(Debug)]
pub(crate) enum ExtractServiceError<BE, RE> {
    BuiltIn(BE),
    Rules {
        rules_path: PathBuf,
        /// Rules 失败前已经各自成功提交、不会被组合回滚的 owner。
        completed_owners: Vec<RpgMakerAssetOwner>,
        source: RE,
    },
}

impl<BE, RE> fmt::Display for ExtractServiceError<BE, RE>
where
    BE: Error,
    RE: Error,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BuiltIn(source) => write!(formatter, "内置提取失败：{source}"),
            Self::Rules {
                rules_path,
                completed_owners,
                source,
            } => {
                write!(formatter, "规则提取失败 {}：{source}", rules_path.display())?;
                if !completed_owners.is_empty() {
                    formatter.write_str("；失败前已经提交：")?;
                    for (index, owner) in completed_owners.iter().enumerate() {
                        if index != 0 {
                            formatter.write_str("、")?;
                        }
                        formatter.write_str(owner.storage_name())?;
                    }
                }
                Ok(())
            }
        }
    }
}

impl<BE, RE> Error for ExtractServiceError<BE, RE>
where
    BE: Error + 'static,
    RE: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::BuiltIn(source) => Some(source),
            Self::Rules { source, .. } => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::progress::{ProgressAmount, ProgressSnapshot};
    use crate::rpg_maker::extract::builtin::BuiltInExtraction;
    use crate::rpg_maker::extract::rules::{RulesExtraction, RulesExtractionOutput, RulesProgram};
    use crate::rpg_maker::project::OpenedProject;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakeError;

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("fake error")
        }
    }

    impl Error for FakeError {}

    #[derive(Clone, Copy)]
    struct FakeBuiltIn;

    impl BuiltInExtraction for FakeBuiltIn {
        type Error = FakeError;

        async fn refresh(
            &self,
            _project: &OpenedProject,
            progress: ExtractProgress,
        ) -> Result<(), Self::Error> {
            progress.determinate(ExtractProgressPhase::BuiltinDocuments, 0, 0);
            progress.indeterminate(ExtractProgressPhase::BuiltinCommit);
            progress.determinate(ExtractProgressPhase::BuiltinCommit, 1, 1);
            Ok(())
        }
    }

    #[derive(Clone, Copy)]
    struct FakeRules;

    impl RulesExtraction for FakeRules {
        type Error = FakeError;

        async fn replace(
            &self,
            _project: &OpenedProject,
            _program: RulesProgram,
            progress: ExtractProgress,
        ) -> Result<RulesExtractionOutput, Self::Error> {
            progress.determinate(ExtractProgressPhase::RulesMatches, 0, 0);
            progress.indeterminate(ExtractProgressPhase::RulesCommit);
            progress.determinate(ExtractProgressPhase::RulesCommit, 1, 1);
            Ok(RulesExtractionOutput::default())
        }
    }

    #[derive(Clone, Default)]
    struct RecordingProgress {
        snapshots: Arc<Mutex<Vec<ProgressSnapshot<ExtractProgressPhase>>>>,
    }

    impl RecordingProgress {
        fn snapshots(&self) -> Vec<ProgressSnapshot<ExtractProgressPhase>> {
            self.snapshots.lock().expect("进度记录锁不应中毒").clone()
        }
    }

    impl ProgressObserver<ExtractProgressPhase> for RecordingProgress {
        fn observe(&self, snapshot: ProgressSnapshot<ExtractProgressPhase>) {
            self.snapshots
                .lock()
                .expect("进度记录锁不应中毒")
                .push(snapshot);
        }
    }

    #[tokio::test]
    async fn builtin_and_rules_owner_phases_each_complete_before_the_next_phase_starts() {
        let project = OpenedProject::new(
            "demo".parse().expect("项目名应合法"),
            PathBuf::from("C:/projects/demo"),
            PathBuf::from("C:/projects/demo/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
        );
        let progress = RecordingProgress::default();
        let rules = RulesProgram::from_toml(PathBuf::from("rules.toml"), b"rule = []".to_vec())
            .expect("测试 Rules 应合法");
        let extract = ExtractService::new(
            Some(FakeBuiltIn),
            Some(SelectedRules::new(rules, FakeRules)),
            CooperativeCancellation::default(),
        )
        .with_progress(progress.clone());

        let completion = extract.execute(&project).await.expect("Extract 应成功");
        assert!(matches!(completion, OperationCompletion::Completed(_)));

        let lifecycle_phases = progress
            .snapshots()
            .into_iter()
            .filter(|snapshot| {
                matches!(
                    snapshot.phase,
                    ExtractProgressPhase::Builtin
                        | ExtractProgressPhase::BuiltinCommit
                        | ExtractProgressPhase::Rules
                        | ExtractProgressPhase::RulesCommit
                )
            })
            .map(|snapshot| (snapshot.phase, snapshot.amount))
            .collect::<Vec<_>>();
        assert_eq!(
            lifecycle_phases,
            [
                (
                    ExtractProgressPhase::Builtin,
                    ProgressAmount::Determinate {
                        completed: 0,
                        total: 1,
                    },
                ),
                (
                    ExtractProgressPhase::BuiltinCommit,
                    ProgressAmount::Indeterminate,
                ),
                (
                    ExtractProgressPhase::BuiltinCommit,
                    ProgressAmount::Determinate {
                        completed: 1,
                        total: 1,
                    },
                ),
                (
                    ExtractProgressPhase::Builtin,
                    ProgressAmount::Determinate {
                        completed: 1,
                        total: 1,
                    },
                ),
                (
                    ExtractProgressPhase::Rules,
                    ProgressAmount::Determinate {
                        completed: 0,
                        total: 1,
                    },
                ),
                (
                    ExtractProgressPhase::RulesCommit,
                    ProgressAmount::Indeterminate,
                ),
                (
                    ExtractProgressPhase::RulesCommit,
                    ProgressAmount::Determinate {
                        completed: 1,
                        total: 1,
                    },
                ),
                (
                    ExtractProgressPhase::Rules,
                    ProgressAmount::Determinate {
                        completed: 1,
                        total: 1,
                    },
                ),
            ],
            "commit 必须在所属 owner 完成且下一 owner 开始前显式完成"
        );
    }
}
