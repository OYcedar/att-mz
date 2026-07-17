//! 从数据库译文规划并发布 Standard MZ 写回的业务编排。

mod layout;

pub(crate) use layout::ConservativeMzWriteBackTextLayouter;

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};

use super::{
    PublishedWriteBack, StandardWriteBack, StandardWriteBackReport, StandardWriteBackSummary,
};
use crate::att_mz::ProjectName;
use crate::att_mz::project::{MaxFullwidthChars, MzWriteBackLayoutProfile, OpenedProject};
use crate::att_mz::text::{MzLocation, MzLocationStep, MzSource, StandardDataFile, TextGroupKind};
use crate::execution::{CooperativeCancellation, OperationCancelled};
use crate::observability::PersistentEventLog;

/// 数据库资产叶子在 Standard 写回中的结构化角色。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StandardWriteBackFieldRole {
    /// 不属于对话或滚动正文的普通单值字段。
    Scalar { field_name: String },
    /// MZ 对话起始指令中的说话人字段。
    DialogueSpeaker,
    /// 一条原生 401 对话正文，索引来自同一语义组中的 `body[n]`。
    DialogueBody { index: usize },
    /// 一条原生 405 滚动正文，索引来自同一语义组中的 `body[n]`。
    ScrollingTextBody { index: usize },
}

impl StandardWriteBackFieldRole {
    pub(crate) fn scalar(field_name: impl Into<String>) -> Self {
        Self::Scalar {
            field_name: field_name.into(),
        }
    }

    pub(crate) const fn dialogue_speaker() -> Self {
        Self::DialogueSpeaker
    }

    pub(crate) const fn dialogue_body(index: usize) -> Self {
        Self::DialogueBody { index }
    }

    pub(crate) const fn scrolling_text_body(index: usize) -> Self {
        Self::ScrollingTextBody { index }
    }

    pub(crate) fn field_name(&self) -> Option<&str> {
        match self {
            Self::Scalar { field_name } => Some(field_name),
            Self::DialogueSpeaker => Some("speaker"),
            Self::DialogueBody { .. } | Self::ScrollingTextBody { .. } => None,
        }
    }
}

/// 五张标准资产表中一个可独立拥有译文的位置。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StandardWriteBackLeaf {
    role: StandardWriteBackFieldRole,
    exact_location: MzLocation,
    original_text: String,
    translation: Option<String>,
}

impl StandardWriteBackLeaf {
    pub(crate) fn new(
        role: StandardWriteBackFieldRole,
        exact_location: MzLocation,
        original_text: impl Into<String>,
        translation: Option<String>,
    ) -> Result<Self, StandardWriteBackSnapshotError> {
        if role
            .field_name()
            .is_some_and(|field_name| field_name.trim().is_empty())
        {
            return Err(StandardWriteBackSnapshotError::EmptyFieldName {
                exact_location: Box::new(exact_location),
            });
        }
        let original_text = original_text.into();
        if original_text.trim().is_empty() {
            return Err(StandardWriteBackSnapshotError::BlankOriginal {
                exact_location: Box::new(exact_location),
            });
        }
        if translation
            .as_deref()
            .is_some_and(|translation| translation.trim().is_empty())
        {
            return Err(StandardWriteBackSnapshotError::BlankTranslation {
                exact_location: Box::new(exact_location),
            });
        }
        Ok(Self {
            role,
            exact_location,
            original_text,
            translation,
        })
    }

    #[cfg(test)]
    pub(crate) fn role(&self) -> &StandardWriteBackFieldRole {
        &self.role
    }

    #[cfg(test)]
    pub(crate) fn exact_location(&self) -> &MzLocation {
        &self.exact_location
    }

    #[cfg(test)]
    pub(crate) fn original_text(&self) -> &str {
        &self.original_text
    }

    #[cfg(test)]
    pub(crate) fn translation(&self) -> Option<&str> {
        self.translation.as_deref()
    }
}

/// 一组必须共同保留 MZ 语义边界的数据库写回资产。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StandardWriteBackGroup {
    kind: TextGroupKind,
    group_location: MzLocation,
    leaves: Vec<StandardWriteBackLeaf>,
}

impl StandardWriteBackGroup {
    pub(crate) fn new(
        kind: TextGroupKind,
        group_location: MzLocation,
        mut leaves: Vec<StandardWriteBackLeaf>,
    ) -> Result<Self, StandardWriteBackSnapshotError> {
        if leaves.is_empty() {
            return Err(StandardWriteBackSnapshotError::EmptyGroup {
                group_location: Box::new(group_location),
            });
        }
        for leaf in &leaves {
            if leaf.exact_location.source() != group_location.source() {
                return Err(StandardWriteBackSnapshotError::MismatchedSource {
                    group_location: Box::new(group_location),
                    exact_location: Box::new(leaf.exact_location.clone()),
                });
            }
            if !role_matches_kind(kind, &leaf.role) {
                return Err(StandardWriteBackSnapshotError::InvalidRole {
                    kind,
                    exact_location: Box::new(leaf.exact_location.clone()),
                });
            }
        }

        match kind {
            TextGroupKind::EventDialogue => {
                validate_dialogue_roles(&group_location, &leaves)?;
                leaves.sort_by(compare_dialogue_leaves);
            }
            TextGroupKind::EventScrollingText => {
                validate_scrolling_roles(&group_location, &leaves)?;
                leaves.sort_by(compare_scrolling_leaves);
            }
            _ => leaves.sort_by(|left, right| left.exact_location.cmp(&right.exact_location)),
        }

        for pair in leaves.windows(2) {
            if pair[0].exact_location == pair[1].exact_location {
                return Err(StandardWriteBackSnapshotError::DuplicateLocation {
                    exact_location: Box::new(pair[0].exact_location.clone()),
                });
            }
        }

        Ok(Self {
            kind,
            group_location,
            leaves,
        })
    }

    #[cfg(test)]
    pub(crate) const fn kind(&self) -> TextGroupKind {
        self.kind
    }

    #[cfg(test)]
    pub(crate) fn group_location(&self) -> &MzLocation {
        &self.group_location
    }

    #[cfg(test)]
    pub(crate) fn leaves(&self) -> &[StandardWriteBackLeaf] {
        &self.leaves
    }

    fn into_parts(self) -> (TextGroupKind, MzLocation, Vec<StandardWriteBackLeaf>) {
        (self.kind, self.group_location, self.leaves)
    }
}

/// Reader 在同一个一致读视图中建立的完整 Standard 写回快照。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct StandardWriteBackSnapshot {
    groups: Vec<StandardWriteBackGroup>,
}

impl StandardWriteBackSnapshot {
    pub(crate) fn new(
        mut groups: Vec<StandardWriteBackGroup>,
    ) -> Result<Self, StandardWriteBackSnapshotError> {
        groups.sort_by(|left, right| {
            left.kind
                .cmp(&right.kind)
                .then_with(|| left.group_location.cmp(&right.group_location))
        });

        let mut group_keys = BTreeSet::new();
        let mut exact_locations = BTreeSet::new();
        for group in &groups {
            if !group_keys.insert((group.group_location.clone(), group.kind)) {
                return Err(StandardWriteBackSnapshotError::DuplicateGroup {
                    kind: group.kind,
                    group_location: Box::new(group.group_location.clone()),
                });
            }
            for leaf in &group.leaves {
                if !exact_locations.insert(leaf.exact_location.clone()) {
                    return Err(StandardWriteBackSnapshotError::DuplicateLocation {
                        exact_location: Box::new(leaf.exact_location.clone()),
                    });
                }
            }
        }
        Ok(Self { groups })
    }

    pub(crate) fn empty() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub(crate) fn groups(&self) -> &[StandardWriteBackGroup] {
        &self.groups
    }

    fn into_groups(self) -> Vec<StandardWriteBackGroup> {
        self.groups
    }
}

fn role_matches_kind(kind: TextGroupKind, role: &StandardWriteBackFieldRole) -> bool {
    match kind {
        TextGroupKind::EventDialogue => matches!(
            role,
            StandardWriteBackFieldRole::DialogueSpeaker
                | StandardWriteBackFieldRole::DialogueBody { .. }
        ),
        TextGroupKind::EventScrollingText => {
            matches!(role, StandardWriteBackFieldRole::ScrollingTextBody { .. })
        }
        _ => matches!(role, StandardWriteBackFieldRole::Scalar { .. }),
    }
}

fn validate_dialogue_roles(
    group_location: &MzLocation,
    leaves: &[StandardWriteBackLeaf],
) -> Result<(), StandardWriteBackSnapshotError> {
    let speaker_count = leaves
        .iter()
        .filter(|leaf| matches!(leaf.role, StandardWriteBackFieldRole::DialogueSpeaker))
        .count();
    if speaker_count > 1 {
        return Err(StandardWriteBackSnapshotError::DuplicateDialogueSpeaker {
            group_location: Box::new(group_location.clone()),
        });
    }
    validate_body_indices(
        group_location,
        leaves.iter().filter_map(|leaf| match leaf.role {
            StandardWriteBackFieldRole::DialogueBody { index } => Some(index),
            _ => None,
        }),
    )?;
    validate_body_location_order(
        group_location,
        leaves.iter().filter_map(|leaf| match leaf.role {
            StandardWriteBackFieldRole::DialogueBody { index } => {
                Some((index, &leaf.exact_location))
            }
            _ => None,
        }),
    )
}

fn validate_scrolling_roles(
    group_location: &MzLocation,
    leaves: &[StandardWriteBackLeaf],
) -> Result<(), StandardWriteBackSnapshotError> {
    validate_body_indices(
        group_location,
        leaves.iter().filter_map(|leaf| match leaf.role {
            StandardWriteBackFieldRole::ScrollingTextBody { index } => Some(index),
            _ => None,
        }),
    )?;
    validate_body_location_order(
        group_location,
        leaves.iter().filter_map(|leaf| match leaf.role {
            StandardWriteBackFieldRole::ScrollingTextBody { index } => {
                Some((index, &leaf.exact_location))
            }
            _ => None,
        }),
    )
}

fn validate_body_indices(
    group_location: &MzLocation,
    indices: impl IntoIterator<Item = usize>,
) -> Result<(), StandardWriteBackSnapshotError> {
    let mut indices = indices.into_iter().collect::<Vec<_>>();
    indices.sort_unstable();
    for pair in indices.windows(2) {
        if pair[0] == pair[1] {
            return Err(StandardWriteBackSnapshotError::DuplicateBodyIndex {
                group_location: Box::new(group_location.clone()),
                index: pair[0],
            });
        }
    }
    for (expected, actual) in indices.into_iter().enumerate() {
        if actual != expected {
            return Err(StandardWriteBackSnapshotError::NonContiguousBodyIndex {
                group_location: Box::new(group_location.clone()),
                expected,
                actual,
            });
        }
    }
    Ok(())
}

fn validate_body_location_order<'a>(
    group_location: &MzLocation,
    entries: impl IntoIterator<Item = (usize, &'a MzLocation)>,
) -> Result<(), StandardWriteBackSnapshotError> {
    let mut entries = entries.into_iter().collect::<Vec<_>>();
    entries.sort_by_key(|(index, _)| *index);
    for pair in entries.windows(2) {
        if pair[0].1 >= pair[1].1 {
            return Err(StandardWriteBackSnapshotError::BodyLocationOrder {
                group_location: Box::new(group_location.clone()),
                previous: Box::new(pair[0].1.clone()),
                next: Box::new(pair[1].1.clone()),
            });
        }
    }
    Ok(())
}

fn compare_dialogue_leaves(
    left: &StandardWriteBackLeaf,
    right: &StandardWriteBackLeaf,
) -> Ordering {
    dialogue_role_order(&left.role)
        .cmp(&dialogue_role_order(&right.role))
        .then_with(|| left.exact_location.cmp(&right.exact_location))
}

fn dialogue_role_order(role: &StandardWriteBackFieldRole) -> (u8, usize) {
    match role {
        StandardWriteBackFieldRole::DialogueSpeaker => (0, 0),
        StandardWriteBackFieldRole::DialogueBody { index } => (1, *index),
        _ => (2, 0),
    }
}

fn compare_scrolling_leaves(
    left: &StandardWriteBackLeaf,
    right: &StandardWriteBackLeaf,
) -> Ordering {
    scrolling_role_order(&left.role)
        .cmp(&scrolling_role_order(&right.role))
        .then_with(|| left.exact_location.cmp(&right.exact_location))
}

fn scrolling_role_order(role: &StandardWriteBackFieldRole) -> usize {
    match role {
        StandardWriteBackFieldRole::ScrollingTextBody { index } => *index,
        _ => usize::MAX,
    }
}

/// Reader 交回受信快照前必须排除的数据损坏。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StandardWriteBackSnapshotError {
    EmptyFieldName {
        exact_location: Box<MzLocation>,
    },
    BlankOriginal {
        exact_location: Box<MzLocation>,
    },
    BlankTranslation {
        exact_location: Box<MzLocation>,
    },
    EmptyGroup {
        group_location: Box<MzLocation>,
    },
    InvalidRole {
        kind: TextGroupKind,
        exact_location: Box<MzLocation>,
    },
    MismatchedSource {
        group_location: Box<MzLocation>,
        exact_location: Box<MzLocation>,
    },
    DuplicateDialogueSpeaker {
        group_location: Box<MzLocation>,
    },
    DuplicateBodyIndex {
        group_location: Box<MzLocation>,
        index: usize,
    },
    NonContiguousBodyIndex {
        group_location: Box<MzLocation>,
        expected: usize,
        actual: usize,
    },
    BodyLocationOrder {
        group_location: Box<MzLocation>,
        previous: Box<MzLocation>,
        next: Box<MzLocation>,
    },
    DuplicateLocation {
        exact_location: Box<MzLocation>,
    },
    DuplicateGroup {
        kind: TextGroupKind,
        group_location: Box<MzLocation>,
    },
}

impl fmt::Display for StandardWriteBackSnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyFieldName { exact_location } => {
                write!(formatter, "写回资产字段名为空：{exact_location}")
            }
            Self::BlankOriginal { exact_location } => {
                write!(formatter, "写回资产原文仅包含空白：{exact_location}")
            }
            Self::BlankTranslation { exact_location } => {
                write!(formatter, "写回资产译文仅包含空白：{exact_location}")
            }
            Self::EmptyGroup { group_location } => {
                write!(formatter, "写回资产组不包含叶子：{group_location}")
            }
            Self::InvalidRole {
                kind,
                exact_location,
            } => write!(
                formatter,
                "写回资产角色与组类型 {kind:?} 不一致：{exact_location}"
            ),
            Self::MismatchedSource {
                group_location,
                exact_location,
            } => write!(
                formatter,
                "写回资产组与叶子不属于同一来源：{group_location} / {exact_location}"
            ),
            Self::DuplicateDialogueSpeaker { group_location } => {
                write!(formatter, "对话组包含多个 speaker：{group_location}")
            }
            Self::DuplicateBodyIndex {
                group_location,
                index,
            } => write!(formatter, "正文组 {group_location} 重复 body[{index}]"),
            Self::NonContiguousBodyIndex {
                group_location,
                expected,
                actual,
            } => write!(
                formatter,
                "正文组 {group_location} 的索引不连续：期望 body[{expected}]，实际 body[{actual}]"
            ),
            Self::BodyLocationOrder {
                group_location,
                previous,
                next,
            } => write!(
                formatter,
                "正文组 {group_location} 的物理位置不按 body 索引递增：{previous} / {next}"
            ),
            Self::DuplicateLocation { exact_location } => {
                write!(formatter, "写回快照包含重复位置：{exact_location}")
            }
            Self::DuplicateGroup {
                kind,
                group_location,
            } => write!(formatter, "写回快照包含重复组 {kind:?}：{group_location}"),
        }
    }
}

impl Error for StandardWriteBackSnapshotError {}

/// 在同一个一致读视图中取得五张标准资产表的当前写回事实。
///
/// 实现不得读取或校验术语依赖；术语数据不是 WriteBack 的输入。
pub(crate) trait StandardWriteBackAssetReader: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn read(
        &self,
        project: &OpenedProject,
    ) -> impl Future<Output = Result<StandardWriteBackSnapshot, Self::Error>> + Send;
}

/// 当前允许自动布局的 MZ 显示区域。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MzWriteBackLayoutRegion {
    DialogueBody,
    ScrollingText,
    HelpDescription,
}

/// 一个布局段当前写回内容的权威来源。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum MzWriteBackLayoutCandidate {
    /// 数据库没有译文，必须保持冻结原命令或原字段不变。
    FrozenOriginal,
    /// 数据库明确存在译文，允许布局器调整显示行。
    DatabaseTranslation(String),
}

/// 布局请求中一个仍与数据库叶子保持对应关系的内容段。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MzWriteBackLayoutSegment {
    exact_location: MzLocation,
    original_text: String,
    candidate: MzWriteBackLayoutCandidate,
}

impl MzWriteBackLayoutSegment {
    fn from_leaf(leaf: &StandardWriteBackLeaf) -> Self {
        let candidate = leaf
            .translation
            .clone()
            .map_or(MzWriteBackLayoutCandidate::FrozenOriginal, |translation| {
                MzWriteBackLayoutCandidate::DatabaseTranslation(translation)
            });
        Self {
            exact_location: leaf.exact_location.clone(),
            original_text: leaf.original_text.clone(),
            candidate,
        }
    }

    pub(crate) fn exact_location(&self) -> &MzLocation {
        &self.exact_location
    }

    pub(crate) fn candidate(&self) -> &MzWriteBackLayoutCandidate {
        &self.candidate
    }

    pub(crate) fn effective_text(&self) -> &str {
        match &self.candidate {
            MzWriteBackLayoutCandidate::FrozenOriginal => &self.original_text,
            MzWriteBackLayoutCandidate::DatabaseTranslation(translation) => translation,
        }
    }
}

/// Standard 为一个完整布局单元建立的显式请求。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MzWriteBackLayoutRequest {
    unit_location: MzLocation,
    region: MzWriteBackLayoutRegion,
    max_fullwidth_chars: MaxFullwidthChars,
    auto_wrap_enabled: bool,
    segments: Vec<MzWriteBackLayoutSegment>,
}

impl MzWriteBackLayoutRequest {
    fn new(
        unit_location: MzLocation,
        region: MzWriteBackLayoutRegion,
        max_fullwidth_chars: MaxFullwidthChars,
        auto_wrap_enabled: bool,
        segments: Vec<MzWriteBackLayoutSegment>,
    ) -> Self {
        debug_assert!(!segments.is_empty(), "布局单元必须包含至少一个文本段");
        debug_assert!(
            segments.iter().any(|segment| matches!(
                segment.candidate,
                MzWriteBackLayoutCandidate::DatabaseTranslation(_)
            )),
            "没有数据库译文的单元不应请求布局"
        );
        Self {
            unit_location,
            region,
            max_fullwidth_chars,
            auto_wrap_enabled,
            segments,
        }
    }

    #[cfg(test)]
    pub(crate) fn unit_location(&self) -> &MzLocation {
        &self.unit_location
    }

    #[cfg(test)]
    pub(crate) const fn region(&self) -> MzWriteBackLayoutRegion {
        self.region
    }

    pub(crate) const fn max_fullwidth_chars(&self) -> MaxFullwidthChars {
        self.max_fullwidth_chars
    }

    pub(crate) const fn auto_wrap_enabled(&self) -> bool {
        self.auto_wrap_enabled
    }

    pub(crate) fn segments(&self) -> &[MzWriteBackLayoutSegment] {
        &self.segments
    }
}

/// 布局器为一个数据库译文叶子产生的最终显示行。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MzWriteBackLaidOutSegment {
    exact_location: MzLocation,
    lines: Vec<String>,
}

impl MzWriteBackLaidOutSegment {
    pub(crate) fn new(
        exact_location: MzLocation,
        lines: Vec<String>,
    ) -> Result<Self, MzWriteBackAppliedLayoutError> {
        if lines.is_empty() {
            return Err(MzWriteBackAppliedLayoutError::EmptyReplacement {
                exact_location: Box::new(exact_location),
            });
        }
        if let Some(line_index) = lines.iter().position(|line| line.contains('\n')) {
            return Err(MzWriteBackAppliedLayoutError::EmbeddedLineBreak {
                exact_location: Box::new(exact_location),
                line_index,
            });
        }
        Ok(Self {
            exact_location,
            lines,
        })
    }

    #[cfg(test)]
    pub(crate) fn exact_location(&self) -> &MzLocation {
        &self.exact_location
    }

    #[cfg(test)]
    pub(crate) fn lines(&self) -> &[String] {
        &self.lines
    }
}

/// 一次已经通过请求对应性校验的布局成功结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MzWriteBackAppliedLayout {
    segments: Vec<MzWriteBackLaidOutSegment>,
    inserted_line_breaks: usize,
    inserted_fullwidth_indents: usize,
}

impl MzWriteBackAppliedLayout {
    pub(crate) fn new(
        request: &MzWriteBackLayoutRequest,
        segments: Vec<MzWriteBackLaidOutSegment>,
        inserted_line_breaks: usize,
        inserted_fullwidth_indents: usize,
    ) -> Result<Self, MzWriteBackAppliedLayoutError> {
        let mut replacements = BTreeMap::new();
        for segment in segments {
            let location = segment.exact_location.clone();
            if replacements.insert(location.clone(), segment).is_some() {
                return Err(MzWriteBackAppliedLayoutError::DuplicateReplacement {
                    exact_location: Box::new(location),
                });
            }
        }

        let mut ordered = Vec::new();
        for request_segment in &request.segments {
            match request_segment.candidate {
                MzWriteBackLayoutCandidate::FrozenOriginal => {
                    if replacements.contains_key(&request_segment.exact_location) {
                        return Err(MzWriteBackAppliedLayoutError::ChangesFrozenOriginal {
                            exact_location: Box::new(request_segment.exact_location.clone()),
                        });
                    }
                }
                MzWriteBackLayoutCandidate::DatabaseTranslation(_) => {
                    let Some(segment) = replacements.remove(&request_segment.exact_location) else {
                        return Err(MzWriteBackAppliedLayoutError::MissingReplacement {
                            exact_location: Box::new(request_segment.exact_location.clone()),
                        });
                    };
                    ordered.push(segment);
                }
            }
        }
        if let Some((exact_location, _)) = replacements.into_iter().next() {
            return Err(MzWriteBackAppliedLayoutError::UnexpectedReplacement {
                exact_location: Box::new(exact_location),
            });
        }

        Ok(Self {
            segments: ordered,
            inserted_line_breaks,
            inserted_fullwidth_indents,
        })
    }

    #[cfg(test)]
    pub(crate) fn segments(&self) -> &[MzWriteBackLaidOutSegment] {
        &self.segments
    }

    #[cfg(test)]
    pub(crate) const fn inserted_line_breaks(&self) -> usize {
        self.inserted_line_breaks
    }

    #[cfg(test)]
    pub(crate) const fn inserted_fullwidth_indents(&self) -> usize {
        self.inserted_fullwidth_indents
    }

    fn into_parts(self) -> (Vec<MzWriteBackLaidOutSegment>, usize, usize) {
        (
            self.segments,
            self.inserted_line_breaks,
            self.inserted_fullwidth_indents,
        )
    }
}

/// 布局器在构造 Applied 结果时违反请求边界。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum MzWriteBackAppliedLayoutError {
    EmptyReplacement {
        exact_location: Box<MzLocation>,
    },
    EmbeddedLineBreak {
        exact_location: Box<MzLocation>,
        line_index: usize,
    },
    DuplicateReplacement {
        exact_location: Box<MzLocation>,
    },
    ChangesFrozenOriginal {
        exact_location: Box<MzLocation>,
    },
    MissingReplacement {
        exact_location: Box<MzLocation>,
    },
    UnexpectedReplacement {
        exact_location: Box<MzLocation>,
    },
}

impl fmt::Display for MzWriteBackAppliedLayoutError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyReplacement { exact_location } => {
                write!(formatter, "布局结果没有提供任何显示行：{exact_location}")
            }
            Self::EmbeddedLineBreak {
                exact_location,
                line_index,
            } => write!(
                formatter,
                "布局结果第 {line_index} 个显示行仍包含真实换行：{exact_location}"
            ),
            Self::DuplicateReplacement { exact_location } => {
                write!(formatter, "布局结果重复返回位置：{exact_location}")
            }
            Self::ChangesFrozenOriginal { exact_location } => {
                write!(formatter, "布局结果试图修改缺译原文：{exact_location}")
            }
            Self::MissingReplacement { exact_location } => {
                write!(formatter, "布局结果缺少数据库译文位置：{exact_location}")
            }
            Self::UnexpectedReplacement { exact_location } => {
                write!(formatter, "布局结果包含请求外位置：{exact_location}")
            }
        }
    }
}

impl Error for MzWriteBackAppliedLayoutError {}

/// 保守布局的正常业务结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum MzWriteBackLayoutOutcome {
    Applied(MzWriteBackAppliedLayout),
    /// 无法保证阅读质量；调用方必须撤销整个单元的自动布局。
    Manual,
}

/// 对一个完整 MZ 显示单元执行保守布局。
///
/// 本能力是同步纯业务计算，并且必须遵守以下交接约束：
///
/// - 请求已经显式给出区域与该区域的行宽，不得自行读取或选择整个布局 Profile；
/// - `auto_wrap_enabled == false` 只禁止新增自动换行，数据库译文已有的真实换行仍是
///   必须保留的硬边界；
/// - 段边界就是数据库叶子的来源边界：可以跨段观察括号和缩进状态，但不得把字符
///   移动到其他段，也不得返回对 `FrozenOriginal` 的修改；
/// - 必须先决定自动换行，再为符合规则的续行补全角空格；
/// - `inserted_line_breaks` 与 `inserted_fullwidth_indents` 只统计本次自动新增内容，
///   不包含数据库硬换行、原 401/405 边界或原文已有空格。
///
/// 控制符不明确、没有安全断点或无法完整遵守上述规则时，必须对整个请求返回
/// `Manual`，不得升级为技术错误或强制切断文本。
pub(crate) trait MzWriteBackTextLayouter: Send + Sync {
    fn layout(&self, request: &MzWriteBackLayoutRequest) -> MzWriteBackLayoutOutcome;
}

/// 一次普通单值文本替换。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SetTextMutation {
    exact_location: MzLocation,
    expected_original: String,
    replacement: String,
}

impl SetTextMutation {
    #[cfg(test)]
    pub(crate) fn for_test(
        exact_location: MzLocation,
        expected_original: impl Into<String>,
        replacement: impl Into<String>,
    ) -> Self {
        Self {
            exact_location,
            expected_original: expected_original.into(),
            replacement: replacement.into(),
        }
    }

    fn from_leaf(leaf: StandardWriteBackLeaf, replacement: String) -> Self {
        Self {
            exact_location: leaf.exact_location,
            expected_original: leaf.original_text,
            replacement,
        }
    }

    pub(crate) fn exact_location(&self) -> &MzLocation {
        &self.exact_location
    }

    pub(crate) fn expected_original(&self) -> &str {
        &self.expected_original
    }

    pub(crate) fn replacement(&self) -> &str {
        &self.replacement
    }
}

/// 需要由 Rewriter 映射为原生事件命令的正文类型。
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum EventBodyKind {
    Dialogue,
    ScrollingText,
}

/// 事件正文中一个原始命令的最终处理方式。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum EventBodyMutationAction {
    /// 数据库没有译文，Rewriter 必须保留原命令及文本。
    KeepOriginal,
    /// 用一条或多条原生正文命令替换当前译文叶子。
    ReplaceWithLines(Vec<String>),
}

/// 一条原始 401/405 正文在块级重建计划中的对应项。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EventBodyMutationSegment {
    exact_location: MzLocation,
    expected_original: String,
    action: EventBodyMutationAction,
}

impl EventBodyMutationSegment {
    #[cfg(test)]
    pub(crate) fn keep_for_test(
        exact_location: MzLocation,
        expected_original: impl Into<String>,
    ) -> Self {
        Self {
            exact_location,
            expected_original: expected_original.into(),
            action: EventBodyMutationAction::KeepOriginal,
        }
    }

    #[cfg(test)]
    pub(crate) fn replace_for_test(
        exact_location: MzLocation,
        expected_original: impl Into<String>,
        lines: Vec<String>,
    ) -> Self {
        Self {
            exact_location,
            expected_original: expected_original.into(),
            action: EventBodyMutationAction::ReplaceWithLines(lines),
        }
    }

    fn keep_original(leaf: StandardWriteBackLeaf) -> Self {
        Self {
            exact_location: leaf.exact_location,
            expected_original: leaf.original_text,
            action: EventBodyMutationAction::KeepOriginal,
        }
    }

    fn replace(leaf: StandardWriteBackLeaf, lines: Vec<String>) -> Self {
        debug_assert!(!lines.is_empty(), "译文叶必须至少产生一个原生正文行");
        Self {
            exact_location: leaf.exact_location,
            expected_original: leaf.original_text,
            action: EventBodyMutationAction::ReplaceWithLines(lines),
        }
    }

    pub(crate) fn exact_location(&self) -> &MzLocation {
        &self.exact_location
    }

    pub(crate) fn expected_original(&self) -> &str {
        &self.expected_original
    }

    pub(crate) fn action(&self) -> &EventBodyMutationAction {
        &self.action
    }
}

/// 一个完整对话或滚动文本正文块的重建计划。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReplaceEventBodyMutation {
    kind: EventBodyKind,
    group_location: MzLocation,
    segments: Vec<EventBodyMutationSegment>,
}

impl ReplaceEventBodyMutation {
    pub(crate) fn new(
        kind: EventBodyKind,
        group_location: MzLocation,
        segments: Vec<EventBodyMutationSegment>,
    ) -> Result<Self, StandardWriteBackMutationPlanError> {
        if segments.is_empty() {
            return Err(StandardWriteBackMutationPlanError::EmptyEventBody { group_location });
        }
        if !segments
            .iter()
            .any(|segment| matches!(segment.action, EventBodyMutationAction::ReplaceWithLines(_)))
        {
            return Err(
                StandardWriteBackMutationPlanError::EventBodyWithoutTranslation { group_location },
            );
        }
        let mut exact_locations = BTreeSet::new();
        for segment in &segments {
            if !exact_locations.insert(segment.exact_location.clone()) {
                return Err(StandardWriteBackMutationPlanError::DuplicateLocation {
                    exact_location: segment.exact_location.clone(),
                });
            }
            if let EventBodyMutationAction::ReplaceWithLines(lines) = &segment.action
                && lines.is_empty()
            {
                return Err(StandardWriteBackMutationPlanError::EmptyEventReplacement {
                    exact_location: segment.exact_location.clone(),
                });
            }
        }
        Ok(Self {
            kind,
            group_location,
            segments,
        })
    }

    pub(crate) const fn kind(&self) -> EventBodyKind {
        self.kind
    }

    pub(crate) fn group_location(&self) -> &MzLocation {
        &self.group_location
    }

    pub(crate) fn segments(&self) -> &[EventBodyMutationSegment] {
        &self.segments
    }
}

/// Standard 交给 MZ 文档改写器的一项领域修改。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StandardWriteBackMutation {
    SetText(SetTextMutation),
    ReplaceEventBody(ReplaceEventBodyMutation),
}

/// 已经排除位置冲突的一轮完整文档修改计划。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct StandardWriteBackMutationPlan {
    mutations: Vec<StandardWriteBackMutation>,
}

impl StandardWriteBackMutationPlan {
    pub(crate) fn new(
        mutations: Vec<StandardWriteBackMutation>,
    ) -> Result<Self, StandardWriteBackMutationPlanError> {
        let mut exact_locations = BTreeSet::new();
        let mut event_groups = BTreeSet::new();
        for mutation in &mutations {
            match mutation {
                StandardWriteBackMutation::SetText(mutation) => {
                    if !exact_locations.insert(mutation.exact_location.clone()) {
                        return Err(StandardWriteBackMutationPlanError::DuplicateLocation {
                            exact_location: mutation.exact_location.clone(),
                        });
                    }
                }
                StandardWriteBackMutation::ReplaceEventBody(mutation) => {
                    if !event_groups.insert(mutation.group_location.clone()) {
                        return Err(StandardWriteBackMutationPlanError::DuplicateEventBody {
                            group_location: mutation.group_location.clone(),
                        });
                    }
                    if !exact_locations.insert(mutation.group_location.clone()) {
                        return Err(StandardWriteBackMutationPlanError::DuplicateLocation {
                            exact_location: mutation.group_location.clone(),
                        });
                    }
                    for segment in &mutation.segments {
                        if !exact_locations.insert(segment.exact_location.clone()) {
                            return Err(StandardWriteBackMutationPlanError::DuplicateLocation {
                                exact_location: segment.exact_location.clone(),
                            });
                        }
                    }
                }
            }
        }
        Ok(Self { mutations })
    }

    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self::default()
    }

    pub(crate) fn mutations(&self) -> &[StandardWriteBackMutation] {
        &self.mutations
    }
}

/// Mutation 计划构造时发现的内部冲突。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StandardWriteBackMutationPlanError {
    EmptyEventBody { group_location: MzLocation },
    EventBodyWithoutTranslation { group_location: MzLocation },
    EmptyEventReplacement { exact_location: MzLocation },
    DuplicateLocation { exact_location: MzLocation },
    DuplicateEventBody { group_location: MzLocation },
}

impl fmt::Display for StandardWriteBackMutationPlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyEventBody { group_location } => {
                write!(formatter, "事件正文修改不包含原始段：{group_location}")
            }
            Self::EventBodyWithoutTranslation { group_location } => {
                write!(
                    formatter,
                    "事件正文修改没有任何数据库译文：{group_location}"
                )
            }
            Self::EmptyEventReplacement { exact_location } => {
                write!(formatter, "事件正文译文没有产生显示行：{exact_location}")
            }
            Self::DuplicateLocation { exact_location } => {
                write!(formatter, "Mutation 计划重复修改位置：{exact_location}")
            }
            Self::DuplicateEventBody { group_location } => {
                write!(formatter, "Mutation 计划重复修改事件正文：{group_location}")
            }
        }
    }
}

impl Error for StandardWriteBackMutationPlanError {}

/// 把领域 Mutation 应用到冻结 MZ 文档并产生一个待发布候选。
///
/// 实现必须从 `OpenedProject::source_root()` 下的冻结文档读取权威结构，并在修改前用
/// `expected_original` 核对每个目标仍与快照一致。每项 Mutation 必须恰好应用一次；
/// 目标缺失、重复或原文不匹配都是技术错误。`ReplaceEventBody` 由本能力映射为对应的
/// 401/405 命令块：`KeepOriginal` 逐字保留原命令，`ReplaceWithLines` 按给定显示行重建
/// 命令。本能力只产生候选，不发布文件，也不把领域计划泄漏成 JSON 或字节覆盖集合。
pub(crate) trait MzWriteBackDocumentRewriter: Send + Sync {
    type RewrittenDocuments: Send + 'static;
    type Error: Error + Send + Sync + 'static;

    fn rewrite(
        &self,
        project: &OpenedProject,
        plan: StandardWriteBackMutationPlan,
    ) -> impl Future<Output = Result<Self::RewrittenDocuments, Self::Error>> + Send;
}

/// 把完整候选发布为项目固定的最新 `write_back/` 输出。
///
/// 实现必须以冻结 `source/data`、`source/js` 为基底，把候选改写与所有未修改文件组成
/// 完整副本；即使 Mutation 为空也必须发布完整输出。唯一成功目标是
/// `OpenedProject::write_back_root()`。成功表示新的 `data/`、`js/` 已共同成为固定最新
/// 输出；失败不得暴露半成品，也不得破坏此前一次成功输出。
pub(crate) trait StandardWriteBackPublisher<D>: Send + Sync
where
    D: Send + 'static,
{
    type Error: Error + Send + Sync + 'static;

    fn publish(
        &self,
        project: &OpenedProject,
        documents: D,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// 一项需要人工调整布局、但没有阻止写回的结构化诊断。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManualLayoutDiagnostic {
    unit_location: MzLocation,
    region: MzWriteBackLayoutRegion,
    max_fullwidth_chars: MaxFullwidthChars,
}

impl ManualLayoutDiagnostic {
    fn from_request(request: &MzWriteBackLayoutRequest) -> Self {
        Self {
            unit_location: request.unit_location.clone(),
            region: request.region,
            max_fullwidth_chars: request.max_fullwidth_chars,
        }
    }

    pub(crate) fn unit_location(&self) -> &MzLocation {
        &self.unit_location
    }

    pub(crate) const fn region(&self) -> MzWriteBackLayoutRegion {
        self.region
    }

    pub(crate) const fn max_fullwidth_chars(&self) -> MaxFullwidthChars {
        self.max_fullwidth_chars
    }
}

/// Standard 已成功发布后一次性写入持久日志的完整运行事实。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StandardWriteBackRunLog {
    name: ProjectName,
    layout_profile: MzWriteBackLayoutProfile,
    output_root: PathBuf,
    summary: StandardWriteBackSummary,
    manual_layout_diagnostics: Vec<ManualLayoutDiagnostic>,
}

impl StandardWriteBackRunLog {
    pub(crate) fn new(
        project: &OpenedProject,
        layout_profile: MzWriteBackLayoutProfile,
        summary: StandardWriteBackSummary,
        manual_layout_diagnostics: Vec<ManualLayoutDiagnostic>,
    ) -> Self {
        assert_eq!(
            summary.manual_layout_units,
            manual_layout_diagnostics.len(),
            "人工布局计数必须由结构化诊断唯一建立"
        );
        Self {
            name: project.name().clone(),
            layout_profile,
            output_root: project.write_back_root().to_path_buf(),
            summary,
            manual_layout_diagnostics,
        }
    }

    pub(crate) fn name(&self) -> &ProjectName {
        &self.name
    }

    pub(crate) const fn layout_profile(&self) -> MzWriteBackLayoutProfile {
        self.layout_profile
    }

    pub(crate) fn output_root(&self) -> &Path {
        &self.output_root
    }

    pub(crate) const fn summary(&self) -> StandardWriteBackSummary {
        self.summary
    }

    pub(crate) fn manual_layout_diagnostics(&self) -> &[ManualLayoutDiagnostic] {
        &self.manual_layout_diagnostics
    }
}

/// 使用四个业务能力和唯一持久日志根完成 Standard 写回。
pub(crate) struct StandardWriteBackService<R, L, D, P, J> {
    asset_reader: R,
    text_layouter: L,
    document_rewriter: D,
    publisher: P,
    event_log: J,
    cancellation: CooperativeCancellation,
}

impl<R, L, D, P, J> StandardWriteBackService<R, L, D, P, J> {
    pub(crate) fn new(
        asset_reader: R,
        text_layouter: L,
        document_rewriter: D,
        publisher: P,
        event_log: J,
        cancellation: CooperativeCancellation,
    ) -> Self {
        Self {
            asset_reader,
            text_layouter,
            document_rewriter,
            publisher,
            event_log,
            cancellation,
        }
    }
}

impl<R, L, D, P, J> StandardWriteBack for StandardWriteBackService<R, L, D, P, J>
where
    R: StandardWriteBackAssetReader,
    L: MzWriteBackTextLayouter,
    D: MzWriteBackDocumentRewriter,
    P: StandardWriteBackPublisher<D::RewrittenDocuments>,
    J: PersistentEventLog<StandardWriteBackRunLog>,
{
    type Error = StandardWriteBackServiceError<R::Error, D::Error, P::Error, J::Error>;

    async fn run(
        &self,
        project: &OpenedProject,
        layout_profile: &MzWriteBackLayoutProfile,
    ) -> Result<StandardWriteBackReport, Self::Error> {
        self.cancellation
            .check()
            .map_err(StandardWriteBackServiceError::Cancelled)?;
        let snapshot = self
            .asset_reader
            .read(project)
            .await
            .map_err(StandardWriteBackServiceError::ReadAssets)?;
        self.cancellation
            .check()
            .map_err(StandardWriteBackServiceError::Cancelled)?;
        let planned = plan_standard_write_back(snapshot, layout_profile, &self.text_layouter);
        self.cancellation
            .check()
            .map_err(StandardWriteBackServiceError::Cancelled)?;
        let rewritten = self
            .document_rewriter
            .rewrite(project, planned.mutation_plan)
            .await
            .map_err(StandardWriteBackServiceError::RewriteDocuments)?;
        self.cancellation
            .check()
            .map_err(StandardWriteBackServiceError::Cancelled)?;
        self.publisher
            .publish(project, rewritten)
            .await
            .map_err(StandardWriteBackServiceError::Publish)?;

        let published = PublishedWriteBack::new(project);
        let output_root = published.output_root().to_path_buf();
        self.event_log
            .append(StandardWriteBackRunLog::new(
                project,
                *layout_profile,
                planned.summary,
                planned.manual_layout_diagnostics,
            ))
            .await
            .map_err(|source| StandardWriteBackServiceError::RecordPublishedRun {
                output_root,
                source,
            })?;

        self.cancellation
            .check()
            .map_err(StandardWriteBackServiceError::Cancelled)?;

        Ok(StandardWriteBackReport::new(published, planned.summary))
    }
}

struct PlannedStandardWriteBack {
    mutation_plan: StandardWriteBackMutationPlan,
    summary: StandardWriteBackSummary,
    manual_layout_diagnostics: Vec<ManualLayoutDiagnostic>,
}

fn plan_standard_write_back(
    snapshot: StandardWriteBackSnapshot,
    profile: &MzWriteBackLayoutProfile,
    layouter: &impl MzWriteBackTextLayouter,
) -> PlannedStandardWriteBack {
    let mut mutations = Vec::new();
    let mut summary = StandardWriteBackSummary::default();
    let mut manual_layout_diagnostics = Vec::new();

    for group in snapshot.into_groups() {
        for leaf in &group.leaves {
            if leaf.translation.is_some() {
                summary.translated_locations += 1;
            } else {
                summary.original_locations += 1;
            }
        }

        let (kind, group_location, leaves) = group.into_parts();
        match kind {
            TextGroupKind::EventDialogue => plan_dialogue_group(
                group_location,
                leaves,
                profile.dialogue_body(),
                layouter,
                &mut mutations,
                &mut summary,
                &mut manual_layout_diagnostics,
            ),
            TextGroupKind::EventScrollingText => plan_scrolling_group(
                group_location,
                leaves,
                profile.scrolling_text(),
                layouter,
                &mut mutations,
                &mut summary,
                &mut manual_layout_diagnostics,
            ),
            _ => plan_scalar_group(
                kind,
                leaves,
                profile.help_description(),
                layouter,
                &mut mutations,
                &mut summary,
                &mut manual_layout_diagnostics,
            ),
        }
    }

    summary.manual_layout_units = manual_layout_diagnostics.len();
    let mutation_plan = StandardWriteBackMutationPlan::new(mutations)
        .expect("受信快照和布局结果不应产生冲突 Mutation");
    PlannedStandardWriteBack {
        mutation_plan,
        summary,
        manual_layout_diagnostics,
    }
}

fn plan_dialogue_group(
    group_location: MzLocation,
    leaves: Vec<StandardWriteBackLeaf>,
    max_fullwidth_chars: MaxFullwidthChars,
    layouter: &impl MzWriteBackTextLayouter,
    mutations: &mut Vec<StandardWriteBackMutation>,
    summary: &mut StandardWriteBackSummary,
    manual_layout_diagnostics: &mut Vec<ManualLayoutDiagnostic>,
) {
    let mut body = Vec::new();
    for leaf in leaves {
        match leaf.role {
            StandardWriteBackFieldRole::DialogueSpeaker => {
                if let Some(translation) = leaf.translation.clone() {
                    mutations.push(StandardWriteBackMutation::SetText(
                        SetTextMutation::from_leaf(leaf, translation),
                    ));
                }
            }
            StandardWriteBackFieldRole::DialogueBody { .. } => body.push(leaf),
            _ => unreachable!("受信对话组只包含 speaker 和 dialogue body"),
        }
    }
    plan_event_body(
        EventBodyKind::Dialogue,
        MzWriteBackLayoutRegion::DialogueBody,
        group_location,
        body,
        max_fullwidth_chars,
        layouter,
        mutations,
        summary,
        manual_layout_diagnostics,
    );
}

fn plan_scrolling_group(
    group_location: MzLocation,
    leaves: Vec<StandardWriteBackLeaf>,
    max_fullwidth_chars: MaxFullwidthChars,
    layouter: &impl MzWriteBackTextLayouter,
    mutations: &mut Vec<StandardWriteBackMutation>,
    summary: &mut StandardWriteBackSummary,
    manual_layout_diagnostics: &mut Vec<ManualLayoutDiagnostic>,
) {
    plan_event_body(
        EventBodyKind::ScrollingText,
        MzWriteBackLayoutRegion::ScrollingText,
        group_location,
        leaves,
        max_fullwidth_chars,
        layouter,
        mutations,
        summary,
        manual_layout_diagnostics,
    );
}

#[allow(
    clippy::too_many_arguments,
    reason = "参数逐项表达一个事件正文规划上下文"
)]
fn plan_event_body(
    kind: EventBodyKind,
    region: MzWriteBackLayoutRegion,
    group_location: MzLocation,
    leaves: Vec<StandardWriteBackLeaf>,
    max_fullwidth_chars: MaxFullwidthChars,
    layouter: &impl MzWriteBackTextLayouter,
    mutations: &mut Vec<StandardWriteBackMutation>,
    summary: &mut StandardWriteBackSummary,
    manual_layout_diagnostics: &mut Vec<ManualLayoutDiagnostic>,
) {
    if !leaves.iter().any(|leaf| leaf.translation.is_some()) {
        return;
    }
    let request = MzWriteBackLayoutRequest::new(
        group_location.clone(),
        region,
        max_fullwidth_chars,
        true,
        leaves
            .iter()
            .map(MzWriteBackLayoutSegment::from_leaf)
            .collect(),
    );

    let replacements = match layouter.layout(&request) {
        MzWriteBackLayoutOutcome::Applied(applied) => {
            let (segments, inserted_line_breaks, inserted_fullwidth_indents) = applied.into_parts();
            record_applied_layout(summary, inserted_line_breaks, inserted_fullwidth_indents);
            segments
                .into_iter()
                .map(|segment| (segment.exact_location, segment.lines))
                .collect::<BTreeMap<_, _>>()
        }
        MzWriteBackLayoutOutcome::Manual => {
            manual_layout_diagnostics.push(ManualLayoutDiagnostic::from_request(&request));
            request
                .segments
                .iter()
                .filter_map(|segment| match &segment.candidate {
                    MzWriteBackLayoutCandidate::FrozenOriginal => None,
                    MzWriteBackLayoutCandidate::DatabaseTranslation(translation) => Some((
                        segment.exact_location.clone(),
                        split_hard_lines(translation),
                    )),
                })
                .collect()
        }
    };

    let mut replacements = replacements;
    let segments = leaves
        .into_iter()
        .map(|leaf| {
            if leaf.translation.is_some() {
                let lines = replacements
                    .remove(&leaf.exact_location)
                    .expect("受信布局结果必须覆盖每个数据库译文叶");
                EventBodyMutationSegment::replace(leaf, lines)
            } else {
                EventBodyMutationSegment::keep_original(leaf)
            }
        })
        .collect();
    debug_assert!(replacements.is_empty());
    let mutation = ReplaceEventBodyMutation::new(kind, group_location, segments)
        .expect("受信事件正文应建立合法块级 Mutation");
    mutations.push(StandardWriteBackMutation::ReplaceEventBody(mutation));
}

#[allow(clippy::too_many_arguments, reason = "参数逐项表达单值字段规划上下文")]
fn plan_scalar_group(
    kind: TextGroupKind,
    leaves: Vec<StandardWriteBackLeaf>,
    help_max_fullwidth_chars: MaxFullwidthChars,
    layouter: &impl MzWriteBackTextLayouter,
    mutations: &mut Vec<StandardWriteBackMutation>,
    summary: &mut StandardWriteBackSummary,
    manual_layout_diagnostics: &mut Vec<ManualLayoutDiagnostic>,
) {
    for leaf in leaves {
        let Some(raw_translation) = leaf.translation.clone() else {
            continue;
        };
        if kind != TextGroupKind::DatabaseEntry || !is_canonical_help_description(&leaf) {
            mutations.push(StandardWriteBackMutation::SetText(
                SetTextMutation::from_leaf(leaf, raw_translation),
            ));
            continue;
        }

        let request = MzWriteBackLayoutRequest::new(
            leaf.exact_location.clone(),
            MzWriteBackLayoutRegion::HelpDescription,
            help_max_fullwidth_chars,
            leaf.original_text.contains('\n'),
            vec![MzWriteBackLayoutSegment::from_leaf(&leaf)],
        );
        let replacement = match layouter.layout(&request) {
            MzWriteBackLayoutOutcome::Applied(applied) => {
                let (mut segments, inserted_line_breaks, inserted_fullwidth_indents) =
                    applied.into_parts();
                record_applied_layout(summary, inserted_line_breaks, inserted_fullwidth_indents);
                segments
                    .pop()
                    .expect("帮助说明布局必须返回唯一译文叶")
                    .lines
                    .join("\n")
            }
            MzWriteBackLayoutOutcome::Manual => {
                manual_layout_diagnostics.push(ManualLayoutDiagnostic::from_request(&request));
                raw_translation
            }
        };
        mutations.push(StandardWriteBackMutation::SetText(
            SetTextMutation::from_leaf(leaf, replacement),
        ));
    }
}

fn record_applied_layout(
    summary: &mut StandardWriteBackSummary,
    inserted_line_breaks: usize,
    inserted_fullwidth_indents: usize,
) {
    if inserted_line_breaks > 0 {
        summary.auto_wrapped_units += 1;
    }
    summary.inserted_line_breaks += inserted_line_breaks;
    summary.inserted_fullwidth_indents += inserted_fullwidth_indents;
}

fn split_hard_lines(text: &str) -> Vec<String> {
    text.split('\n').map(str::to_owned).collect()
}

fn is_canonical_help_description(leaf: &StandardWriteBackLeaf) -> bool {
    if !matches!(
        leaf.role,
        StandardWriteBackFieldRole::Scalar { ref field_name } if field_name == "description"
    ) {
        return false;
    }
    let MzLocation::Value { source, steps } = &leaf.exact_location else {
        return false;
    };
    let MzSource::Data(file) = source else {
        return false;
    };
    if !matches!(
        file,
        StandardDataFile::Skills
            | StandardDataFile::Items
            | StandardDataFile::Weapons
            | StandardDataFile::Armors
    ) {
        return false;
    }
    matches!(
        steps.as_slice(),
        [MzLocationStep::ArrayIndex(_), MzLocationStep::ObjectKey(field_name)]
            if field_name == "description"
    )
}

/// Standard 在四个业务依赖和发布后日志边界上遇到的技术失败。
#[derive(Debug)]
pub(crate) enum StandardWriteBackServiceError<R, D, P, J> {
    Cancelled(OperationCancelled),
    ReadAssets(R),
    RewriteDocuments(D),
    Publish(P),
    RecordPublishedRun { output_root: PathBuf, source: J },
}

impl<R, D, P, J> fmt::Display for StandardWriteBackServiceError<R, D, P, J>
where
    R: fmt::Display,
    D: fmt::Display,
    P: fmt::Display,
    J: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled(error) => error.fmt(formatter),
            Self::ReadAssets(source) => write!(formatter, "读取 Standard 写回资产失败：{source}"),
            Self::RewriteDocuments(source) => write!(formatter, "改写 MZ 文档失败：{source}"),
            Self::Publish(source) => write!(formatter, "发布 Standard 写回输出失败：{source}"),
            Self::RecordPublishedRun {
                output_root,
                source,
            } => write!(
                formatter,
                "Standard 输出已发布到 {}，但持久日志记录失败：{source}",
                output_root.display()
            ),
        }
    }
}

impl<R, D, P, J> Error for StandardWriteBackServiceError<R, D, P, J>
where
    R: Error + 'static,
    D: Error + 'static,
    P: Error + 'static,
    J: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Cancelled(error) => Some(error),
            Self::ReadAssets(source) => Some(source),
            Self::RewriteDocuments(source) => Some(source),
            Self::Publish(source) => Some(source),
            Self::RecordPublishedRun { source, .. } => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum Stage {
        Read,
        Layout(MzWriteBackLayoutRegion),
        Rewrite,
        Publish(PathBuf),
        Log,
    }

    #[derive(Clone, Debug, Default)]
    struct Recording {
        stages: Vec<Stage>,
        requests: Vec<MzWriteBackLayoutRequest>,
        plans: Vec<StandardWriteBackMutationPlan>,
        published_documents: Vec<RewrittenDocuments>,
        logs: Vec<StandardWriteBackRunLog>,
    }

    type SharedRecording = Arc<Mutex<Recording>>;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakeError(&'static str);

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl Error for FakeError {}

    #[derive(Clone)]
    struct FakeAssetReader {
        response: Result<StandardWriteBackSnapshot, FakeError>,
        recording: SharedRecording,
    }

    impl StandardWriteBackAssetReader for FakeAssetReader {
        type Error = FakeError;

        async fn read(&self, _: &OpenedProject) -> Result<StandardWriteBackSnapshot, Self::Error> {
            self.recording
                .lock()
                .expect("记录锁不应中毒")
                .stages
                .push(Stage::Read);
            self.response.clone()
        }
    }

    #[derive(Clone, Default)]
    struct FakeLayoutConfig {
        manual_regions: Vec<MzWriteBackLayoutRegion>,
        replacements: BTreeMap<MzLocation, Vec<String>>,
        stats: Vec<(MzWriteBackLayoutRegion, usize, usize)>,
    }

    #[derive(Clone)]
    struct FakeLayouter {
        config: FakeLayoutConfig,
        recording: SharedRecording,
    }

    impl MzWriteBackTextLayouter for FakeLayouter {
        fn layout(&self, request: &MzWriteBackLayoutRequest) -> MzWriteBackLayoutOutcome {
            let mut recording = self.recording.lock().expect("记录锁不应中毒");
            recording.stages.push(Stage::Layout(request.region()));
            recording.requests.push(request.clone());
            drop(recording);

            if self.config.manual_regions.contains(&request.region()) {
                return MzWriteBackLayoutOutcome::Manual;
            }
            let segments = request
                .segments()
                .iter()
                .filter_map(|segment| match segment.candidate() {
                    MzWriteBackLayoutCandidate::FrozenOriginal => None,
                    MzWriteBackLayoutCandidate::DatabaseTranslation(translation) => {
                        let lines = self
                            .config
                            .replacements
                            .get(segment.exact_location())
                            .cloned()
                            .unwrap_or_else(|| split_hard_lines(translation));
                        Some(
                            MzWriteBackLaidOutSegment::new(segment.exact_location().clone(), lines)
                                .expect("测试布局行应满足契约"),
                        )
                    }
                })
                .collect();
            let (inserted_line_breaks, inserted_fullwidth_indents) = self
                .config
                .stats
                .iter()
                .find_map(|(region, breaks, indents)| {
                    (*region == request.region()).then_some((*breaks, *indents))
                })
                .unwrap_or_default();
            MzWriteBackLayoutOutcome::Applied(
                MzWriteBackAppliedLayout::new(
                    request,
                    segments,
                    inserted_line_breaks,
                    inserted_fullwidth_indents,
                )
                .expect("测试布局结果应完整覆盖译文叶"),
            )
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct RewrittenDocuments(String);

    #[derive(Clone)]
    struct FakeDocumentRewriter {
        fail: bool,
        recording: SharedRecording,
    }

    impl MzWriteBackDocumentRewriter for FakeDocumentRewriter {
        type RewrittenDocuments = RewrittenDocuments;
        type Error = FakeError;

        async fn rewrite(
            &self,
            _: &OpenedProject,
            plan: StandardWriteBackMutationPlan,
        ) -> Result<Self::RewrittenDocuments, Self::Error> {
            let mut recording = self.recording.lock().expect("记录锁不应中毒");
            recording.stages.push(Stage::Rewrite);
            recording.plans.push(plan);
            if self.fail {
                Err(FakeError("rewrite"))
            } else {
                Ok(RewrittenDocuments("candidate".to_owned()))
            }
        }
    }

    #[derive(Clone)]
    struct FakePublisher {
        fail: bool,
        recording: SharedRecording,
    }

    impl StandardWriteBackPublisher<RewrittenDocuments> for FakePublisher {
        type Error = FakeError;

        async fn publish(
            &self,
            project: &OpenedProject,
            documents: RewrittenDocuments,
        ) -> Result<(), Self::Error> {
            let mut recording = self.recording.lock().expect("记录锁不应中毒");
            recording
                .stages
                .push(Stage::Publish(project.write_back_root().to_path_buf()));
            recording.published_documents.push(documents);
            if self.fail {
                Err(FakeError("publish"))
            } else {
                Ok(())
            }
        }
    }

    #[derive(Clone)]
    struct FakeEventLog {
        fail: bool,
        recording: SharedRecording,
    }

    impl PersistentEventLog<StandardWriteBackRunLog> for FakeEventLog {
        type Error = FakeError;

        async fn append(&self, event: StandardWriteBackRunLog) -> Result<(), Self::Error> {
            let mut recording = self.recording.lock().expect("记录锁不应中毒");
            recording.stages.push(Stage::Log);
            recording.logs.push(event);
            if self.fail {
                Err(FakeError("log"))
            } else {
                Ok(())
            }
        }
    }

    type Service = StandardWriteBackService<
        FakeAssetReader,
        FakeLayouter,
        FakeDocumentRewriter,
        FakePublisher,
        FakeEventLog,
    >;

    struct Harness {
        service: Service,
        recording: SharedRecording,
    }

    impl Harness {
        fn new(
            snapshot: Result<StandardWriteBackSnapshot, FakeError>,
            layout: FakeLayoutConfig,
            failing_stage: Option<&str>,
        ) -> Self {
            let recording = Arc::new(Mutex::new(Recording::default()));
            let service = StandardWriteBackService::new(
                FakeAssetReader {
                    response: snapshot,
                    recording: Arc::clone(&recording),
                },
                FakeLayouter {
                    config: layout,
                    recording: Arc::clone(&recording),
                },
                FakeDocumentRewriter {
                    fail: failing_stage == Some("rewrite"),
                    recording: Arc::clone(&recording),
                },
                FakePublisher {
                    fail: failing_stage == Some("publish"),
                    recording: Arc::clone(&recording),
                },
                FakeEventLog {
                    fail: failing_stage == Some("log"),
                    recording: Arc::clone(&recording),
                },
                CooperativeCancellation::default(),
            );
            Self { service, recording }
        }

        fn recorded(&self) -> Recording {
            self.recording.lock().expect("记录锁不应中毒").clone()
        }
    }

    fn width(value: u32) -> MaxFullwidthChars {
        MaxFullwidthChars::new(value).expect("测试行宽应为正整数")
    }

    fn profile() -> MzWriteBackLayoutProfile {
        MzWriteBackLayoutProfile::new(width(24), width(30), width(18))
    }

    fn project() -> OpenedProject {
        OpenedProject::new(
            "demo".parse().expect("测试项目名应合法"),
            PathBuf::from("C:/att/projects/demo"),
            PathBuf::from("C:/att/projects/demo/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            profile(),
        )
    }

    fn data_group_location(file: StandardDataFile, index: usize) -> MzLocation {
        MzLocation::value(MzSource::data(file), vec![MzLocationStep::index(index)])
    }

    fn data_field_location(file: StandardDataFile, index: usize, field_name: &str) -> MzLocation {
        MzLocation::value(
            MzSource::data(file),
            vec![
                MzLocationStep::index(index),
                MzLocationStep::key(field_name),
            ],
        )
    }

    fn command_location(map_id: u32, command_index: usize) -> MzLocation {
        MzLocation::value(
            MzSource::map(map_id),
            vec![
                MzLocationStep::key("events"),
                MzLocationStep::index(1),
                MzLocationStep::key("pages"),
                MzLocationStep::index(0),
                MzLocationStep::key("list"),
                MzLocationStep::index(command_index),
            ],
        )
    }

    fn command_parameter_location(
        map_id: u32,
        command_index: usize,
        parameter_index: usize,
    ) -> MzLocation {
        let MzLocation::Value { source, mut steps } = command_location(map_id, command_index)
        else {
            unreachable!("命令位置始终是 Value")
        };
        steps.push(MzLocationStep::key("parameters"));
        steps.push(MzLocationStep::index(parameter_index));
        MzLocation::value(source, steps)
    }

    fn scalar_leaf(
        location: MzLocation,
        field_name: &str,
        original: &str,
        translation: Option<&str>,
    ) -> StandardWriteBackLeaf {
        StandardWriteBackLeaf::new(
            StandardWriteBackFieldRole::scalar(field_name),
            location,
            original,
            translation.map(str::to_owned),
        )
        .expect("测试单值叶应合法")
    }

    fn body_leaf(
        role: StandardWriteBackFieldRole,
        location: MzLocation,
        original: &str,
        translation: Option<&str>,
    ) -> StandardWriteBackLeaf {
        StandardWriteBackLeaf::new(role, location, original, translation.map(str::to_owned))
            .expect("测试正文叶应合法")
    }

    fn group(
        kind: TextGroupKind,
        location: MzLocation,
        leaves: Vec<StandardWriteBackLeaf>,
    ) -> StandardWriteBackGroup {
        StandardWriteBackGroup::new(kind, location, leaves).expect("测试资产组应合法")
    }

    fn snapshot(groups: Vec<StandardWriteBackGroup>) -> StandardWriteBackSnapshot {
        StandardWriteBackSnapshot::new(groups).expect("测试快照应合法")
    }

    #[tokio::test]
    async fn dispatches_every_standard_stage_and_builds_group_aware_mutations() {
        let item_group = data_group_location(StandardDataFile::Items, 1);
        let item_name = data_field_location(StandardDataFile::Items, 1, "name");
        let help = data_field_location(StandardDataFile::Items, 1, "description");
        let state_description = data_field_location(StandardDataFile::States, 1, "description");
        let system_title = data_field_location(StandardDataFile::System, 0, "gameTitle");
        let dialogue_group = command_location(1, 10);
        let speaker = command_parameter_location(1, 10, 4);
        let dialogue_body_0 = command_parameter_location(1, 11, 0);
        let dialogue_body_1 = command_parameter_location(1, 12, 0);
        let scrolling_group = command_location(1, 20);
        let scrolling_body = command_parameter_location(1, 21, 0);

        let assets = snapshot(vec![
            group(
                TextGroupKind::EventScrollingText,
                scrolling_group.clone(),
                vec![body_leaf(
                    StandardWriteBackFieldRole::scrolling_text_body(0),
                    scrolling_body.clone(),
                    "滚动原文",
                    Some("滚动译文"),
                )],
            ),
            group(
                TextGroupKind::DatabaseEntry,
                item_group,
                vec![
                    scalar_leaf(item_name.clone(), "name", "剑", Some("剑")),
                    scalar_leaf(
                        help.clone(),
                        "description",
                        "帮助第一行\n帮助第二行",
                        Some("帮助译文"),
                    ),
                ],
            ),
            group(
                TextGroupKind::DatabaseEntry,
                data_group_location(StandardDataFile::States, 1),
                vec![scalar_leaf(
                    state_description,
                    "description",
                    "状态说明",
                    Some("状态译文"),
                )],
            ),
            group(
                TextGroupKind::System,
                data_group_location(StandardDataFile::System, 0),
                vec![scalar_leaf(system_title, "gameTitle", "原题", None)],
            ),
            group(
                TextGroupKind::EventDialogue,
                dialogue_group.clone(),
                vec![
                    body_leaf(
                        StandardWriteBackFieldRole::dialogue_body(1),
                        dialogue_body_1.clone(),
                        "缺译原文",
                        None,
                    ),
                    body_leaf(
                        StandardWriteBackFieldRole::dialogue_speaker(),
                        speaker,
                        "勇者",
                        Some("Hero"),
                    ),
                    body_leaf(
                        StandardWriteBackFieldRole::dialogue_body(0),
                        dialogue_body_0.clone(),
                        "「原文",
                        Some("「译文\n续"),
                    ),
                ],
            ),
        ]);

        let layout = FakeLayoutConfig {
            replacements: BTreeMap::from([
                (help.clone(), vec!["帮助自动".to_owned(), "换行".to_owned()]),
                (
                    dialogue_body_0.clone(),
                    vec!["「译文".to_owned(), "　续".to_owned()],
                ),
                (
                    scrolling_body.clone(),
                    vec!["滚动".to_owned(), "新增".to_owned()],
                ),
            ]),
            stats: vec![
                (MzWriteBackLayoutRegion::HelpDescription, 1, 1),
                (MzWriteBackLayoutRegion::DialogueBody, 0, 1),
                (MzWriteBackLayoutRegion::ScrollingText, 1, 0),
            ],
            ..FakeLayoutConfig::default()
        };
        let harness = Harness::new(Ok(assets), layout, None);
        let project = project();

        let report = harness
            .service
            .run(&project, project.layout_profile())
            .await
            .expect("Standard 写回应成功");
        let (published, summary) = report.into_parts();

        assert_eq!(published.output_root(), project.write_back_root());
        assert_eq!(
            summary,
            StandardWriteBackSummary {
                translated_locations: 6,
                original_locations: 2,
                auto_wrapped_units: 2,
                inserted_line_breaks: 2,
                inserted_fullwidth_indents: 2,
                manual_layout_units: 0,
            }
        );

        let recorded = harness.recorded();
        assert_eq!(
            recorded.stages,
            vec![
                Stage::Read,
                Stage::Layout(MzWriteBackLayoutRegion::HelpDescription),
                Stage::Layout(MzWriteBackLayoutRegion::DialogueBody),
                Stage::Layout(MzWriteBackLayoutRegion::ScrollingText),
                Stage::Rewrite,
                Stage::Publish(project.write_back_root().to_path_buf()),
                Stage::Log,
            ]
        );
        assert_eq!(recorded.requests.len(), 3);
        assert_eq!(recorded.requests[0].unit_location(), &help);
        assert_eq!(recorded.requests[0].max_fullwidth_chars(), width(18));
        assert!(recorded.requests[0].auto_wrap_enabled());
        assert_eq!(recorded.requests[1].unit_location(), &dialogue_group);
        assert_eq!(recorded.requests[1].max_fullwidth_chars(), width(24));
        assert_eq!(recorded.requests[1].segments().len(), 2);
        assert!(matches!(
            recorded.requests[1].segments()[1].candidate(),
            MzWriteBackLayoutCandidate::FrozenOriginal
        ));
        assert_eq!(recorded.requests[2].max_fullwidth_chars(), width(30));

        let plan = &recorded.plans[0];
        assert_eq!(plan.mutations().len(), 6);
        let dialogue_mutation = plan
            .mutations()
            .iter()
            .find_map(|mutation| match mutation {
                StandardWriteBackMutation::ReplaceEventBody(mutation)
                    if mutation.kind() == EventBodyKind::Dialogue =>
                {
                    Some(mutation)
                }
                _ => None,
            })
            .expect("应包含对话正文块");
        assert_eq!(dialogue_mutation.group_location(), &dialogue_group);
        assert_eq!(dialogue_mutation.segments().len(), 2);
        assert_eq!(
            dialogue_mutation.segments()[0].action(),
            &EventBodyMutationAction::ReplaceWithLines(vec![
                "「译文".to_owned(),
                "　续".to_owned(),
            ])
        );
        assert_eq!(
            dialogue_mutation.segments()[1].action(),
            &EventBodyMutationAction::KeepOriginal
        );
        assert_eq!(recorded.published_documents.len(), 1);
        assert_eq!(recorded.logs.len(), 1);
        assert_eq!(recorded.logs[0].name(), project.name());
        assert_eq!(recorded.logs[0].output_root(), project.write_back_root());
        assert_eq!(recorded.logs[0].summary(), summary);
        assert!(recorded.logs[0].manual_layout_diagnostics().is_empty());
    }

    #[tokio::test]
    async fn only_canonical_help_description_is_laid_out_and_original_controls_auto_wrap() {
        let canonical = data_field_location(StandardDataFile::Skills, 1, "description");
        let state = data_field_location(StandardDataFile::States, 1, "description");
        let note = MzLocation::note_tag(
            MzSource::data(StandardDataFile::Items),
            vec![MzLocationStep::index(2)],
            "description",
            0,
        );
        let plugin_source = MzSource::plugin_parameter(0, "QuestMenu", "Description");
        let plugin_group = MzLocation::value(plugin_source.clone(), Vec::new());
        let plugin = MzLocation::value(
            plugin_source,
            vec![
                MzLocationStep::DecodeJsonString,
                MzLocationStep::key("description"),
            ],
        );
        let assets = snapshot(vec![
            group(
                TextGroupKind::DatabaseEntry,
                data_group_location(StandardDataFile::Skills, 1),
                vec![scalar_leaf(
                    canonical.clone(),
                    "description",
                    "单行原文",
                    Some("较长帮助译文"),
                )],
            ),
            group(
                TextGroupKind::DatabaseEntry,
                data_group_location(StandardDataFile::States, 1),
                vec![scalar_leaf(
                    state,
                    "description",
                    "状态原文",
                    Some("状态译文"),
                )],
            ),
            group(
                TextGroupKind::DatabaseEntry,
                data_group_location(StandardDataFile::Items, 2),
                vec![scalar_leaf(
                    note,
                    "description",
                    "标签原文",
                    Some("标签译文"),
                )],
            ),
            group(
                TextGroupKind::PluginParameter,
                plugin_group,
                vec![scalar_leaf(
                    plugin,
                    "description",
                    "插件原文",
                    Some("插件译文"),
                )],
            ),
        ]);
        let harness = Harness::new(Ok(assets), FakeLayoutConfig::default(), None);
        let project = project();

        let report = harness
            .service
            .run(&project, project.layout_profile())
            .await
            .expect("非帮助 description 不应阻止写回");
        let (_, summary) = report.into_parts();
        let recorded = harness.recorded();

        assert_eq!(recorded.requests.len(), 1);
        assert_eq!(recorded.requests[0].unit_location(), &canonical);
        assert_eq!(
            recorded.requests[0].region(),
            MzWriteBackLayoutRegion::HelpDescription
        );
        assert!(!recorded.requests[0].auto_wrap_enabled());
        assert_eq!(summary.translated_locations, 4);
        assert_eq!(recorded.plans[0].mutations().len(), 4);
    }

    #[tokio::test]
    async fn manual_event_layout_keeps_raw_hard_lines_and_frozen_commands() {
        let group_location = command_location(3, 5);
        let translated = command_parameter_location(3, 6, 0);
        let frozen = command_parameter_location(3, 7, 0);
        let assets = snapshot(vec![group(
            TextGroupKind::EventDialogue,
            group_location.clone(),
            vec![
                body_leaf(
                    StandardWriteBackFieldRole::dialogue_body(0),
                    translated,
                    "原文一",
                    Some("\n甲\n"),
                ),
                body_leaf(
                    StandardWriteBackFieldRole::dialogue_body(1),
                    frozen,
                    "原文二",
                    None,
                ),
            ],
        )]);
        let harness = Harness::new(
            Ok(assets),
            FakeLayoutConfig {
                manual_regions: vec![MzWriteBackLayoutRegion::DialogueBody],
                ..FakeLayoutConfig::default()
            },
            None,
        );
        let project = project();

        let report = harness
            .service
            .run(&project, project.layout_profile())
            .await
            .expect("Manual 是正常写回结果");
        let (_, summary) = report.into_parts();
        let recorded = harness.recorded();

        assert_eq!(
            summary,
            StandardWriteBackSummary {
                translated_locations: 1,
                original_locations: 1,
                manual_layout_units: 1,
                ..StandardWriteBackSummary::default()
            }
        );
        let StandardWriteBackMutation::ReplaceEventBody(mutation) =
            &recorded.plans[0].mutations()[0]
        else {
            panic!("应生成整个对话正文块 Mutation")
        };
        assert_eq!(
            mutation.segments()[0].action(),
            &EventBodyMutationAction::ReplaceWithLines(vec![
                String::new(),
                "甲".to_owned(),
                String::new(),
            ])
        );
        assert_eq!(
            mutation.segments()[1].action(),
            &EventBodyMutationAction::KeepOriginal
        );
        assert_eq!(recorded.logs.len(), 1);
        assert_eq!(recorded.logs[0].manual_layout_diagnostics().len(), 1);
        let diagnostic = &recorded.logs[0].manual_layout_diagnostics()[0];
        assert_eq!(diagnostic.unit_location(), &group_location);
        assert_eq!(diagnostic.region(), MzWriteBackLayoutRegion::DialogueBody);
        assert_eq!(diagnostic.max_fullwidth_chars(), width(24));
        assert_eq!(recorded.stages.last(), Some(&Stage::Log));
    }

    #[tokio::test]
    async fn empty_snapshot_still_rewrites_publishes_and_records_one_run() {
        let harness = Harness::new(
            Ok(StandardWriteBackSnapshot::empty()),
            FakeLayoutConfig::default(),
            None,
        );
        let project = project();

        let report = harness
            .service
            .run(&project, project.layout_profile())
            .await
            .expect("空快照仍应发布完整冻结副本");
        let (_, summary) = report.into_parts();
        let recorded = harness.recorded();

        assert_eq!(summary, StandardWriteBackSummary::default());
        assert_eq!(
            recorded.stages,
            vec![
                Stage::Read,
                Stage::Rewrite,
                Stage::Publish(project.write_back_root().to_path_buf()),
                Stage::Log,
            ]
        );
        assert!(recorded.requests.is_empty());
        assert!(recorded.plans[0].mutations().is_empty());
        assert_eq!(recorded.logs.len(), 1);
    }

    #[tokio::test]
    async fn explicit_translation_equal_to_original_is_still_applied_and_counted() {
        let location = data_field_location(StandardDataFile::Actors, 1, "name");
        let assets = snapshot(vec![group(
            TextGroupKind::DatabaseEntry,
            data_group_location(StandardDataFile::Actors, 1),
            vec![scalar_leaf(location.clone(), "name", "勇者", Some("勇者"))],
        )]);
        let harness = Harness::new(Ok(assets), FakeLayoutConfig::default(), None);
        let project = project();

        let report = harness
            .service
            .run(&project, project.layout_profile())
            .await
            .expect("显式同文译文仍应应用");
        let (_, summary) = report.into_parts();
        let recorded = harness.recorded();

        assert_eq!(summary.translated_locations, 1);
        assert_eq!(summary.original_locations, 0);
        let StandardWriteBackMutation::SetText(mutation) = &recorded.plans[0].mutations()[0] else {
            panic!("普通字段应生成 SetText")
        };
        assert_eq!(mutation.exact_location(), &location);
        assert_eq!(mutation.expected_original(), "勇者");
        assert_eq!(mutation.replacement(), "勇者");
    }

    #[tokio::test]
    async fn every_technical_failure_stops_later_stages_and_preserves_its_source() {
        let cases = [
            ("read", vec![Stage::Read]),
            ("rewrite", vec![Stage::Read, Stage::Rewrite]),
            (
                "publish",
                vec![
                    Stage::Read,
                    Stage::Rewrite,
                    Stage::Publish(project().write_back_root().to_path_buf()),
                ],
            ),
            (
                "log",
                vec![
                    Stage::Read,
                    Stage::Rewrite,
                    Stage::Publish(project().write_back_root().to_path_buf()),
                    Stage::Log,
                ],
            ),
        ];

        for (stage, expected_stages) in cases {
            let snapshot = if stage == "read" {
                Err(FakeError("read"))
            } else {
                Ok(StandardWriteBackSnapshot::empty())
            };
            let harness = Harness::new(snapshot, FakeLayoutConfig::default(), Some(stage));
            let project = project();

            let error = harness
                .service
                .run(&project, project.layout_profile())
                .await
                .expect_err("指定阶段应技术失败");

            match stage {
                "read" => assert!(matches!(
                    error,
                    StandardWriteBackServiceError::ReadAssets(FakeError("read"))
                )),
                "rewrite" => assert!(matches!(
                    error,
                    StandardWriteBackServiceError::RewriteDocuments(FakeError("rewrite"))
                )),
                "publish" => assert!(matches!(
                    error,
                    StandardWriteBackServiceError::Publish(FakeError("publish"))
                )),
                "log" => {
                    assert!(matches!(
                        &error,
                        StandardWriteBackServiceError::RecordPublishedRun {
                            output_root,
                            source: FakeError("log"),
                        } if output_root == project.write_back_root()
                    ));
                    assert!(
                        error
                            .to_string()
                            .contains(&project.write_back_root().display().to_string())
                    );
                }
                _ => unreachable!("测试只包含已知失败阶段"),
            }
            assert_eq!(
                error.source().and_then(|source| source.downcast_ref()),
                Some(&FakeError(stage))
            );
            assert_eq!(harness.recorded().stages, expected_stages);
        }
    }

    #[test]
    fn snapshot_rejects_invalid_roles_indices_locations_and_duplicate_addresses() {
        let dialogue_group = command_location(1, 10);
        let invalid_role = StandardWriteBackLeaf::new(
            StandardWriteBackFieldRole::scalar("body[0]"),
            command_parameter_location(1, 11, 0),
            "原文",
            None,
        )
        .expect("叶本身合法");
        assert!(matches!(
            StandardWriteBackGroup::new(
                TextGroupKind::EventDialogue,
                dialogue_group.clone(),
                vec![invalid_role]
            ),
            Err(StandardWriteBackSnapshotError::InvalidRole { .. })
        ));

        let skipped_index = body_leaf(
            StandardWriteBackFieldRole::dialogue_body(1),
            command_parameter_location(1, 11, 0),
            "原文",
            None,
        );
        assert!(matches!(
            StandardWriteBackGroup::new(
                TextGroupKind::EventDialogue,
                dialogue_group.clone(),
                vec![skipped_index]
            ),
            Err(StandardWriteBackSnapshotError::NonContiguousBodyIndex {
                expected: 0,
                actual: 1,
                ..
            })
        ));

        let reversed = vec![
            body_leaf(
                StandardWriteBackFieldRole::dialogue_body(0),
                command_parameter_location(1, 12, 0),
                "第一行",
                None,
            ),
            body_leaf(
                StandardWriteBackFieldRole::dialogue_body(1),
                command_parameter_location(1, 11, 0),
                "第二行",
                None,
            ),
        ];
        assert!(matches!(
            StandardWriteBackGroup::new(TextGroupKind::EventDialogue, dialogue_group, reversed),
            Err(StandardWriteBackSnapshotError::BodyLocationOrder { .. })
        ));

        let duplicate = data_field_location(StandardDataFile::Items, 1, "name");
        let first = group(
            TextGroupKind::DatabaseEntry,
            data_group_location(StandardDataFile::Items, 1),
            vec![scalar_leaf(duplicate.clone(), "name", "甲", None)],
        );
        let second = group(
            TextGroupKind::DatabaseEntry,
            data_group_location(StandardDataFile::Items, 2),
            vec![scalar_leaf(duplicate.clone(), "name", "乙", None)],
        );
        assert_eq!(
            StandardWriteBackSnapshot::new(vec![first, second]),
            Err(StandardWriteBackSnapshotError::DuplicateLocation {
                exact_location: Box::new(duplicate),
            })
        );
    }

    #[test]
    fn leaf_and_snapshot_construction_preserve_values_but_reject_blank_facts() {
        let location = data_field_location(StandardDataFile::Items, 1, "name");
        assert!(matches!(
            StandardWriteBackLeaf::new(
                StandardWriteBackFieldRole::scalar(" "),
                location.clone(),
                "原文",
                None
            ),
            Err(StandardWriteBackSnapshotError::EmptyFieldName { .. })
        ));
        assert!(matches!(
            StandardWriteBackLeaf::new(
                StandardWriteBackFieldRole::scalar("name"),
                location.clone(),
                " \n ",
                None
            ),
            Err(StandardWriteBackSnapshotError::BlankOriginal { .. })
        ));
        assert!(matches!(
            StandardWriteBackLeaf::new(
                StandardWriteBackFieldRole::scalar("name"),
                location.clone(),
                "原文",
                Some(" \n ".to_owned())
            ),
            Err(StandardWriteBackSnapshotError::BlankTranslation { .. })
        ));

        let leaf = StandardWriteBackLeaf::new(
            StandardWriteBackFieldRole::scalar("name"),
            location,
            "  原文  ",
            Some("  译文\n  ".to_owned()),
        )
        .expect("非空首尾字符必须原样保留");
        assert_eq!(leaf.original_text(), "  原文  ");
        assert_eq!(leaf.translation(), Some("  译文\n  "));
        assert!(StandardWriteBackSnapshot::empty().groups().is_empty());
    }

    #[test]
    fn snapshot_normalizes_groups_and_leaves_into_stable_domain_order() {
        let later_group_location = data_group_location(StandardDataFile::Items, 2);
        let earlier_group_location = data_group_location(StandardDataFile::Items, 1);
        let later_field = data_field_location(StandardDataFile::Items, 2, "name");
        let earlier_description = data_field_location(StandardDataFile::Items, 1, "description");
        let earlier_name = data_field_location(StandardDataFile::Items, 1, "name");
        let snapshot = snapshot(vec![
            group(
                TextGroupKind::DatabaseEntry,
                later_group_location.clone(),
                vec![scalar_leaf(later_field, "name", "后", None)],
            ),
            group(
                TextGroupKind::DatabaseEntry,
                earlier_group_location.clone(),
                vec![
                    scalar_leaf(earlier_name.clone(), "name", "先名称", None),
                    scalar_leaf(earlier_description.clone(), "description", "先说明", None),
                ],
            ),
        ]);

        assert_eq!(
            snapshot.groups()[0].group_location(),
            &earlier_group_location
        );
        assert_eq!(snapshot.groups()[1].group_location(), &later_group_location);
        assert_eq!(
            snapshot.groups()[0]
                .leaves()
                .iter()
                .map(StandardWriteBackLeaf::exact_location)
                .collect::<Vec<_>>(),
            vec![&earlier_description, &earlier_name]
        );
    }

    #[test]
    fn applied_layout_rejects_every_violation_of_the_request_mapping() {
        let group_location = command_location(7, 10);
        let translated_location = command_parameter_location(7, 11, 0);
        let frozen_location = command_parameter_location(7, 12, 0);
        let unexpected_location = command_parameter_location(7, 13, 0);
        let translated = body_leaf(
            StandardWriteBackFieldRole::dialogue_body(0),
            translated_location.clone(),
            "译文叶原文",
            Some("数据库译文"),
        );
        let frozen = body_leaf(
            StandardWriteBackFieldRole::dialogue_body(1),
            frozen_location.clone(),
            "缺译原文",
            None,
        );
        let request = MzWriteBackLayoutRequest::new(
            group_location,
            MzWriteBackLayoutRegion::DialogueBody,
            width(24),
            true,
            vec![
                MzWriteBackLayoutSegment::from_leaf(&translated),
                MzWriteBackLayoutSegment::from_leaf(&frozen),
            ],
        );

        assert_eq!(
            MzWriteBackLaidOutSegment::new(translated_location.clone(), Vec::new()),
            Err(MzWriteBackAppliedLayoutError::EmptyReplacement {
                exact_location: Box::new(translated_location.clone()),
            })
        );
        assert_eq!(
            MzWriteBackLaidOutSegment::new(
                translated_location.clone(),
                vec!["仍含\n硬换行".to_owned()],
            ),
            Err(MzWriteBackAppliedLayoutError::EmbeddedLineBreak {
                exact_location: Box::new(translated_location.clone()),
                line_index: 0,
            })
        );

        let replacement = || {
            MzWriteBackLaidOutSegment::new(translated_location.clone(), vec!["布局译文".to_owned()])
                .expect("测试替换段应合法")
        };
        assert_eq!(
            MzWriteBackAppliedLayout::new(&request, Vec::new(), 0, 0),
            Err(MzWriteBackAppliedLayoutError::MissingReplacement {
                exact_location: Box::new(translated_location.clone()),
            })
        );
        assert_eq!(
            MzWriteBackAppliedLayout::new(&request, vec![replacement(), replacement()], 0, 0,),
            Err(MzWriteBackAppliedLayoutError::DuplicateReplacement {
                exact_location: Box::new(translated_location.clone()),
            })
        );
        assert_eq!(
            MzWriteBackAppliedLayout::new(
                &request,
                vec![
                    replacement(),
                    MzWriteBackLaidOutSegment::new(
                        frozen_location.clone(),
                        vec!["非法改动".to_owned()],
                    )
                    .expect("替换段形状本身合法"),
                ],
                0,
                0,
            ),
            Err(MzWriteBackAppliedLayoutError::ChangesFrozenOriginal {
                exact_location: Box::new(frozen_location),
            })
        );
        assert_eq!(
            MzWriteBackAppliedLayout::new(
                &request,
                vec![
                    replacement(),
                    MzWriteBackLaidOutSegment::new(
                        unexpected_location.clone(),
                        vec!["请求外".to_owned()],
                    )
                    .expect("替换段形状本身合法"),
                ],
                0,
                0,
            ),
            Err(MzWriteBackAppliedLayoutError::UnexpectedReplacement {
                exact_location: Box::new(unexpected_location),
            })
        );
    }

    #[test]
    fn applied_layout_normalizes_replacements_to_request_order() {
        let first_location = command_parameter_location(8, 11, 0);
        let second_location = command_parameter_location(8, 12, 0);
        let first = body_leaf(
            StandardWriteBackFieldRole::dialogue_body(0),
            first_location.clone(),
            "第一原文",
            Some("第一译文"),
        );
        let second = body_leaf(
            StandardWriteBackFieldRole::dialogue_body(1),
            second_location.clone(),
            "第二原文",
            Some("第二译文"),
        );
        let request = MzWriteBackLayoutRequest::new(
            command_location(8, 10),
            MzWriteBackLayoutRegion::DialogueBody,
            width(24),
            true,
            vec![
                MzWriteBackLayoutSegment::from_leaf(&first),
                MzWriteBackLayoutSegment::from_leaf(&second),
            ],
        );
        let applied = MzWriteBackAppliedLayout::new(
            &request,
            vec![
                MzWriteBackLaidOutSegment::new(second_location, vec!["第二译文".to_owned()])
                    .expect("替换段应合法"),
                MzWriteBackLaidOutSegment::new(first_location.clone(), vec!["第一译文".to_owned()])
                    .expect("替换段应合法"),
            ],
            0,
            0,
        )
        .expect("完整映射应被接受");

        assert_eq!(applied.segments()[0].exact_location(), &first_location);
    }

    #[test]
    fn mutation_plan_rejects_every_point_and_event_block_address_conflict() {
        let group_location = command_location(9, 10);
        let first_location = command_parameter_location(9, 11, 0);
        let second_location = command_parameter_location(9, 12, 0);
        let first_leaf = body_leaf(
            StandardWriteBackFieldRole::dialogue_body(0),
            first_location.clone(),
            "第一原文",
            Some("第一译文"),
        );
        let second_leaf = body_leaf(
            StandardWriteBackFieldRole::dialogue_body(0),
            second_location,
            "第二原文",
            Some("第二译文"),
        );
        let first_body = ReplaceEventBodyMutation::new(
            EventBodyKind::Dialogue,
            group_location.clone(),
            vec![EventBodyMutationSegment::replace(
                first_leaf.clone(),
                vec!["第一译文".to_owned()],
            )],
        )
        .expect("事件块应合法");
        let point = SetTextMutation::from_leaf(first_leaf, "冲突替换".to_owned());
        assert!(matches!(
            StandardWriteBackMutationPlan::new(vec![
                StandardWriteBackMutation::SetText(point),
                StandardWriteBackMutation::ReplaceEventBody(first_body.clone()),
            ]),
            Err(StandardWriteBackMutationPlanError::DuplicateLocation {
                exact_location,
            }) if exact_location == first_location
        ));

        let group_point = SetTextMutation::from_leaf(
            scalar_leaf(
                group_location.clone(),
                "synthetic",
                "起始命令",
                Some("非法点替换"),
            ),
            "非法点替换".to_owned(),
        );
        assert!(matches!(
            StandardWriteBackMutationPlan::new(vec![
                StandardWriteBackMutation::ReplaceEventBody(first_body.clone()),
                StandardWriteBackMutation::SetText(group_point),
            ]),
            Err(StandardWriteBackMutationPlanError::DuplicateLocation {
                exact_location,
            }) if exact_location == group_location
        ));

        let second_body = ReplaceEventBodyMutation::new(
            EventBodyKind::ScrollingText,
            group_location.clone(),
            vec![EventBodyMutationSegment::replace(
                second_leaf,
                vec!["第二译文".to_owned()],
            )],
        )
        .expect("第二事件块应合法");
        assert_eq!(
            StandardWriteBackMutationPlan::new(vec![
                StandardWriteBackMutation::ReplaceEventBody(first_body),
                StandardWriteBackMutation::ReplaceEventBody(second_body),
            ]),
            Err(StandardWriteBackMutationPlanError::DuplicateEventBody { group_location })
        );
    }

    #[tokio::test]
    async fn manual_help_layout_uses_raw_database_translation_and_exact_location() {
        let location = data_field_location(StandardDataFile::Weapons, 2, "description");
        let assets = snapshot(vec![group(
            TextGroupKind::DatabaseEntry,
            data_group_location(StandardDataFile::Weapons, 2),
            vec![scalar_leaf(
                location.clone(),
                "description",
                "原文第一行\n原文第二行",
                Some("译文硬边界\n保持原样"),
            )],
        )]);
        let harness = Harness::new(
            Ok(assets),
            FakeLayoutConfig {
                manual_regions: vec![MzWriteBackLayoutRegion::HelpDescription],
                ..FakeLayoutConfig::default()
            },
            None,
        );
        let project = project();

        let report = harness
            .service
            .run(&project, project.layout_profile())
            .await
            .expect("帮助框 Manual 仍应成功发布");
        let (_, summary) = report.into_parts();
        let recorded = harness.recorded();

        assert_eq!(summary.manual_layout_units, 1);
        assert_eq!(recorded.logs[0].manual_layout_diagnostics().len(), 1);
        assert_eq!(
            recorded.logs[0].manual_layout_diagnostics()[0].unit_location(),
            &location
        );
        let StandardWriteBackMutation::SetText(mutation) = &recorded.plans[0].mutations()[0] else {
            panic!("帮助说明应产生单值 Mutation")
        };
        assert_eq!(mutation.replacement(), "译文硬边界\n保持原样");
    }

    #[test]
    fn standard_write_back_future_remains_send() {
        fn assert_send(_: impl Send) {}

        let harness = Harness::new(
            Ok(StandardWriteBackSnapshot::empty()),
            FakeLayoutConfig::default(),
            None,
        );
        let project = project();
        assert_send(harness.service.run(&project, project.layout_profile()));
    }
}
