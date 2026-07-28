//! 与具体游戏引擎无关的 Lua 托管翻译领域模型。
//!
//! 本模块只拥有 collection、unit、内容形状、metadata 与成对译文的不变量。
//! 引擎来源身份、项目快照、checkpoint 和持久化由各引擎适配边界负责。

mod kernel;
mod lua;

use std::collections::HashSet;
use std::error::Error;
use std::fmt;

use crate::fingerprint::Sha256Fingerprint;
use crate::json::{self, StackSafeJsonError};
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
    validate_content_shape(shape, translation)?;
    match (shape, original, translation) {
        (
            ManagedTranslationShape::Single,
            ManagedTranslationContent::Scalar(_),
            ManagedTranslationContent::Scalar(value),
        ) => validate_line(value, "translation", 0),
        (
            ManagedTranslationShape::Reflow,
            ManagedTranslationContent::Scalar(_),
            ManagedTranslationContent::Scalar(value),
        ) => {
            if value.contains('\0') || value.contains('\r') {
                Err(ManagedTranslationModelError::InvalidContentText {
                    role: "translation",
                    index: 0,
                })
            } else {
                Ok(())
            }
        }
        (
            ManagedTranslationShape::Lines,
            ManagedTranslationContent::Array(original),
            ManagedTranslationContent::Array(translation),
        ) => validate_aligned_array(shape, original, translation, true),
        (
            ManagedTranslationShape::Items,
            ManagedTranslationContent::Array(original),
            ManagedTranslationContent::Array(translation),
        ) => validate_aligned_array(shape, original, translation, false),
        _ => unreachable!("原文与译文形状均已验证"),
    }
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

fn validate_aligned_array(
    shape: ManagedTranslationShape,
    original: &[String],
    translation: &[String],
    preserve_blank_slots: bool,
) -> Result<(), ManagedTranslationModelError> {
    if original.len() != translation.len() {
        return Err(ManagedTranslationModelError::TranslationItemCountMismatch {
            shape,
            expected: original.len(),
            actual: translation.len(),
        });
    }
    for (index, (original, translation)) in original.iter().zip(translation).enumerate() {
        validate_line(translation, "translation", index)?;
        if preserve_blank_slots && original.trim().is_empty() != translation.trim().is_empty() {
            return Err(ManagedTranslationModelError::BlankSlotMismatch { index });
        }
        if !preserve_blank_slots && translation.trim().is_empty() {
            return Err(ManagedTranslationModelError::BlankItem {
                role: "translation",
                index,
            });
        }
    }
    Ok(())
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
}
