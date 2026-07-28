//! Lua 托管翻译高级接口与共享 Managed planner 之间的引擎无关契约。

use std::sync::Arc;

use crate::fingerprint::Sha256Fingerprint;
use crate::lua_host::{TrustedLuaHostCallError, TrustedLuaHostFuture};

use super::{ManagedTranslationContent, ManagedTranslationShape};

/// Lua 高级接口沿用根领域形状，不建立第二套枚举。
pub(crate) type TrustedLuaManagedTranslationShape = ManagedTranslationShape;

/// Lua 高级接口沿用根领域正文，不建立第二套内容类型。
pub(crate) type TrustedLuaManagedTranslationContent = ManagedTranslationContent;

/// Lua Extract 已完整解析的一项托管翻译声明。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TrustedLuaManagedTranslationUnitDeclaration {
    key: String,
    kind: String,
    shape: TrustedLuaManagedTranslationShape,
    original: TrustedLuaManagedTranslationContent,
    context: String,
    metadata_json: Option<String>,
}

impl TrustedLuaManagedTranslationUnitDeclaration {
    pub(crate) fn new(
        key: String,
        kind: String,
        shape: TrustedLuaManagedTranslationShape,
        original: TrustedLuaManagedTranslationContent,
        context: String,
        metadata_json: Option<String>,
    ) -> Self {
        Self {
            key,
            kind,
            shape,
            original,
            context,
            metadata_json,
        }
    }

    pub(crate) fn key(&self) -> &str {
        &self.key
    }

    pub(crate) fn kind(&self) -> &str {
        &self.kind
    }

    pub(crate) const fn shape(&self) -> TrustedLuaManagedTranslationShape {
        self.shape
    }

    pub(crate) fn original(&self) -> &TrustedLuaManagedTranslationContent {
        &self.original
    }

    pub(crate) fn context(&self) -> &str {
        &self.context
    }

    pub(crate) fn metadata_json(&self) -> Option<&str> {
        self.metadata_json.as_deref()
    }
}

/// Lua Extract 已完整解析的一个托管翻译集合。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TrustedLuaManagedTranslationCollectionDeclaration {
    name: String,
    instruction: String,
    units: Vec<TrustedLuaManagedTranslationUnitDeclaration>,
}

impl TrustedLuaManagedTranslationCollectionDeclaration {
    pub(crate) fn new(
        name: String,
        instruction: String,
        units: Vec<TrustedLuaManagedTranslationUnitDeclaration>,
    ) -> Self {
        Self {
            name,
            instruction,
            units,
        }
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn instruction(&self) -> &str {
        &self.instruction
    }

    pub(crate) fn units(&self) -> &[TrustedLuaManagedTranslationUnitDeclaration] {
        &self.units
    }
}

/// Lua Extract 通过 `ctx.translations.replace` 声明的完整托管快照。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct TrustedLuaManagedTranslationSnapshot {
    collections: Vec<TrustedLuaManagedTranslationCollectionDeclaration>,
}

impl TrustedLuaManagedTranslationSnapshot {
    pub(crate) fn new(collections: Vec<TrustedLuaManagedTranslationCollectionDeclaration>) -> Self {
        Self { collections }
    }

    pub(crate) fn collections(&self) -> &[TrustedLuaManagedTranslationCollectionDeclaration] {
        &self.collections
    }
}

/// `ctx.translations.open` 投影的一项持久化状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TrustedLuaManagedTranslationUnitStatus {
    Current,
    Missing,
    NotApplicable,
    Unavailable,
}

impl TrustedLuaManagedTranslationUnitStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Missing => "missing",
            Self::NotApplicable => "not_applicable",
            Self::Unavailable => "unavailable",
        }
    }
}

/// 托管核心冻结后交给 Lua 只读访问的一项单元。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TrustedLuaManagedTranslationUnit {
    key: String,
    kind: String,
    shape: TrustedLuaManagedTranslationShape,
    original: TrustedLuaManagedTranslationContent,
    context: String,
    metadata_json: Option<String>,
    translation: Option<TrustedLuaManagedTranslationContent>,
    status: TrustedLuaManagedTranslationUnitStatus,
}

impl TrustedLuaManagedTranslationUnit {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        key: String,
        kind: String,
        shape: TrustedLuaManagedTranslationShape,
        original: TrustedLuaManagedTranslationContent,
        context: String,
        metadata_json: Option<String>,
        translation: Option<TrustedLuaManagedTranslationContent>,
        status: TrustedLuaManagedTranslationUnitStatus,
    ) -> Self {
        Self {
            key,
            kind,
            shape,
            original,
            context,
            metadata_json,
            translation,
            status,
        }
    }

    pub(crate) fn key(&self) -> &str {
        &self.key
    }

    pub(crate) fn kind(&self) -> &str {
        &self.kind
    }

    pub(crate) const fn shape(&self) -> TrustedLuaManagedTranslationShape {
        self.shape
    }

    pub(crate) fn original(&self) -> &TrustedLuaManagedTranslationContent {
        &self.original
    }

    pub(crate) fn context(&self) -> &str {
        &self.context
    }

    pub(crate) fn metadata_json(&self) -> Option<&str> {
        self.metadata_json.as_deref()
    }

    pub(crate) fn translation(&self) -> Option<&TrustedLuaManagedTranslationContent> {
        self.translation.as_ref()
    }

    pub(crate) const fn status(&self) -> TrustedLuaManagedTranslationUnitStatus {
        self.status
    }
}

/// 托管核心冻结后交给 Lua 只读访问的一个集合。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TrustedLuaManagedTranslationCollection {
    name: String,
    instruction: String,
    units: Vec<TrustedLuaManagedTranslationUnit>,
}

impl TrustedLuaManagedTranslationCollection {
    pub(crate) fn new(
        name: String,
        instruction: String,
        units: Vec<TrustedLuaManagedTranslationUnit>,
    ) -> Self {
        Self {
            name,
            instruction,
            units,
        }
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn instruction(&self) -> &str {
        &self.instruction
    }

    pub(crate) fn units(&self) -> &[TrustedLuaManagedTranslationUnit] {
        &self.units
    }

    pub(crate) fn get(&self, key: &str) -> Option<&TrustedLuaManagedTranslationUnit> {
        self.units.iter().find(|unit| unit.key() == key)
    }
}

/// 一次 managed Translate 对单元的可观察结果分类。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TrustedLuaManagedTranslationResultStatus {
    Current,
    Translated,
    NotApplicable,
    Unavailable,
}

impl TrustedLuaManagedTranslationResultStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Translated => "translated",
            Self::NotApplicable => "not_applicable",
            Self::Unavailable => "unavailable",
        }
    }
}

/// 一次 managed Translate 逐单元返回的结构化事实。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TrustedLuaManagedTranslationResult {
    collection: String,
    key: String,
    status: TrustedLuaManagedTranslationResultStatus,
    translation: Option<TrustedLuaManagedTranslationContent>,
    reason: Option<String>,
    details_json: Option<String>,
}

impl TrustedLuaManagedTranslationResult {
    pub(crate) fn new(
        collection: String,
        key: String,
        status: TrustedLuaManagedTranslationResultStatus,
        translation: Option<TrustedLuaManagedTranslationContent>,
        reason: Option<String>,
        details_json: Option<String>,
    ) -> Self {
        Self {
            collection,
            key,
            status,
            translation,
            reason,
            details_json,
        }
    }

    pub(crate) fn collection(&self) -> &str {
        &self.collection
    }

    pub(crate) fn key(&self) -> &str {
        &self.key
    }

    pub(crate) const fn status(&self) -> TrustedLuaManagedTranslationResultStatus {
        self.status
    }

    pub(crate) fn translation(&self) -> Option<&TrustedLuaManagedTranslationContent> {
        self.translation.as_ref()
    }

    pub(crate) fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    pub(crate) fn details_json(&self) -> Option<&str> {
        self.details_json.as_deref()
    }
}

/// 一次 managed Translate 完成后返回给 Lua 的只读报告。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct TrustedLuaManagedTranslationReport {
    units: Vec<TrustedLuaManagedTranslationResult>,
}

impl TrustedLuaManagedTranslationReport {
    pub(crate) fn new(units: Vec<TrustedLuaManagedTranslationResult>) -> Self {
        Self { units }
    }

    pub(crate) fn units(&self) -> &[TrustedLuaManagedTranslationResult] {
        &self.units
    }
}

/// Translate Host 交给 `ctx.translations` 的托管业务能力。
pub(crate) trait TrustedLuaManagedTranslateHostCalls: Send + Sync + 'static {
    fn translate(
        &self,
    ) -> TrustedLuaHostFuture<Result<TrustedLuaManagedTranslationReport, TrustedLuaHostCallError>>;

    fn open(
        &self,
        name: String,
    ) -> TrustedLuaHostFuture<
        Result<Option<TrustedLuaManagedTranslationCollection>, TrustedLuaHostCallError>,
    >;
}

/// WriteBack 只读打开最后提交托管快照所需的窄能力。
pub(crate) trait TrustedLuaManagedTranslationReader: Send + Sync + 'static {
    fn open(
        &self,
        name: String,
    ) -> TrustedLuaHostFuture<
        Result<Option<TrustedLuaManagedTranslationCollection>, TrustedLuaHostCallError>,
    >;
}

pub(crate) fn managed_translations_unavailable(operation: &'static str) -> TrustedLuaHostCallError {
    TrustedLuaHostCallError::new(
        "translations",
        "unavailable",
        "当前 Lua Host 未构造托管翻译能力",
        None,
        None,
    )
    .with_operation(operation)
}

/// Translate 共享语义对一段文本完成预处理后的稳定状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TrustedLuaPreparedTranslationStatus {
    Active,
    NonSourceLanguage,
    FullyProtected,
}

impl TrustedLuaPreparedTranslationStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::NonSourceLanguage => "non_source_language",
            Self::FullyProtected => "fully_protected",
        }
    }
}

/// 当前单段文本实际命中的一个有序术语对。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TrustedLuaTranslationTerm {
    term: String,
    translation: String,
}

impl TrustedLuaTranslationTerm {
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

/// prepared handle 对候选正文的正常验收结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TrustedLuaPreparedTranslationAcceptance {
    Accepted {
        translation: String,
        state: Sha256Fingerprint,
    },
    Rejected {
        reason: String,
    },
}

impl TrustedLuaPreparedTranslationAcceptance {
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

/// 共享翻译语义建立、不可由 Lua 伪造的预处理句柄。
pub(crate) trait TrustedLuaPreparedTranslation: Send + Sync + 'static {
    fn status(&self) -> TrustedLuaPreparedTranslationStatus;
    fn model_text(&self) -> &str;
    fn terms(&self) -> &[TrustedLuaTranslationTerm];
    fn semantic_fingerprint(&self) -> Sha256Fingerprint;

    fn is_current(
        &self,
        translation: String,
        state: Sha256Fingerprint,
    ) -> Result<bool, TrustedLuaHostCallError>;

    fn accept(
        &self,
        candidate: String,
    ) -> Result<TrustedLuaPreparedTranslationAcceptance, TrustedLuaHostCallError>;
}

/// 引擎适配器向共享 Managed planner 提供的翻译语义。
///
/// `kind` 保持调用引擎的原始稳定名称；根内核不解释任何引擎枚举。
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
    ) -> Result<Arc<dyn TrustedLuaPreparedTranslation>, TrustedLuaHostCallError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakePrepared {
        model_text: String,
        terms: Vec<TrustedLuaTranslationTerm>,
    }

    impl TrustedLuaPreparedTranslation for FakePrepared {
        fn status(&self) -> TrustedLuaPreparedTranslationStatus {
            TrustedLuaPreparedTranslationStatus::Active
        }

        fn model_text(&self) -> &str {
            &self.model_text
        }

        fn terms(&self) -> &[TrustedLuaTranslationTerm] {
            &self.terms
        }

        fn semantic_fingerprint(&self) -> Sha256Fingerprint {
            Sha256Fingerprint::from_bytes([0x31; 32])
        }

        fn is_current(
            &self,
            translation: String,
            state: Sha256Fingerprint,
        ) -> Result<bool, TrustedLuaHostCallError> {
            Ok(translation == "译文" && state == self.semantic_fingerprint())
        }

        fn accept(
            &self,
            candidate: String,
        ) -> Result<TrustedLuaPreparedTranslationAcceptance, TrustedLuaHostCallError> {
            Ok(TrustedLuaPreparedTranslationAcceptance::accepted(
                candidate,
                self.semantic_fingerprint(),
            ))
        }
    }

    struct FakeSemantics;

    impl ManagedTranslationSemantics for FakeSemantics {
        fn engine_semantic_identity(&self) -> &str {
            "future_engine"
        }

        fn system_prompt(&self) -> &str {
            "system"
        }

        fn source_language(&self) -> &str {
            "ja"
        }

        fn target_language(&self) -> &str {
            "zh-CN"
        }

        fn prepare_translation(
            &self,
            kind: &str,
            shape: ManagedTranslationShape,
            original: &ManagedTranslationContent,
            semantic_context: &str,
        ) -> Result<Arc<dyn TrustedLuaPreparedTranslation>, TrustedLuaHostCallError> {
            assert_eq!(kind, "future_engine_private_kind");
            assert_eq!(shape, ManagedTranslationShape::Single);
            assert_eq!(original, &ManagedTranslationContent::scalar("原文"),);
            assert_eq!(semantic_context, "上下文");
            Ok(Arc::new(FakePrepared {
                model_text: "protected".to_owned(),
                terms: vec![TrustedLuaTranslationTerm::new("原文", "translation")],
            }))
        }
    }

    #[test]
    fn lua_contract_uses_the_root_shape_and_content_types() {
        let declaration = TrustedLuaManagedTranslationUnitDeclaration::new(
            "key".to_owned(),
            "kind".to_owned(),
            ManagedTranslationShape::Items,
            ManagedTranslationContent::array(vec!["一".to_owned(), "二".to_owned()]),
            String::new(),
            Some(r#"{"opaque":true}"#.to_owned()),
        );

        assert_eq!(declaration.shape(), ManagedTranslationShape::Items);
        assert_eq!(
            declaration.original(),
            &ManagedTranslationContent::array(vec!["一".to_owned(), "二".to_owned()])
        );
    }

    #[test]
    fn semantics_port_accepts_an_engine_neutral_kind_and_root_content() {
        let semantics = FakeSemantics;
        assert_eq!(semantics.engine_semantic_identity(), "future_engine");
        assert_eq!(semantics.system_prompt(), "system");
        assert_eq!(semantics.source_language(), "ja");
        assert_eq!(semantics.target_language(), "zh-CN");
        let prepared = semantics
            .prepare_translation(
                "future_engine_private_kind",
                ManagedTranslationShape::Single,
                &ManagedTranslationContent::scalar("原文"),
                "上下文",
            )
            .expect("根语义端口应接受不属于 RPG Maker 枚举的 kind");

        assert_eq!(prepared.model_text(), "protected");
        assert_eq!(prepared.terms()[0].term(), "原文");
        assert!(
            prepared
                .is_current("译文".to_owned(), Sha256Fingerprint::from_bytes([0x31; 32]))
                .expect("current 检查应成功")
        );
    }
}
