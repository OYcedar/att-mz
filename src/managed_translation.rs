//! 与具体游戏引擎无关的 Lua 托管翻译领域模型。
//!
//! 本模块只拥有 collection、unit、内容形状、metadata 与成对译文的不变量。
//! 引擎来源身份、项目快照、checkpoint 和持久化由各引擎适配边界负责。

mod kernel;
mod lua;

/// Managed `reflow` 在 user message 中使用的固定输入标记。
pub(crate) const MANAGED_REFLOW_WIRE_MARKER: &str = "single string, LF allowed";

/// 追加到具体引擎 Prompt 后的 Managed 机器协议。
///
/// 该片段由 Managed 模块维护，避免把只适用于 Managed shape 的模型协议复制到各 locale
/// 资源。具体引擎仍负责提供翻译方向、质量要求、共同使用的 JSON 信封与其他 shape 规则。
pub(crate) fn managed_translation_system_prompt_fragment() -> String {
    [
        "# ATT Managed translation extension\n\n",
        "This section applies only to ATT Managed translation TaskBlocks. ",
        "For the exact input marker `",
        MANAGED_REFLOW_WIRE_MARKER,
        "`, these rules override any shared ",
        "rule that forbids LF inside a JSON string:\n\n",
        "- Return the ID exactly once in the top-level JSON object. Its value must be an array ",
        "containing exactly one non-blank JSON string; never split the result into multiple array ",
        "elements.\n",
        "- An LF in the decoded string is allowed and must be encoded as `\\n` in the JSON text. ",
        "CR and NUL are forbidden.\n",
        "- ATT tokens may move only between LF-delimited segments within that same ID. They must ",
        "never move to another ID.\n",
        "- When a thinking requirement follows this section, the `<why>` analysis must cover this ",
        "marker's one-element shape, LF placement, and ATT-token placement.",
    ]
    .concat()
}

use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use crate::fingerprint::Sha256Fingerprint;
use crate::fingerprint::Sha256FramedHasher;
use crate::json::{self, StackSafeJsonError};
use crate::lua_host::TrustedLuaHostCallError;
pub(crate) use kernel::*;
pub(crate) use lua::*;

/// 一个托管翻译单元的原子内容形状。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum ManagedTranslationShape {
    Single,
    Reflow,
    Lines,
    Items,
}

impl ManagedTranslationShape {
    pub(crate) const fn storage_name(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Reflow => "reflow",
            Self::Lines => "lines",
            Self::Items => "items",
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        self.storage_name()
    }

    pub(crate) fn from_storage_name(value: &str) -> Option<Self> {
        match value {
            "single" => Some(Self::Single),
            "reflow" => Some(Self::Reflow),
            "lines" => Some(Self::Lines),
            "items" => Some(Self::Items),
            _ => None,
        }
    }

    const fn expects_scalar(self) -> bool {
        matches!(self, Self::Single | Self::Reflow)
    }
}

/// 标量或有序字符串数组；数组整体仍是一个翻译原子。
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ManagedTranslationContent {
    Scalar(String),
    Array(Vec<String>),
}

impl ManagedTranslationContent {
    pub(crate) fn scalar(value: impl Into<String>) -> Self {
        Self::Scalar(value.into())
    }

    pub(crate) fn array(values: Vec<String>) -> Self {
        Self::Array(values)
    }

    pub(crate) fn as_array(&self) -> Option<&[String]> {
        match self {
            Self::Scalar(_) => None,
            Self::Array(values) => Some(values),
        }
    }

    pub(crate) fn canonical_json(&self) -> String {
        match self {
            Self::Scalar(value) => {
                serde_json::to_string(value).expect("Rust 字符串必须可编码为 JSON")
            }
            Self::Array(values) => {
                serde_json::to_string(values).expect("Rust 字符串数组必须可编码为 JSON")
            }
        }
    }
}

/// Managed 候选正文违反的结构化不变量。
///
/// 该类型只描述 shape、正文和原文槽位之间的关系，不包含模型协议或具体引擎语义。
/// 模型响应、人工候选和持久化边界应复用同一校验器，再把本类型投影成各自的错误格式。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ManagedTranslationContentRejection {
    ContentShapeMismatch {
        shape: ManagedTranslationShape,
    },
    InvalidText {
        index: usize,
    },
    ItemCountMismatch {
        shape: ManagedTranslationShape,
        expected: usize,
        actual: usize,
    },
    BlankSlotMismatch {
        index: usize,
    },
    BlankItem {
        index: usize,
    },
    BlankTranslation,
}

impl ManagedTranslationContentRejection {
    pub(crate) const fn reason(&self) -> &'static str {
        match self {
            Self::ContentShapeMismatch { .. } => "content_shape_mismatch",
            Self::InvalidText { .. } => "invalid_line_text",
            Self::ItemCountMismatch { .. } => "item_count_mismatch",
            Self::BlankSlotMismatch { .. } => "blank_slot_mismatch",
            Self::BlankItem { .. } => "blank_item",
            Self::BlankTranslation => "blank_translation",
        }
    }

    /// 返回适合外部协议展示的一基项号。
    pub(crate) fn item_number(&self) -> Option<usize> {
        match self {
            Self::InvalidText { index }
            | Self::BlankSlotMismatch { index }
            | Self::BlankItem { index } => Some(index + 1),
            _ => None,
        }
    }

    pub(crate) const fn expected_actual(&self) -> Option<(usize, usize)> {
        match self {
            Self::ItemCountMismatch {
                expected, actual, ..
            } => Some((*expected, *actual)),
            _ => None,
        }
    }
}

/// Translate 语义对一段标量文本完成预处理后的稳定状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManagedPreparedTranslationStatus {
    Active,
    NonSourceLanguage,
    FullyProtected,
}

impl ManagedPreparedTranslationStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::NonSourceLanguage => "non_source_language",
            Self::FullyProtected => "fully_protected",
        }
    }
}

/// 当前预处理文本实际命中的一个有序术语对。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManagedTranslationTerm {
    term: String,
    translation: String,
}

impl ManagedTranslationTerm {
    pub(crate) fn new(term: impl Into<String>, translation: impl Into<String>) -> Self {
        Self {
            term: term.into(),
            translation: translation.into(),
        }
    }

    pub(crate) fn term(&self) -> &str {
        &self.term
    }

    pub(crate) fn translation(&self) -> &str {
        &self.translation
    }
}

/// 引擎语义对一个标量候选的正常验收结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ManagedPreparedTranslationAcceptance {
    Accepted {
        translation: String,
        state: Sha256Fingerprint,
    },
    Rejected {
        reason: String,
    },
}

impl ManagedPreparedTranslationAcceptance {
    pub(crate) fn accepted(translation: impl Into<String>, state: Sha256Fingerprint) -> Self {
        Self::Accepted {
            translation: translation.into(),
            state,
        }
    }

    pub(crate) fn rejected(reason: impl Into<String>) -> Self {
        Self::Rejected {
            reason: reason.into(),
        }
    }
}

/// 具体引擎建立、不可由扩展伪造的一段标量预处理语义。
pub(crate) trait ManagedPreparedTranslation: Send + Sync + 'static {
    fn status(&self) -> ManagedPreparedTranslationStatus;
    fn model_text(&self) -> &str;
    fn terms(&self) -> &[ManagedTranslationTerm];
    fn semantic_fingerprint(&self) -> Sha256Fingerprint;

    fn is_current(
        &self,
        translation: String,
        state: Sha256Fingerprint,
    ) -> Result<bool, TrustedLuaHostCallError>;

    fn accept(
        &self,
        candidate: String,
    ) -> Result<ManagedPreparedTranslationAcceptance, TrustedLuaHostCallError>;
}

/// 引擎适配器向 Managed 核心提供的翻译语义。
///
/// `kind` 保持调用引擎的稳定原始名称；本模块不解释具体引擎枚举。
pub(crate) trait ManagedTranslationSemantics: Send + Sync + 'static {
    fn engine_semantic_identity(&self) -> &str;
    fn system_prompt(&self) -> &str;
    fn source_language(&self) -> &str;
    fn target_language(&self) -> &str;

    fn prepare_translation(
        &self,
        kind: &str,
        shape: ManagedTranslationShape,
        original: &ManagedTranslationContent,
        semantic_context: &str,
    ) -> Result<Arc<dyn ManagedPreparedTranslation>, TrustedLuaHostCallError>;
}

#[derive(Clone)]
enum ManagedPreparedContentParts {
    Atomic(Arc<dyn ManagedPreparedTranslation>),
    Items(Vec<Arc<dyn ManagedPreparedTranslation>>),
}

/// 四种 Managed shape 共用的一次冻结预处理结果。
///
/// 具体引擎只准备标量语义；本类型负责 `single`、`reflow`、`lines` 与 `items` 的
/// shape 组合、逐槽验收和 content state。Lua 高级接口与自动 Managed 规划器应共同
/// 使用本类型，避免各自重新解释数组、LF 和非活动槽位。
#[derive(Clone)]
pub(crate) struct ManagedPreparedContent {
    kind: String,
    shape: ManagedTranslationShape,
    original: ManagedTranslationContent,
    semantic_context: String,
    parts: ManagedPreparedContentParts,
}

impl ManagedPreparedContent {
    pub(crate) fn prepare(
        semantics: &dyn ManagedTranslationSemantics,
        kind: &str,
        shape: ManagedTranslationShape,
        original: &ManagedTranslationContent,
        semantic_context: &str,
    ) -> Result<Self, ManagedPreparedContentError> {
        validate_original(shape, original).map_err(ManagedPreparedContentError::InvalidOriginal)?;
        let parts = match (shape, original) {
            (
                ManagedTranslationShape::Single | ManagedTranslationShape::Reflow,
                ManagedTranslationContent::Scalar(_),
            )
            | (ManagedTranslationShape::Lines, ManagedTranslationContent::Array(_)) => {
                ManagedPreparedContentParts::Atomic(
                    semantics
                        .prepare_translation(kind, shape, original, semantic_context)
                        .map_err(ManagedPreparedContentError::Semantics)?,
                )
            }
            (ManagedTranslationShape::Items, ManagedTranslationContent::Array(original)) => {
                let mut items = Vec::with_capacity(original.len());
                for (index, item) in original.iter().enumerate() {
                    items.push(
                        semantics
                            .prepare_translation(
                                kind,
                                ManagedTranslationShape::Items,
                                &ManagedTranslationContent::scalar(item),
                                &format!("{semantic_context}\nitem_index={index}"),
                            )
                            .map_err(ManagedPreparedContentError::Semantics)?,
                    );
                }
                ManagedPreparedContentParts::Items(items)
            }
            _ => unreachable!("原文 shape 已验证"),
        };
        Ok(Self {
            kind: kind.to_owned(),
            shape,
            original: original.clone(),
            semantic_context: semantic_context.to_owned(),
            parts,
        })
    }

    pub(crate) const fn shape(&self) -> ManagedTranslationShape {
        self.shape
    }

    pub(crate) fn is_active(&self) -> bool {
        match &self.parts {
            ManagedPreparedContentParts::Atomic(prepared) => {
                prepared.status() == ManagedPreparedTranslationStatus::Active
            }
            ManagedPreparedContentParts::Items(items) => items
                .iter()
                .any(|prepared| prepared.status() == ManagedPreparedTranslationStatus::Active),
        }
    }

    /// 按稳定槽位顺序返回底层语义状态。
    ///
    /// `single`、`reflow` 与 `lines` 只有一个整体状态；`items` 每个槽位各有一个状态。
    pub(crate) fn part_statuses(&self) -> Vec<ManagedPreparedTranslationStatus> {
        match &self.parts {
            ManagedPreparedContentParts::Atomic(prepared) => vec![prepared.status()],
            ManagedPreparedContentParts::Items(items) => {
                items.iter().map(|prepared| prepared.status()).collect()
            }
        }
    }

    pub(crate) fn model_content(&self) -> ManagedTranslationContent {
        match &self.parts {
            ManagedPreparedContentParts::Atomic(prepared) => match self.shape {
                ManagedTranslationShape::Single | ManagedTranslationShape::Reflow => {
                    ManagedTranslationContent::scalar(prepared.model_text())
                }
                ManagedTranslationShape::Lines => ManagedTranslationContent::array(
                    prepared
                        .model_text()
                        .split('\n')
                        .map(str::to_owned)
                        .collect(),
                ),
                ManagedTranslationShape::Items => unreachable!("items 使用逐项语义"),
            },
            ManagedPreparedContentParts::Items(items) => ManagedTranslationContent::array(
                items
                    .iter()
                    .map(|prepared| prepared.model_text().to_owned())
                    .collect(),
            ),
        }
    }

    pub(crate) fn terms(&self) -> Vec<ManagedTranslationTerm> {
        let mut seen = HashSet::new();
        let mut terms = Vec::new();
        self.for_each_part(|prepared| {
            for term in prepared.terms() {
                if seen.insert((term.term().to_owned(), term.translation().to_owned())) {
                    terms.push(term.clone());
                }
            }
        });
        terms
    }

    /// 使用 Managed content state 判断候选是否属于当前冻结语义。
    ///
    /// 该状态与旧的标量 Lua state 属于不同命名空间；底层标量接口的可观察行为不变。
    pub(crate) fn is_current(
        &self,
        translation: &ManagedTranslationContent,
        state: Sha256Fingerprint,
    ) -> Result<bool, ManagedTranslationContentRejection> {
        validate_translation_content(self.shape, &self.original, translation)?;
        Ok(self.content_state(translation) == state)
    }

    pub(crate) fn accept(
        &self,
        candidate: ManagedTranslationContent,
    ) -> Result<ManagedPreparedContentAcceptance, TrustedLuaHostCallError> {
        if let Err(rejection) =
            validate_managed_translation_candidate(self.shape, &self.original, &candidate)
        {
            return Ok(ManagedPreparedContentAcceptance::rejected(
                ManagedPreparedContentRejection::Structure(rejection),
            ));
        }
        let accepted = match (&self.parts, candidate) {
            (
                ManagedPreparedContentParts::Atomic(prepared),
                ManagedTranslationContent::Scalar(candidate),
            ) => match prepared.accept(candidate)? {
                ManagedPreparedTranslationAcceptance::Accepted { translation, .. } => {
                    ManagedTranslationContent::scalar(translation)
                }
                ManagedPreparedTranslationAcceptance::Rejected { reason } => {
                    return Ok(ManagedPreparedContentAcceptance::rejected(
                        ManagedPreparedContentRejection::Semantics { reason },
                    ));
                }
            },
            (
                ManagedPreparedContentParts::Atomic(prepared),
                ManagedTranslationContent::Array(candidate),
            ) => {
                let joined = candidate.join("\n");
                match prepared.accept(joined)? {
                    ManagedPreparedTranslationAcceptance::Accepted { translation, .. } => {
                        ManagedTranslationContent::array(
                            translation.split('\n').map(str::to_owned).collect(),
                        )
                    }
                    ManagedPreparedTranslationAcceptance::Rejected { reason } => {
                        return Ok(ManagedPreparedContentAcceptance::rejected(
                            ManagedPreparedContentRejection::Semantics { reason },
                        ));
                    }
                }
            }
            (
                ManagedPreparedContentParts::Items(prepared_items),
                ManagedTranslationContent::Array(candidate),
            ) => {
                let originals = self.original.as_array().expect("items 原文必须是数组");
                let mut accepted = Vec::with_capacity(candidate.len());
                for (index, ((prepared, candidate), original)) in prepared_items
                    .iter()
                    .zip(candidate)
                    .zip(originals)
                    .enumerate()
                {
                    if prepared.status() == ManagedPreparedTranslationStatus::Active {
                        match prepared.accept(candidate)? {
                            ManagedPreparedTranslationAcceptance::Accepted {
                                translation, ..
                            } => accepted.push(translation),
                            ManagedPreparedTranslationAcceptance::Rejected { reason } => {
                                return Ok(ManagedPreparedContentAcceptance::rejected(
                                    ManagedPreparedContentRejection::Semantics { reason },
                                ));
                            }
                        }
                    } else if candidate == prepared.model_text() {
                        accepted.push(original.clone());
                    } else {
                        return Ok(ManagedPreparedContentAcceptance::rejected(
                            ManagedPreparedContentRejection::InactiveItemChanged { index },
                        ));
                    }
                }
                ManagedTranslationContent::array(accepted)
            }
            (ManagedPreparedContentParts::Items(_), ManagedTranslationContent::Scalar(_)) => {
                unreachable!("候选 shape 已验证")
            }
        };
        if let Err(rejection) =
            validate_managed_translation_candidate(self.shape, &self.original, &accepted)
        {
            return Ok(ManagedPreparedContentAcceptance::rejected(
                ManagedPreparedContentRejection::Structure(rejection),
            ));
        }
        let state = self.content_state(&accepted);
        Ok(ManagedPreparedContentAcceptance::accepted(accepted, state))
    }

    /// 把模型协议的字符串数组转换成领域正文后执行同一验收。
    pub(crate) fn accept_wire_values(
        &self,
        values: &[String],
    ) -> Result<ManagedPreparedContentAcceptance, TrustedLuaHostCallError> {
        if let Some(index) = values.iter().position(|value| {
            value.contains(['\0', '\r'])
                || (self.shape != ManagedTranslationShape::Reflow && value.contains('\n'))
        }) {
            return Ok(ManagedPreparedContentAcceptance::rejected(
                ManagedPreparedContentRejection::Structure(
                    ManagedTranslationContentRejection::InvalidText { index },
                ),
            ));
        }
        let candidate = match self.shape {
            ManagedTranslationShape::Single | ManagedTranslationShape::Reflow => {
                if values.len() != 1 {
                    return Ok(ManagedPreparedContentAcceptance::rejected(
                        ManagedPreparedContentRejection::Structure(
                            ManagedTranslationContentRejection::ItemCountMismatch {
                                shape: self.shape,
                                expected: 1,
                                actual: values.len(),
                            },
                        ),
                    ));
                }
                ManagedTranslationContent::scalar(values[0].clone())
            }
            ManagedTranslationShape::Lines | ManagedTranslationShape::Items => {
                ManagedTranslationContent::array(values.to_vec())
            }
        };
        self.accept(candidate)
    }

    fn for_each_part(&self, mut visit: impl FnMut(&dyn ManagedPreparedTranslation)) {
        match &self.parts {
            ManagedPreparedContentParts::Atomic(prepared) => visit(prepared.as_ref()),
            ManagedPreparedContentParts::Items(items) => {
                for prepared in items {
                    visit(prepared.as_ref());
                }
            }
        }
    }

    /// 保持既有自动 Managed state context 的 part framing。
    pub(crate) fn frame_automatic_state_context(&self, hasher: &mut Sha256FramedHasher) {
        match &self.parts {
            ManagedPreparedContentParts::Atomic(prepared) => {
                hasher
                    .frame(9, b"atomic")
                    .frame(10, prepared.semantic_fingerprint().as_bytes());
            }
            ManagedPreparedContentParts::Items(items) => {
                hasher.frame(9, b"items");
                for prepared in items {
                    hasher.frame(10, prepared.semantic_fingerprint().as_bytes());
                }
            }
        }
    }

    fn content_state(&self, content: &ManagedTranslationContent) -> Sha256Fingerprint {
        let mut hasher = Sha256FramedHasher::new(b"att.managed_translation.prepared_content_state");
        hasher
            .frame(1, self.shape.storage_name().as_bytes())
            .frame(2, self.kind.as_bytes())
            .frame(3, self.original.canonical_json().as_bytes())
            .frame(4, self.semantic_context.as_bytes());
        self.for_each_part(|prepared| {
            hasher.frame(5, prepared.semantic_fingerprint().as_bytes());
        });
        hasher.frame(6, content.canonical_json().as_bytes());
        hasher.finish()
    }
}

#[derive(Debug)]
pub(crate) enum ManagedPreparedContentError {
    InvalidOriginal(ManagedTranslationModelError),
    Semantics(TrustedLuaHostCallError),
}

impl fmt::Display for ManagedPreparedContentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidOriginal(source) => {
                write!(formatter, "Managed 原文不符合 shape：{source}")
            }
            Self::Semantics(source) => write!(formatter, "Managed 翻译语义准备失败：{source}"),
        }
    }
}

impl Error for ManagedPreparedContentError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidOriginal(source) => Some(source),
            Self::Semantics(source) => Some(source),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ManagedPreparedContentAcceptance {
    Accepted {
        content: ManagedTranslationContent,
        state: Sha256Fingerprint,
    },
    Rejected {
        rejection: ManagedPreparedContentRejection,
    },
}

impl ManagedPreparedContentAcceptance {
    pub(crate) fn accepted(content: ManagedTranslationContent, state: Sha256Fingerprint) -> Self {
        Self::Accepted { content, state }
    }

    pub(crate) fn rejected(rejection: ManagedPreparedContentRejection) -> Self {
        Self::Rejected { rejection }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ManagedPreparedContentRejection {
    Structure(ManagedTranslationContentRejection),
    Semantics { reason: String },
    InactiveItemChanged { index: usize },
}

impl ManagedPreparedContentRejection {
    pub(crate) fn reason(&self) -> &str {
        match self {
            Self::Structure(rejection) => rejection.reason(),
            Self::Semantics { reason } => reason,
            Self::InactiveItemChanged { .. } => "inactive_item_changed",
        }
    }

    pub(crate) fn item_number(&self) -> Option<usize> {
        match self {
            Self::Structure(rejection) => rejection.item_number(),
            Self::InactiveItemChanged { index } => Some(index + 1),
            Self::Semantics { .. } => None,
        }
    }

    pub(crate) const fn expected_actual(&self) -> Option<(usize, usize)> {
        match self {
            Self::Structure(rejection) => rejection.expected_actual(),
            Self::Semantics { .. } | Self::InactiveItemChanged { .. } => None,
        }
    }
}

/// 已由 JSON Host 建立、且不被 ATT 解释的任意 metadata JSON 值。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManagedTranslationMetadata {
    canonical_json: String,
}

impl ManagedTranslationMetadata {
    pub(crate) fn from_canonical_json(
        canonical_json: impl Into<String>,
    ) -> Result<Self, ManagedTranslationModelError> {
        let canonical_json = canonical_json.into();
        let value = json::from_str(&canonical_json)
            .map_err(ManagedTranslationModelError::InvalidMetadataJson)?;
        let encoded =
            json::to_string(&value).map_err(ManagedTranslationModelError::EncodeMetadataJson)?;
        if encoded != canonical_json {
            return Err(ManagedTranslationModelError::NonCanonicalMetadataJson);
        }
        Ok(Self { canonical_json })
    }

    pub(crate) fn canonical_json(&self) -> &str {
        &self.canonical_json
    }
}

/// 一个译文及其精确语义状态；两者不可拆分。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManagedTranslationPair {
    content: ManagedTranslationContent,
    state: Sha256Fingerprint,
}

impl ManagedTranslationPair {
    pub(crate) fn content(&self) -> &ManagedTranslationContent {
        &self.content
    }

    pub(crate) const fn state(&self) -> Sha256Fingerprint {
        self.state
    }

    pub(crate) fn new_trusted(
        content: ManagedTranslationContent,
        state: Sha256Fingerprint,
    ) -> Self {
        Self { content, state }
    }
}

/// 一个 collection 内具有稳定 key 和自然顺序的翻译单位。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManagedTranslationUnit {
    pub(crate) key: String,
    pub(crate) kind: String,
    pub(crate) shape: ManagedTranslationShape,
    pub(crate) original: ManagedTranslationContent,
    pub(crate) context: String,
    pub(crate) metadata: Option<ManagedTranslationMetadata>,
    pub(crate) translation: Option<ManagedTranslationPair>,
}

impl ManagedTranslationUnit {
    pub(crate) fn new(
        key: impl Into<String>,
        kind: impl Into<String>,
        shape: ManagedTranslationShape,
        original: ManagedTranslationContent,
        context: impl Into<String>,
        metadata: Option<ManagedTranslationMetadata>,
    ) -> Result<Self, ManagedTranslationModelError> {
        let key = key.into();
        let kind = kind.into();
        validate_identity("unit.key", &key)?;
        validate_identity("unit.kind", &kind)?;
        validate_original(shape, &original)?;
        Ok(Self {
            key,
            kind,
            shape,
            original,
            context: context.into(),
            metadata,
            translation: None,
        })
    }

    pub(crate) fn key(&self) -> &str {
        &self.key
    }

    pub(crate) fn kind(&self) -> &str {
        &self.kind
    }

    pub(crate) const fn shape(&self) -> ManagedTranslationShape {
        self.shape
    }

    pub(crate) fn original(&self) -> &ManagedTranslationContent {
        &self.original
    }

    pub(crate) fn context(&self) -> &str {
        &self.context
    }

    pub(crate) fn metadata(&self) -> Option<&ManagedTranslationMetadata> {
        self.metadata.as_ref()
    }

    pub(crate) fn translation(&self) -> Option<&ManagedTranslationPair> {
        self.translation.as_ref()
    }

    /// 按本单元 shape 和原文建立可提交的成对译文。
    pub(crate) fn translation_pair(
        &self,
        content: ManagedTranslationContent,
        state: Sha256Fingerprint,
    ) -> Result<ManagedTranslationPair, ManagedTranslationModelError> {
        validate_translation(self.shape, &self.original, &content)?;
        Ok(ManagedTranslationPair::new_trusted(content, state))
    }

    pub(crate) fn with_stored_translation(
        mut self,
        translation: Option<ManagedTranslationPair>,
    ) -> Result<Self, ManagedTranslationModelError> {
        if let Some(translation) = &translation {
            validate_translation(self.shape, &self.original, translation.content())?;
        }
        self.translation = translation;
        Ok(self)
    }
}

/// 共享一条任务 instruction、但保持独立原子单位的声明集合。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManagedTranslationCollection {
    pub(crate) name: String,
    pub(crate) instruction: String,
    pub(crate) units: Vec<ManagedTranslationUnit>,
}

impl ManagedTranslationCollection {
    pub(crate) fn new(
        name: impl Into<String>,
        instruction: impl Into<String>,
        units: Vec<ManagedTranslationUnit>,
    ) -> Result<Self, ManagedTranslationModelError> {
        let name = name.into();
        validate_identity("collection.name", &name)?;
        let mut keys = HashSet::with_capacity(units.len());
        for unit in &units {
            if !keys.insert(unit.key().to_owned()) {
                return Err(ManagedTranslationModelError::DuplicateUnitKey {
                    collection: name,
                    key: unit.key().to_owned(),
                });
            }
        }
        Ok(Self {
            name,
            instruction: instruction.into(),
            units,
        })
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn instruction(&self) -> &str {
        &self.instruction
    }

    pub(crate) fn units(&self) -> &[ManagedTranslationUnit] {
        &self.units
    }

    pub(crate) fn unit(&self, key: &str) -> Option<&ManagedTranslationUnit> {
        self.units.iter().find(|unit| unit.key == key)
    }
}

fn validate_identity(field: &'static str, value: &str) -> Result<(), ManagedTranslationModelError> {
    if value.trim().is_empty() {
        Err(ManagedTranslationModelError::BlankIdentity { field })
    } else {
        Ok(())
    }
}

fn validate_original(
    shape: ManagedTranslationShape,
    content: &ManagedTranslationContent,
) -> Result<(), ManagedTranslationModelError> {
    validate_content_shape(shape, content)?;
    match (shape, content) {
        (ManagedTranslationShape::Single, ManagedTranslationContent::Scalar(value)) => {
            validate_line(value, "original", 0)?;
        }
        (ManagedTranslationShape::Reflow, ManagedTranslationContent::Scalar(value)) => {
            if value.contains('\0') || value.contains('\r') {
                return Err(ManagedTranslationModelError::InvalidContentText {
                    role: "original",
                    index: 0,
                });
            }
        }
        (ManagedTranslationShape::Lines, ManagedTranslationContent::Array(values)) => {
            if values.is_empty() {
                return Err(ManagedTranslationModelError::EmptyArray {
                    shape,
                    role: "original",
                });
            }
            for (index, value) in values.iter().enumerate() {
                validate_line(value, "original", index)?;
            }
        }
        (ManagedTranslationShape::Items, ManagedTranslationContent::Array(values)) => {
            if values.is_empty() {
                return Err(ManagedTranslationModelError::EmptyArray {
                    shape,
                    role: "original",
                });
            }
            for (index, value) in values.iter().enumerate() {
                validate_line(value, "original", index)?;
                if value.trim().is_empty() {
                    return Err(ManagedTranslationModelError::BlankItem {
                        role: "original",
                        index,
                    });
                }
            }
        }
        _ => unreachable!("内容形状已验证"),
    }
    Ok(())
}

pub(crate) fn validate_translation(
    shape: ManagedTranslationShape,
    original: &ManagedTranslationContent,
    translation: &ManagedTranslationContent,
) -> Result<(), ManagedTranslationModelError> {
    validate_translation_content(shape, original, translation).map_err(
        |rejection| match rejection {
            ManagedTranslationContentRejection::ContentShapeMismatch { shape } => {
                ManagedTranslationModelError::ContentShapeMismatch { shape }
            }
            ManagedTranslationContentRejection::InvalidText { index } => {
                ManagedTranslationModelError::InvalidContentText {
                    role: "translation",
                    index,
                }
            }
            ManagedTranslationContentRejection::ItemCountMismatch {
                shape,
                expected,
                actual,
            } => ManagedTranslationModelError::TranslationItemCountMismatch {
                shape,
                expected,
                actual,
            },
            ManagedTranslationContentRejection::BlankSlotMismatch { index } => {
                ManagedTranslationModelError::BlankSlotMismatch { index }
            }
            ManagedTranslationContentRejection::BlankItem { index } => {
                ManagedTranslationModelError::BlankItem {
                    role: "translation",
                    index,
                }
            }
            ManagedTranslationContentRejection::BlankTranslation => {
                unreachable!("持久化译文校验保持既有标量空白行为")
            }
        },
    )
}

fn validate_content_shape(
    shape: ManagedTranslationShape,
    content: &ManagedTranslationContent,
) -> Result<(), ManagedTranslationModelError> {
    let scalar = matches!(content, ManagedTranslationContent::Scalar(_));
    if scalar == shape.expects_scalar() {
        Ok(())
    } else {
        Err(ManagedTranslationModelError::ContentShapeMismatch { shape })
    }
}

/// 校验一个准备进入模型响应或人工候选验收的完整 Managed 正文。
///
/// 与持久化模型相比，候选额外拒绝纯空白的 `single`/`reflow` 标量；这是现有自动
/// Managed 的候选行为，不会改变低级标量 `prepare/accept` 接口。
pub(crate) fn validate_managed_translation_candidate(
    shape: ManagedTranslationShape,
    original: &ManagedTranslationContent,
    candidate: &ManagedTranslationContent,
) -> Result<(), ManagedTranslationContentRejection> {
    validate_translation_content_with_policy(shape, original, candidate, true)
}

fn validate_translation_content(
    shape: ManagedTranslationShape,
    original: &ManagedTranslationContent,
    translation: &ManagedTranslationContent,
) -> Result<(), ManagedTranslationContentRejection> {
    validate_translation_content_with_policy(shape, original, translation, false)
}

fn validate_translation_content_with_policy(
    shape: ManagedTranslationShape,
    original: &ManagedTranslationContent,
    translation: &ManagedTranslationContent,
    reject_blank_scalar: bool,
) -> Result<(), ManagedTranslationContentRejection> {
    if validate_content_shape(shape, translation).is_err() {
        return Err(ManagedTranslationContentRejection::ContentShapeMismatch { shape });
    }
    match (shape, original, translation) {
        (
            ManagedTranslationShape::Single,
            ManagedTranslationContent::Scalar(_),
            ManagedTranslationContent::Scalar(value),
        ) => {
            validate_candidate_text(value, false, 0)?;
            if reject_blank_scalar && value.trim().is_empty() {
                return Err(ManagedTranslationContentRejection::BlankTranslation);
            }
        }
        (
            ManagedTranslationShape::Reflow,
            ManagedTranslationContent::Scalar(_),
            ManagedTranslationContent::Scalar(value),
        ) => {
            validate_candidate_text(value, true, 0)?;
            if reject_blank_scalar && value.trim().is_empty() {
                return Err(ManagedTranslationContentRejection::BlankTranslation);
            }
        }
        (
            ManagedTranslationShape::Lines,
            ManagedTranslationContent::Array(original),
            ManagedTranslationContent::Array(translation),
        ) => {
            validate_candidate_array_count(shape, original, translation)?;
            for (index, (original, translation)) in original.iter().zip(translation).enumerate() {
                validate_candidate_text(translation, false, index)?;
                if original.trim().is_empty() != translation.trim().is_empty() {
                    return Err(ManagedTranslationContentRejection::BlankSlotMismatch { index });
                }
            }
        }
        (
            ManagedTranslationShape::Items,
            ManagedTranslationContent::Array(original),
            ManagedTranslationContent::Array(translation),
        ) => {
            validate_candidate_array_count(shape, original, translation)?;
            for (index, translation) in translation.iter().enumerate() {
                validate_candidate_text(translation, false, index)?;
                if translation.trim().is_empty() {
                    return Err(ManagedTranslationContentRejection::BlankItem { index });
                }
            }
        }
        _ => unreachable!("原文和候选 shape 已验证"),
    }
    Ok(())
}

fn validate_candidate_array_count(
    shape: ManagedTranslationShape,
    original: &[String],
    translation: &[String],
) -> Result<(), ManagedTranslationContentRejection> {
    if original.len() != translation.len() {
        Err(ManagedTranslationContentRejection::ItemCountMismatch {
            shape,
            expected: original.len(),
            actual: translation.len(),
        })
    } else {
        Ok(())
    }
}

fn validate_candidate_text(
    value: &str,
    allow_lf: bool,
    index: usize,
) -> Result<(), ManagedTranslationContentRejection> {
    if value.contains(['\0', '\r']) || (!allow_lf && value.contains('\n')) {
        Err(ManagedTranslationContentRejection::InvalidText { index })
    } else {
        Ok(())
    }
}

fn validate_line(
    value: &str,
    role: &'static str,
    index: usize,
) -> Result<(), ManagedTranslationModelError> {
    if value.contains(['\0', '\r', '\n']) {
        Err(ManagedTranslationModelError::InvalidContentText { role, index })
    } else {
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) enum ManagedTranslationModelError {
    BlankIdentity {
        field: &'static str,
    },
    DuplicateCollectionName(String),
    DuplicateUnitKey {
        collection: String,
        key: String,
    },
    ContentShapeMismatch {
        shape: ManagedTranslationShape,
    },
    EmptyArray {
        shape: ManagedTranslationShape,
        role: &'static str,
    },
    InvalidContentText {
        role: &'static str,
        index: usize,
    },
    BlankItem {
        role: &'static str,
        index: usize,
    },
    TranslationItemCountMismatch {
        shape: ManagedTranslationShape,
        expected: usize,
        actual: usize,
    },
    BlankSlotMismatch {
        index: usize,
    },
    InvalidMetadataJson(StackSafeJsonError),
    EncodeMetadataJson(StackSafeJsonError),
    NonCanonicalMetadataJson,
    ManifestFingerprintMismatch {
        stored: Sha256Fingerprint,
        calculated: Sha256Fingerprint,
    },
    UnknownCheckpointIdentity {
        collection: String,
        key: String,
    },
    DuplicateCheckpointIdentity {
        collection: String,
        key: String,
    },
    CheckpointSnapshotMismatch,
    CheckpointExpectedTranslationMismatch {
        collection: String,
        key: String,
    },
}

impl fmt::Display for ManagedTranslationModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BlankIdentity { field } => write!(formatter, "{field} 不能为空或纯空白"),
            Self::DuplicateCollectionName(name) => {
                write!(formatter, "托管翻译 collection 名称重复：{name}")
            }
            Self::DuplicateUnitKey { collection, key } => {
                write!(formatter, "collection {collection} 的 unit key 重复：{key}")
            }
            Self::ContentShapeMismatch { shape } => {
                write!(
                    formatter,
                    "内容类型与 shape {} 不匹配",
                    shape.storage_name()
                )
            }
            Self::EmptyArray { shape, role } => write!(
                formatter,
                "{role} 的 {} 数组必须至少包含一个元素",
                shape.storage_name()
            ),
            Self::InvalidContentText { role, index } => {
                write!(
                    formatter,
                    "{role} 的第 {index} 项包含该 shape 不允许的控制字符"
                )
            }
            Self::BlankItem { role, index } => {
                write!(formatter, "{role} 的 items 第 {index} 项不能为空或纯空白")
            }
            Self::TranslationItemCountMismatch {
                shape,
                expected,
                actual,
            } => write!(
                formatter,
                "{} 译文项数必须保持为 {expected}，实际为 {actual}",
                shape.storage_name()
            ),
            Self::BlankSlotMismatch { index } => {
                write!(formatter, "lines 译文改变了第 {index} 项的空槽状态")
            }
            Self::InvalidMetadataJson(source) => write!(formatter, "metadata JSON 无效：{source}"),
            Self::EncodeMetadataJson(source) => {
                write!(formatter, "metadata JSON 无法编码：{source}")
            }
            Self::NonCanonicalMetadataJson => {
                formatter.write_str("metadata 必须使用紧凑 canonical JSON")
            }
            Self::ManifestFingerprintMismatch { stored, calculated } => write!(
                formatter,
                "托管翻译 manifest 指纹不一致：stored={stored:?}, calculated={calculated:?}"
            ),
            Self::UnknownCheckpointIdentity { collection, key } => {
                write!(formatter, "checkpoint 引用了未知 unit：{collection}/{key}")
            }
            Self::DuplicateCheckpointIdentity { collection, key } => {
                write!(formatter, "checkpoint 重复修改 unit：{collection}/{key}")
            }
            Self::CheckpointSnapshotMismatch => {
                formatter.write_str("checkpoint 与待投影的托管翻译快照不一致")
            }
            Self::CheckpointExpectedTranslationMismatch { collection, key } => write!(
                formatter,
                "checkpoint 的预期 translation/state 与快照不一致：{collection}/{key}"
            ),
        }
    }
}

impl Error for ManagedTranslationModelError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidMetadataJson(source) | Self::EncodeMetadataJson(source) => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakePrepared {
        status: ManagedPreparedTranslationStatus,
        model_text: String,
        fingerprint: Sha256Fingerprint,
        terms: Vec<ManagedTranslationTerm>,
    }

    impl ManagedPreparedTranslation for FakePrepared {
        fn status(&self) -> ManagedPreparedTranslationStatus {
            self.status
        }

        fn model_text(&self) -> &str {
            &self.model_text
        }

        fn terms(&self) -> &[ManagedTranslationTerm] {
            &self.terms
        }

        fn semantic_fingerprint(&self) -> Sha256Fingerprint {
            self.fingerprint
        }

        fn is_current(
            &self,
            translation: String,
            state: Sha256Fingerprint,
        ) -> Result<bool, TrustedLuaHostCallError> {
            let mut hasher = Sha256FramedHasher::new(b"att.test.scalar_state");
            hasher.frame(1, translation.as_bytes());
            Ok(hasher.finish() == state)
        }

        fn accept(
            &self,
            candidate: String,
        ) -> Result<ManagedPreparedTranslationAcceptance, TrustedLuaHostCallError> {
            if self.status != ManagedPreparedTranslationStatus::Active {
                return Ok(ManagedPreparedTranslationAcceptance::rejected(
                    self.status.as_str(),
                ));
            }
            let translation = candidate.replace("__ATT__", "\\V[1]");
            let mut hasher = Sha256FramedHasher::new(b"att.test.scalar_state");
            hasher.frame(1, translation.as_bytes());
            Ok(ManagedPreparedTranslationAcceptance::accepted(
                translation,
                hasher.finish(),
            ))
        }
    }

    struct FakeSemantics;

    impl ManagedTranslationSemantics for FakeSemantics {
        fn engine_semantic_identity(&self) -> &str {
            "test"
        }

        fn system_prompt(&self) -> &str {
            "system"
        }

        fn source_language(&self) -> &str {
            "ja"
        }

        fn target_language(&self) -> &str {
            "zh-Hans"
        }

        fn prepare_translation(
            &self,
            kind: &str,
            shape: ManagedTranslationShape,
            original: &ManagedTranslationContent,
            semantic_context: &str,
        ) -> Result<Arc<dyn ManagedPreparedTranslation>, TrustedLuaHostCallError> {
            let (status, model_text) = match original {
                ManagedTranslationContent::Scalar(value) if value == "protected" => (
                    ManagedPreparedTranslationStatus::FullyProtected,
                    "__PROTECTED__".to_owned(),
                ),
                ManagedTranslationContent::Scalar(value) if value == "non_source" => (
                    ManagedPreparedTranslationStatus::NonSourceLanguage,
                    value.clone(),
                ),
                ManagedTranslationContent::Scalar(value) => (
                    ManagedPreparedTranslationStatus::Active,
                    if value == "active" {
                        "__ATT__".to_owned()
                    } else {
                        value.clone()
                    },
                ),
                ManagedTranslationContent::Array(values) => {
                    (ManagedPreparedTranslationStatus::Active, values.join("\n"))
                }
            };
            let mut hasher = Sha256FramedHasher::new(b"att.test.prepared_semantics");
            hasher
                .frame(1, kind.as_bytes())
                .frame(2, shape.storage_name().as_bytes())
                .frame(3, original.canonical_json().as_bytes())
                .frame(4, semantic_context.as_bytes());
            Ok(Arc::new(FakePrepared {
                status,
                model_text,
                fingerprint: hasher.finish(),
                terms: vec![ManagedTranslationTerm::new("勇者", "Hero")],
            }))
        }
    }

    fn scalar_unit(key: &str) -> ManagedTranslationUnit {
        ManagedTranslationUnit::new(
            key,
            "plugin_parameter",
            ManagedTranslationShape::Single,
            ManagedTranslationContent::scalar("原文"),
            "",
            None,
        )
        .expect("测试单元应合法")
    }

    #[test]
    fn collection_requires_unique_unit_keys() {
        let duplicate_units = ManagedTranslationCollection::new(
            "quests",
            "",
            vec![scalar_unit("same"), scalar_unit("same")],
        );
        assert!(matches!(
            duplicate_units,
            Err(ManagedTranslationModelError::DuplicateUnitKey { .. })
        ));
    }

    #[test]
    fn shapes_enforce_atomic_scalar_and_aligned_array_contracts() {
        assert!(matches!(
            ManagedTranslationUnit::new(
                "bad",
                "kind",
                ManagedTranslationShape::Single,
                ManagedTranslationContent::array(vec!["值".to_owned()]),
                "",
                None,
            ),
            Err(ManagedTranslationModelError::ContentShapeMismatch { .. })
        ));
        let lines = ManagedTranslationUnit::new(
            "lines",
            "kind",
            ManagedTranslationShape::Lines,
            ManagedTranslationContent::array(vec!["甲".to_owned(), "".to_owned()]),
            "",
            None,
        )
        .expect("含空槽 lines 应合法");
        assert!(matches!(
            lines.translation_pair(
                ManagedTranslationContent::array(vec!["A".to_owned(), "B".to_owned()]),
                Sha256Fingerprint::from_bytes([3; 32]),
            ),
            Err(ManagedTranslationModelError::BlankSlotMismatch { index: 1 })
        ));

        let reflow = ManagedTranslationUnit::new(
            "body",
            "kind",
            ManagedTranslationShape::Reflow,
            ManagedTranslationContent::scalar("第一行\n第二行"),
            "",
            None,
        )
        .expect("reflow 原文可以包含 LF");
        reflow
            .translation_pair(
                ManagedTranslationContent::scalar("first\nsecond"),
                Sha256Fingerprint::from_bytes([4; 32]),
            )
            .expect("reflow 译文可以重新断行");
    }

    #[test]
    fn metadata_is_any_exact_canonical_json_value() {
        let metadata = ManagedTranslationMetadata::from_canonical_json(r#"{"quest_id":12}"#)
            .expect("紧凑 object 应合法");
        assert_eq!(metadata.canonical_json(), r#"{"quest_id":12}"#);
        for value in [r#"[12]"#, r#""tag""#, "12", "true", "null"] {
            assert_eq!(
                ManagedTranslationMetadata::from_canonical_json(value)
                    .expect("任意 canonical JSON 值都应合法")
                    .canonical_json(),
                value
            );
        }
        assert!(matches!(
            ManagedTranslationMetadata::from_canonical_json(r#"{ "quest_id": 12 }"#),
            Err(ManagedTranslationModelError::NonCanonicalMetadataJson)
        ));
    }

    #[test]
    fn prepared_content_owns_all_shape_composition_and_content_state() {
        let semantics = FakeSemantics;
        let items = ManagedPreparedContent::prepare(
            &semantics,
            "item",
            ManagedTranslationShape::Items,
            &ManagedTranslationContent::array(vec!["active".to_owned(), "protected".to_owned()]),
            "inventory",
        )
        .expect("items 应可准备");

        assert_eq!(
            items.part_statuses(),
            [
                ManagedPreparedTranslationStatus::Active,
                ManagedPreparedTranslationStatus::FullyProtected,
            ]
        );
        assert!(items.is_active());
        assert_eq!(
            items.model_content(),
            ManagedTranslationContent::array(vec![
                "__ATT__".to_owned(),
                "__PROTECTED__".to_owned(),
            ])
        );
        assert_eq!(items.terms().len(), 1, "重复术语应按首次出现去重");

        let ManagedPreparedContentAcceptance::Accepted { content, state } = items
            .accept(ManagedTranslationContent::array(vec![
                "__ATT__译文".to_owned(),
                "__PROTECTED__".to_owned(),
            ]))
            .expect("items 语义验收应执行")
        else {
            panic!("合法 items 候选应被接受");
        };
        assert_eq!(
            content,
            ManagedTranslationContent::array(
                vec!["\\V[1]译文".to_owned(), "protected".to_owned(),]
            ),
            "活动槽恢复 Placeholder，非活动槽恢复原领域值"
        );
        assert!(
            items
                .is_current(&content, state)
                .expect("同 shape 的 Current 检查应成功")
        );
        assert!(
            !items
                .is_current(
                    &ManagedTranslationContent::array(vec![
                        "\\V[1]其他".to_owned(),
                        "protected".to_owned(),
                    ]),
                    state,
                )
                .expect("不同正文仍是合法 shape")
        );

        let inactive = ManagedPreparedContent::prepare(
            &semantics,
            "item",
            ManagedTranslationShape::Items,
            &ManagedTranslationContent::array(vec![
                "protected".to_owned(),
                "non_source".to_owned(),
            ]),
            "",
        )
        .expect("非活动 items 应可准备");
        assert!(!inactive.is_active());
        assert_eq!(
            inactive.part_statuses(),
            [
                ManagedPreparedTranslationStatus::FullyProtected,
                ManagedPreparedTranslationStatus::NonSourceLanguage,
            ]
        );
    }

    #[test]
    fn one_structured_validator_serves_model_and_candidate_boundaries() {
        let original = ManagedTranslationContent::array(vec!["第一行".to_owned(), "".to_owned()]);
        let rejection = validate_managed_translation_candidate(
            ManagedTranslationShape::Lines,
            &original,
            &ManagedTranslationContent::array(vec!["译文".to_owned(), "不应填充".to_owned()]),
        )
        .expect_err("lines 空槽变化必须拒绝");
        assert_eq!(
            rejection,
            ManagedTranslationContentRejection::BlankSlotMismatch { index: 1 }
        );
        assert_eq!(rejection.reason(), "blank_slot_mismatch");
        assert_eq!(rejection.item_number(), Some(2));

        let scalar_original = ManagedTranslationContent::scalar("原文");
        let blank = ManagedTranslationContent::scalar(" ");
        assert_eq!(
            validate_managed_translation_candidate(
                ManagedTranslationShape::Single,
                &scalar_original,
                &blank,
            ),
            Err(ManagedTranslationContentRejection::BlankTranslation)
        );
        validate_translation(ManagedTranslationShape::Single, &scalar_original, &blank)
            .expect("持久化模型保持既有标量空白行为");
    }

    #[test]
    fn prepared_content_preserves_domain_text_and_rejects_only_real_controls() {
        let semantics = FakeSemantics;
        let single = ManagedPreparedContent::prepare(
            &semantics,
            "database_entry",
            ManagedTranslationShape::Single,
            &ManagedTranslationContent::scalar("勇者「字面\\n」🚀"),
            "menu=\"status\"\\panel",
        )
        .expect("Unicode、引号、反斜杠和字面转义序列都应是普通领域文本");
        let accepted = single
            .accept(ManagedTranslationContent::scalar("__ATT__「字面\\n」🚀"))
            .expect("候选语义验收应执行");
        let ManagedPreparedContentAcceptance::Accepted { content, state } = accepted else {
            panic!("合法领域文本应被接受");
        };
        assert_eq!(
            content,
            ManagedTranslationContent::scalar("\\V[1]「字面\\n」🚀")
        );
        assert!(
            single
                .is_current(&content, state)
                .expect("规范领域值应能重算 Current")
        );

        let reflow = ManagedPreparedContent::prepare(
            &semantics,
            "database_entry",
            ManagedTranslationShape::Reflow,
            &ManagedTranslationContent::scalar("第一行\n第二行"),
            "",
        )
        .expect("reflow 原文允许真实 LF");
        assert!(matches!(
            reflow
                .accept(ManagedTranslationContent::scalar("译文一\n译文二"))
                .expect("reflow 验收应执行"),
            ManagedPreparedContentAcceptance::Accepted { .. }
        ));

        for candidate in ["真实\n换行", "回车\r", "空字符\0"] {
            let rejection = single
                .accept(ManagedTranslationContent::scalar(candidate))
                .expect("结构拒绝应是普通验收结果");
            assert!(matches!(
                rejection,
                ManagedPreparedContentAcceptance::Rejected {
                    rejection: ManagedPreparedContentRejection::Structure(
                        ManagedTranslationContentRejection::InvalidText { index: 0 }
                    )
                }
            ));
        }
    }
}
