//! 与具体翻译引擎无关的完整 TaskBlock 装箱和块内临时编号。
//!
//! 本模块只读取 Extract 已经建立的 Semantic Scope、Group、Unit 数量与稳定源文字符数。
//! 译文状态、语言判断、Placeholder 结果、术语、去重结果和任务历史都不进入装箱输入。

use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;
use std::ops::Range;

use crate::execution::CooperativeCancellation;

/// 一个 Group 作为块内首组或后续组时的稳定源文投影字符数。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct StableGroupCharacters {
    first_in_block: usize,
    following_in_block: usize,
}

impl StableGroupCharacters {
    pub(crate) const fn new(first_in_block: usize, following_in_block: usize) -> Self {
        Self {
            first_in_block,
            following_in_block,
        }
    }

    pub(crate) const fn first_in_block(self) -> usize {
        self.first_in_block
    }

    pub(crate) const fn following_in_block(self) -> usize {
        self.following_in_block
    }
}

/// Task planning 中一个不可拆分 Group 的结构投影。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TaskPlanningGroupLayout {
    unit_count: NonZeroUsize,
    stable_characters: StableGroupCharacters,
}

impl TaskPlanningGroupLayout {
    pub(crate) fn new(
        unit_count: usize,
        stable_characters: StableGroupCharacters,
    ) -> Result<Self, TaskPlanningError> {
        let Some(unit_count) = NonZeroUsize::new(unit_count) else {
            return Err(TaskPlanningError::EmptyGroup);
        };
        Ok(Self {
            unit_count,
            stable_characters,
        })
    }

    pub(crate) const fn unit_count(self) -> NonZeroUsize {
        self.unit_count
    }

    pub(crate) const fn stable_characters(self) -> StableGroupCharacters {
        self.stable_characters
    }
}

/// Task planning 中一个不得跨越的有序 Semantic Scope。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TaskPlanningScopeLayout {
    groups: Vec<TaskPlanningGroupLayout>,
}

impl TaskPlanningScopeLayout {
    pub(crate) fn new(groups: Vec<TaskPlanningGroupLayout>) -> Result<Self, TaskPlanningError> {
        if groups.is_empty() {
            return Err(TaskPlanningError::EmptyScope);
        }
        Ok(Self { groups })
    }

    pub(crate) fn groups(&self) -> &[TaskPlanningGroupLayout] {
        &self.groups
    }
}

/// 全部 Scope、Group、Unit 的有序结构，不保存正文或翻译状态。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TaskPlanningLayout {
    scopes: Vec<TaskPlanningScopeLayout>,
    total_units: usize,
}

impl TaskPlanningLayout {
    /// 建立完整语料结构。空语料合法，但已有 Scope 和 Group 都必须非空。
    pub(crate) fn new(scopes: Vec<TaskPlanningScopeLayout>) -> Result<Self, TaskPlanningError> {
        let mut total_units = 0_usize;
        for scope in &scopes {
            if scope.groups().is_empty() {
                return Err(TaskPlanningError::EmptyScope);
            }
            for group in scope.groups() {
                total_units = total_units
                    .checked_add(group.unit_count().get())
                    .ok_or(TaskPlanningError::UnitCountOverflow)?;
            }
        }
        Ok(Self {
            scopes,
            total_units,
        })
    }

    pub(crate) fn scopes(&self) -> &[TaskPlanningScopeLayout] {
        &self.scopes
    }

    pub(crate) const fn total_units(&self) -> usize {
        self.total_units
    }
}

/// 一个稳定 TaskBlock 在完整语料结构中的连续范围。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TaskBlockLayout {
    scope_index: usize,
    group_range: Range<usize>,
    unit_range: Range<usize>,
}

impl TaskBlockLayout {
    pub(crate) const fn scope_index(&self) -> usize {
        self.scope_index
    }

    /// 返回当前 Scope 内的连续 Group 范围。
    pub(crate) fn group_range(&self) -> Range<usize> {
        self.group_range.clone()
    }

    /// 返回全部语料按自然顺序展平后的连续 Unit 范围。
    pub(crate) fn unit_range(&self) -> Range<usize> {
        self.unit_range.clone()
    }
}

/// 与翻译状态无关的完整、稳定 TaskBlock 规划结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CompleteTaskPlan {
    blocks: Vec<TaskBlockLayout>,
    total_units: usize,
}

impl CompleteTaskPlan {
    #[cfg(test)]
    pub(crate) fn blocks(&self) -> &[TaskBlockLayout] {
        &self.blocks
    }

    pub(crate) const fn total_units(&self) -> usize {
        self.total_units
    }
}

/// 一个完整 Unit 在本轮模型请求中的责任。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UnitTaskResponsibility {
    /// 本轮由模型生成译文并接受一个块内临时 ID。
    ModelRepresentative,
    /// 只作为完整 TaskBlock 中的无编号语境。
    Context,
}

/// 实际请求中从 `0` 开始连续分配的块内临时 ID。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct TaskId(usize);

impl TaskId {
    pub(crate) const fn new(value: usize) -> Self {
        Self(value)
    }

    pub(crate) const fn get(self) -> usize {
        self.0
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.get(), formatter)
    }
}

/// 完整 TaskBlock 及其中每个 Unit 的本轮临时 ID。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AssignedTaskBlock {
    layout: TaskBlockLayout,
    unit_task_ids: Vec<Option<TaskId>>,
}

impl AssignedTaskBlock {
    pub(crate) const fn layout(&self) -> &TaskBlockLayout {
        &self.layout
    }

    pub(crate) fn unit_task_ids(&self) -> &[Option<TaskId>] {
        &self.unit_task_ids
    }

    pub(crate) fn has_task_ids(&self) -> bool {
        self.unit_task_ids.iter().any(Option::is_some)
    }
}

/// 保留全部稳定 TaskBlock 以及每个完整 Unit 的可选临时 ID。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AssignedTaskPlan {
    blocks: Vec<AssignedTaskBlock>,
}

impl AssignedTaskPlan {
    #[cfg(test)]
    pub(crate) fn blocks(&self) -> &[AssignedTaskBlock] {
        &self.blocks
    }

    /// 只读过滤出本轮至少含一个 Task ID 的块，不复制、合并或重新装箱。
    pub(crate) fn blocks_with_task_ids(
        &self,
    ) -> impl DoubleEndedIterator<Item = &AssignedTaskBlock> {
        self.blocks.iter().filter(|block| block.has_task_ids())
    }
}

/// 共享装箱或临时编号不能建立完整结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TaskPlanningError {
    Cancelled,
    EmptyScope,
    EmptyGroup,
    UnitCountOverflow,
    CharacterCountOverflow,
    ResponsibilityCountMismatch { expected: usize, actual: usize },
    TaskIdOverflow,
}

impl TaskPlanningError {
    pub(crate) const fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }
}

impl fmt::Display for TaskPlanningError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("TaskBlock 规划已取消"),
            Self::EmptyScope => formatter.write_str("TaskBlock 规划不能接受空 Semantic Scope"),
            Self::EmptyGroup => formatter.write_str("TaskBlock 规划不能接受空 Group"),
            Self::UnitCountOverflow => formatter.write_str("完整语料的 Unit 数量溢出"),
            Self::CharacterCountOverflow => {
                formatter.write_str("TaskBlock 稳定源文投影的字符数溢出")
            }
            Self::ResponsibilityCountMismatch { expected, actual } => write!(
                formatter,
                "Unit 责任数量与完整语料不一致：应为 {expected}，实际为 {actual}"
            ),
            Self::TaskIdOverflow => formatter.write_str("TaskBlock 块内临时 ID 溢出"),
        }
    }
}

impl Error for TaskPlanningError {}

/// 仅按完整结构和稳定源文字符数装箱，不读取任何翻译状态。
pub(crate) fn pack_complete_task_blocks(
    layout: &TaskPlanningLayout,
    target_characters: NonZeroUsize,
    cancellation: &CooperativeCancellation,
) -> Result<CompleteTaskPlan, TaskPlanningError> {
    ensure_running(cancellation)?;
    let mut blocks = Vec::new();
    let mut next_unit_index = 0_usize;

    for (scope_index, scope) in layout.scopes().iter().enumerate() {
        ensure_running(cancellation)?;
        if scope.groups().is_empty() {
            return Err(TaskPlanningError::EmptyScope);
        }

        let mut block_start_group = 0_usize;
        let mut block_start_unit = next_unit_index;
        let mut block_characters = None;

        for (group_index, group) in scope.groups().iter().copied().enumerate() {
            ensure_running(cancellation)?;
            let group_end_unit = next_unit_index
                .checked_add(group.unit_count().get())
                .ok_or(TaskPlanningError::UnitCountOverflow)?;
            let stable_characters = group.stable_characters();

            match block_characters {
                None => {
                    block_start_group = group_index;
                    block_start_unit = next_unit_index;
                    block_characters = Some(stable_characters.first_in_block());
                }
                Some(current_characters) => {
                    let candidate_characters = if current_characters > target_characters.get() {
                        None
                    } else {
                        Some(
                            current_characters
                                .checked_add(stable_characters.following_in_block())
                                .ok_or(TaskPlanningError::CharacterCountOverflow)?,
                        )
                    };
                    if candidate_characters
                        .is_some_and(|characters| characters <= target_characters.get())
                    {
                        block_characters = candidate_characters;
                    } else {
                        blocks.push(TaskBlockLayout {
                            scope_index,
                            group_range: block_start_group..group_index,
                            unit_range: block_start_unit..next_unit_index,
                        });
                        block_start_group = group_index;
                        block_start_unit = next_unit_index;
                        block_characters = Some(stable_characters.first_in_block());
                    }
                }
            }
            next_unit_index = group_end_unit;
        }

        debug_assert!(
            block_characters.is_some(),
            "Semantic Scope 已由构造器证明非空"
        );
        blocks.push(TaskBlockLayout {
            scope_index,
            group_range: block_start_group..scope.groups().len(),
            unit_range: block_start_unit..next_unit_index,
        });
    }

    ensure_running(cancellation)?;
    if next_unit_index != layout.total_units() {
        return Err(TaskPlanningError::UnitCountOverflow);
    }
    Ok(CompleteTaskPlan {
        blocks,
        total_units: layout.total_units(),
    })
}

/// 在完整 TaskBlock 已经确定后，只给本轮模型代表分配块内临时 ID。
pub(crate) fn assign_task_ids(
    complete_plan: CompleteTaskPlan,
    responsibilities: &[UnitTaskResponsibility],
    cancellation: &CooperativeCancellation,
) -> Result<AssignedTaskPlan, TaskPlanningError> {
    ensure_running(cancellation)?;
    if responsibilities.len() != complete_plan.total_units() {
        return Err(TaskPlanningError::ResponsibilityCountMismatch {
            expected: complete_plan.total_units(),
            actual: responsibilities.len(),
        });
    }

    let mut blocks = Vec::with_capacity(complete_plan.blocks.len());
    for layout in complete_plan.blocks {
        ensure_running(cancellation)?;
        let responsibilities = &responsibilities[layout.unit_range.clone()];
        let mut unit_task_ids = Vec::with_capacity(responsibilities.len());
        let mut assigned_count = 0_usize;
        for responsibility in responsibilities {
            ensure_running(cancellation)?;
            let task_id = match responsibility {
                UnitTaskResponsibility::Context => None,
                UnitTaskResponsibility::ModelRepresentative => {
                    let task_id = TaskId::new(assigned_count);
                    assigned_count = assigned_count
                        .checked_add(1)
                        .ok_or(TaskPlanningError::TaskIdOverflow)?;
                    Some(task_id)
                }
            };
            unit_task_ids.push(task_id);
        }
        blocks.push(AssignedTaskBlock {
            layout,
            unit_task_ids,
        });
    }
    ensure_running(cancellation)?;

    Ok(AssignedTaskPlan { blocks })
}

fn ensure_running(cancellation: &CooperativeCancellation) -> Result<(), TaskPlanningError> {
    if cancellation.is_requested() {
        Err(TaskPlanningError::Cancelled)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(
        unit_count: usize,
        first_in_block: usize,
        following_in_block: usize,
    ) -> TaskPlanningGroupLayout {
        TaskPlanningGroupLayout::new(
            unit_count,
            StableGroupCharacters::new(first_in_block, following_in_block),
        )
        .expect("测试 Group 必须非空")
    }

    fn scope(groups: Vec<TaskPlanningGroupLayout>) -> TaskPlanningScopeLayout {
        TaskPlanningScopeLayout::new(groups).expect("测试 Scope 必须非空")
    }

    fn active() -> UnitTaskResponsibility {
        UnitTaskResponsibility::ModelRepresentative
    }

    fn context() -> UnitTaskResponsibility {
        UnitTaskResponsibility::Context
    }

    #[test]
    fn exact_target_stays_in_one_block() {
        let layout = TaskPlanningLayout::new(vec![scope(vec![group(1, 6, 4), group(2, 7, 4)])])
            .expect("layout 应合法");

        let plan = pack_complete_task_blocks(
            &layout,
            NonZeroUsize::new(10).expect("目标非零"),
            &CooperativeCancellation::default(),
        )
        .expect("精确达到目标应装入同一块");

        assert_eq!(plan.blocks.len(), 1);
        assert_eq!(plan.blocks[0].scope_index(), 0);
        assert_eq!(plan.blocks[0].group_range(), 0..2);
        assert_eq!(plan.blocks[0].unit_range(), 0..3);
    }

    #[test]
    fn oversized_group_is_kept_whole_and_alone() {
        let layout = TaskPlanningLayout::new(vec![scope(vec![group(2, 11, 8), group(1, 4, 3)])])
            .expect("layout 应合法");

        let plan = pack_complete_task_blocks(
            &layout,
            NonZeroUsize::new(10).expect("目标非零"),
            &CooperativeCancellation::default(),
        )
        .expect("超大 Group 应独占块");

        assert_eq!(plan.blocks.len(), 2);
        assert_eq!(plan.blocks[0].group_range(), 0..1);
        assert_eq!(plan.blocks[0].unit_range(), 0..2);
        assert_eq!(plan.blocks[1].group_range(), 1..2);
        assert_eq!(plan.blocks[1].unit_range(), 2..3);
    }

    #[test]
    fn blocks_never_cross_scope_boundaries() {
        let layout = TaskPlanningLayout::new(vec![
            scope(vec![group(1, 1, 1)]),
            scope(vec![group(1, 1, 1)]),
        ])
        .expect("layout 应合法");

        let plan = pack_complete_task_blocks(
            &layout,
            NonZeroUsize::new(100).expect("目标非零"),
            &CooperativeCancellation::default(),
        )
        .expect("Scope 边界应强制分块");

        assert_eq!(plan.blocks.len(), 2);
        assert_eq!(plan.blocks[0].scope_index(), 0);
        assert_eq!(plan.blocks[0].unit_range(), 0..1);
        assert_eq!(plan.blocks[1].scope_index(), 1);
        assert_eq!(plan.blocks[1].unit_range(), 1..2);
    }

    #[test]
    fn zero_id_middle_block_is_only_filtered_and_never_merged() {
        let layout = TaskPlanningLayout::new(vec![scope(vec![
            group(1, 6, 6),
            group(1, 6, 6),
            group(1, 6, 6),
        ])])
        .expect("layout 应合法");
        let complete = pack_complete_task_blocks(
            &layout,
            NonZeroUsize::new(6).expect("目标非零"),
            &CooperativeCancellation::default(),
        )
        .expect("应形成三个稳定块");
        let assigned = assign_task_ids(
            complete,
            &[active(), context(), active()],
            &CooperativeCancellation::default(),
        )
        .expect("责任数量应匹配");

        assert_eq!(assigned.blocks().len(), 3);
        let sent = assigned.blocks_with_task_ids().collect::<Vec<_>>();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].layout().group_range(), 0..1);
        assert_eq!(sent[1].layout().group_range(), 2..3);
        assert_eq!(sent[0].unit_task_ids(), [Some(TaskId::new(0))]);
        assert_eq!(sent[1].unit_task_ids(), [Some(TaskId::new(0))]);
    }

    #[test]
    fn responsibility_count_must_match_every_complete_unit() {
        let layout =
            TaskPlanningLayout::new(vec![scope(vec![group(2, 1, 1)])]).expect("layout 应合法");
        let complete = pack_complete_task_blocks(
            &layout,
            NonZeroUsize::new(10).expect("目标非零"),
            &CooperativeCancellation::default(),
        )
        .expect("装箱应完成");

        assert_eq!(
            assign_task_ids(complete, &[active()], &CooperativeCancellation::default()),
            Err(TaskPlanningError::ResponsibilityCountMismatch {
                expected: 2,
                actual: 1,
            })
        );
    }

    #[test]
    fn cancellation_is_an_explicit_planning_result() {
        let layout =
            TaskPlanningLayout::new(vec![scope(vec![group(1, 1, 1)])]).expect("layout 应合法");
        let cancellation = CooperativeCancellation::default();
        cancellation.request();
        assert_eq!(
            pack_complete_task_blocks(
                &layout,
                NonZeroUsize::new(10).expect("目标非零"),
                &cancellation
            ),
            Err(TaskPlanningError::Cancelled)
        );

        let complete = CompleteTaskPlan {
            blocks: vec![TaskBlockLayout {
                scope_index: 0,
                group_range: 0..1,
                unit_range: 0..1,
            }],
            total_units: 1,
        };
        assert_eq!(
            assign_task_ids(complete, &[active()], &cancellation),
            Err(TaskPlanningError::Cancelled)
        );
    }

    #[test]
    fn repeated_planning_is_idempotent() {
        let layout = TaskPlanningLayout::new(vec![scope(vec![
            group(1, 4, 3),
            group(2, 4, 3),
            group(1, 4, 3),
        ])])
        .expect("layout 应合法");
        let target = NonZeroUsize::new(7).expect("目标非零");

        let first = pack_complete_task_blocks(&layout, target, &CooperativeCancellation::default())
            .expect("第一次装箱应完成");
        let second =
            pack_complete_task_blocks(&layout, target, &CooperativeCancellation::default())
                .expect("第二次装箱应完成");

        assert_eq!(first, second);
    }

    #[test]
    fn translation_states_only_change_ids_and_sent_block_set() {
        let layout = TaskPlanningLayout::new(vec![scope(vec![group(2, 6, 4), group(2, 6, 4)])])
            .expect("layout 应合法");
        let target = NonZeroUsize::new(6).expect("目标非零");
        let expected_complete =
            pack_complete_task_blocks(&layout, target, &CooperativeCancellation::default())
                .expect("基准装箱应完成");
        let cases = [
            ("全部待译", vec![active(), active(), active(), active()], 2),
            (
                "只有 B 待译",
                vec![context(), active(), context(), context()],
                1,
            ),
            (
                "部分 Current",
                vec![context(), active(), active(), context()],
                2,
            ),
            ("复用", vec![context(), active(), context(), active()], 2),
            (
                "跨块去重",
                vec![active(), context(), context(), context()],
                1,
            ),
            ("非源语", vec![context(), context(), active(), context()], 1),
            (
                "完全保护",
                vec![context(), context(), context(), context()],
                0,
            ),
            (
                "全部 Current",
                vec![context(), context(), context(), context()],
                0,
            ),
        ];

        for (name, responsibilities, sent_count) in cases {
            let complete =
                pack_complete_task_blocks(&layout, target, &CooperativeCancellation::default())
                    .unwrap_or_else(|error| panic!("{name} 的完整装箱失败：{error}"));
            assert_eq!(complete, expected_complete, "{name} 改变了完整块范围");
            let assigned = assign_task_ids(
                complete,
                &responsibilities,
                &CooperativeCancellation::default(),
            )
            .unwrap_or_else(|error| panic!("{name} 的编号失败：{error}"));
            assert_eq!(
                assigned.blocks().len(),
                expected_complete.blocks().len(),
                "{name} 删除了完整块"
            );
            for (assigned_block, complete_block) in
                assigned.blocks().iter().zip(expected_complete.blocks())
            {
                assert_eq!(
                    assigned_block.layout(),
                    complete_block,
                    "{name} 改变了完整块边界"
                );
            }
            assert_eq!(
                assigned.blocks_with_task_ids().count(),
                sent_count,
                "{name} 的实际发送块集合不正确"
            );
        }
    }

    #[test]
    fn empty_corpus_is_valid_but_empty_scope_or_group_is_not() {
        let layout = TaskPlanningLayout::new(Vec::new()).expect("空语料合法");
        let complete = pack_complete_task_blocks(
            &layout,
            NonZeroUsize::new(1).expect("目标非零"),
            &CooperativeCancellation::default(),
        )
        .expect("空语料应得到空规划");
        assert!(complete.blocks().is_empty());
        assert_eq!(complete.total_units(), 0);
        let assigned = assign_task_ids(complete, &[], &CooperativeCancellation::default())
            .expect("空语料的空责任投影应合法");
        assert!(assigned.blocks().is_empty());
        assert_eq!(assigned.blocks_with_task_ids().count(), 0);

        assert_eq!(
            TaskPlanningScopeLayout::new(Vec::new()),
            Err(TaskPlanningError::EmptyScope)
        );
        assert_eq!(
            TaskPlanningGroupLayout::new(0, StableGroupCharacters::new(1, 1)),
            Err(TaskPlanningError::EmptyGroup)
        );
    }

    #[test]
    fn checked_arithmetic_reports_unit_and_character_overflow() {
        let unit_overflow =
            TaskPlanningLayout::new(vec![scope(vec![group(usize::MAX, 1, 1), group(1, 1, 1)])]);
        assert_eq!(unit_overflow, Err(TaskPlanningError::UnitCountOverflow));

        let layout =
            TaskPlanningLayout::new(vec![scope(vec![group(1, usize::MAX, 1), group(1, 1, 1)])])
                .expect("Unit 数量应合法");
        assert_eq!(
            pack_complete_task_blocks(
                &layout,
                NonZeroUsize::new(usize::MAX).expect("最大 usize 非零"),
                &CooperativeCancellation::default(),
            ),
            Err(TaskPlanningError::CharacterCountOverflow)
        );
    }

    #[test]
    fn task_ids_are_zero_based_and_restart_in_every_block() {
        assert_eq!(TaskId::new(0).to_string(), "0");
        assert_eq!(TaskId::new(7).to_string(), "7");

        let layout = TaskPlanningLayout::new(vec![scope(vec![group(2, 2, 2), group(2, 2, 2)])])
            .expect("layout 应合法");
        let complete = pack_complete_task_blocks(
            &layout,
            NonZeroUsize::new(2).expect("目标非零"),
            &CooperativeCancellation::default(),
        )
        .expect("应形成两个块");
        let assigned = assign_task_ids(
            complete,
            &[active(), active(), context(), active()],
            &CooperativeCancellation::default(),
        )
        .expect("编号应完成");
        assert_eq!(
            assigned.blocks()[0].unit_task_ids(),
            [Some(TaskId::new(0)), Some(TaskId::new(1))]
        );
        assert_eq!(
            assigned.blocks()[1].unit_task_ids(),
            [None, Some(TaskId::new(0))]
        );
    }
}
