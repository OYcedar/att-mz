//! Generic 对公共 Placeholder 算法的任意 kind 精确 scope 适配。

use std::collections::HashMap;
use std::error::Error;
use std::fmt;

use crate::fingerprint::Sha256Fingerprint;
use crate::translation::placeholder::{
    AppliedPlaceholder, PlaceholderProtectionError, PlaceholderRestoreError,
    PlaceholderRuleCompilationError, PlaceholderRuleOrigin, PlaceholderSegment, PlaceholderService,
};

pub(crate) use crate::translation::placeholder::{
    CompiledPlaceholderRules as GenericCompiledPlaceholderRules,
    PlaceholderRuleDefinition as GenericPlaceholderRuleDefinition,
    ProtectedText as GenericProtectedText,
};

/// Generic 不增加内置规则，只把当前 kind 原值交给公共 scope 匹配。
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct GenericPlaceholderService {
    common: PlaceholderService,
}

impl GenericPlaceholderService {
    pub(crate) fn parse_canonical_json(
        &self,
        canonical_json: &str,
    ) -> Result<Vec<GenericPlaceholderRuleDefinition>, GenericPlaceholderError> {
        serde_json::from_str(canonical_json)
            .map_err(GenericPlaceholderError::InvalidResourceSnapshot)
    }

    pub(crate) fn compile(
        &self,
        definitions: Vec<GenericPlaceholderRuleDefinition>,
    ) -> Result<GenericCompiledPlaceholderRules, GenericPlaceholderError> {
        self.common
            .compile_custom(definitions, |scope| !scope.is_empty())
            .map_err(GenericPlaceholderError::Compilation)
    }

    pub(crate) fn protect(
        &self,
        kind: &str,
        original: &str,
        compiled: &GenericCompiledPlaceholderRules,
    ) -> Result<GenericProtectedText, GenericPlaceholderError> {
        self.common
            .protect(kind, original, &[], compiled, None)
            .map_err(GenericPlaceholderError::Protection)
    }

    pub(crate) fn restore(
        &self,
        protected: &GenericProtectedText,
        candidate: &str,
    ) -> Result<String, GenericPlaceholderError> {
        protected
            .restore(candidate)
            .map_err(GenericPlaceholderError::Restore)
    }
}

/// 从项目保存的规范规则重建一个 Unit 的实际 Placeholder 绑定身份。
pub(crate) fn placeholder_binding_fingerprint(
    canonical_json: &str,
    kind: &str,
    source_text: &str,
) -> Result<Sha256Fingerprint, GenericPlaceholderError> {
    let service = GenericPlaceholderService::default();
    let definitions = service.parse_canonical_json(canonical_json)?;
    let compiled = service.compile(definitions)?;
    Ok(service
        .protect(kind, source_text, &compiled)?
        .binding_fingerprint())
}

/// 检查人工译文是否保留当前原文实际命中的 Placeholder。
///
/// 人工译文使用原始 Placeholder，不使用发给模型的临时 token；因此这里分别保护原文和
/// 译文，再比较实际绑定的多重集。Placeholder 可以随语序移动，但不能丢失、增加或改变。
pub(crate) fn validate_manual_translation_placeholders(
    canonical_json: &str,
    kind: &str,
    source_text: &str,
    translation: &str,
) -> Result<(), GenericPlaceholderError> {
    let service = GenericPlaceholderService::default();
    let definitions = service.parse_canonical_json(canonical_json)?;
    let compiled = service.compile(definitions)?;
    validate_translation_placeholders(&service, &compiled, kind, source_text, translation)
}

/// 使用已经编译的规则检查译文与原文实际 Placeholder 多重集。
pub(crate) fn validate_translation_placeholders(
    service: &GenericPlaceholderService,
    compiled: &GenericCompiledPlaceholderRules,
    kind: &str,
    source_text: &str,
    translation: &str,
) -> Result<(), GenericPlaceholderError> {
    let source = service.protect(kind, source_text, compiled)?;
    let candidate = service.protect(kind, translation, compiled)?;
    if placeholder_multiset(source.placeholders()) != placeholder_multiset(candidate.placeholders())
    {
        return Err(GenericPlaceholderError::ManualTranslationMismatch);
    }
    Ok(())
}

fn placeholder_multiset(
    placeholders: &[AppliedPlaceholder],
) -> HashMap<(&str, &str, PlaceholderRuleOrigin, &str, PlaceholderSegment), usize> {
    let mut multiset = HashMap::with_capacity(placeholders.len());
    for placeholder in placeholders {
        *multiset
            .entry((
                placeholder.original(),
                placeholder.label(),
                placeholder.origin(),
                placeholder.scope(),
                placeholder.segment(),
            ))
            .or_default() += 1;
    }
    multiset
}

/// Generic 适配边界为公共 Placeholder 失败补充资源阶段。
#[derive(Debug)]
pub(crate) enum GenericPlaceholderError {
    InvalidResourceSnapshot(serde_json::Error),
    Compilation(PlaceholderRuleCompilationError),
    Protection(PlaceholderProtectionError),
    Restore(PlaceholderRestoreError),
    ManualTranslationMismatch,
}

impl fmt::Display for GenericPlaceholderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidResourceSnapshot(source) => {
                write!(
                    formatter,
                    "Generic Placeholder 资源不是现行规范 JSON：{source}"
                )
            }
            Self::Compilation(source) => {
                write!(formatter, "Generic Placeholder 规则无效：{source}")
            }
            Self::Protection(source) => write!(formatter, "Generic Placeholder 保护失败：{source}"),
            Self::Restore(source) => write!(formatter, "Generic Placeholder 恢复失败：{source}"),
            Self::ManualTranslationMismatch => {
                formatter.write_str("人工译文没有完整保留原文实际命中的 Placeholder")
            }
        }
    }
}

impl Error for GenericPlaceholderError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidResourceSnapshot(source) => Some(source),
            Self::Compilation(source) => Some(source),
            Self::Protection(source) => Some(source),
            Self::Restore(source) => Some(source),
            Self::ManualTranslationMismatch => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arbitrary_kind_scope_is_exact_and_round_trips() {
        let service = GenericPlaceholderService::default();
        let compiled = service
            .compile(vec![GenericPlaceholderRuleDefinition::new(
                Some(vec!["dialogue".to_owned()]),
                r"\{[^}]+\}",
            )])
            .unwrap();
        let protected = service
            .protect("dialogue", "你好 {name}", &compiled)
            .unwrap();
        assert!(!protected.text().contains("{name}"));
        assert_eq!(protected.placeholders().len(), 1);
        assert_eq!(
            service
                .restore(
                    &protected,
                    &format!("您好 {}", protected.placeholders()[0].token())
                )
                .unwrap(),
            "您好 {name}"
        );
        assert!(
            service
                .protect("name", "你好 {name}", &compiled)
                .unwrap()
                .placeholders()
                .is_empty()
        );
    }

    #[test]
    fn whitespace_kind_scope_is_preserved_and_matched_exactly() {
        let service = GenericPlaceholderService::default();
        let compiled = service
            .compile(vec![GenericPlaceholderRuleDefinition::new(
                Some(vec![" ".to_owned()]),
                r"\{[^}]+\}",
            )])
            .expect("纯空白但非空的 Generic kind 是合法 scope");

        assert_eq!(
            service
                .protect(" ", "你好 {name}", &compiled)
                .expect("规则应精确匹配空白 kind")
                .placeholders()
                .len(),
            1
        );
        assert!(
            service
                .protect("dialogue", "你好 {name}", &compiled)
                .expect("其他 kind 仍应合法")
                .placeholders()
                .is_empty()
        );
    }

    #[test]
    fn text_capture_protects_only_the_wrapper() {
        let service = GenericPlaceholderService::default();
        let compiled = service
            .compile(vec![GenericPlaceholderRuleDefinition::new(
                None,
                r"<tag>(?<text>.*?)</tag>",
            )])
            .unwrap();
        let protected = service
            .protect("description", "<tag>可翻译</tag>", &compiled)
            .unwrap();
        assert!(protected.text().contains("可翻译"));
        assert_eq!(protected.placeholders().len(), 2);
        assert_eq!(
            service.restore(&protected, protected.text()).unwrap(),
            "<tag>可翻译</tag>"
        );
    }

    #[test]
    fn binding_identity_depends_on_actual_matches_not_unused_rules() {
        let first = placeholder_binding_fingerprint(r#"[{"pattern":"\\{[^}]+\\}"}]"#, "k", "plain")
            .unwrap();
        let second = placeholder_binding_fingerprint(
            r#"[{"pattern":"never"},{"pattern":"\\{[^}]+\\}"}]"#,
            "k",
            "plain",
        )
        .unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn manual_translation_may_reorder_but_not_change_placeholders() {
        let resource = r#"[{"pattern":"\\{[^}]+\\}"}]"#;
        validate_manual_translation_placeholders(
            resource,
            "dialogue",
            "{speaker} greets {target}",
            "{target}收到{speaker}的问候",
        )
        .expect("人工译文可以调整 Placeholder 次序");
        assert!(matches!(
            validate_manual_translation_placeholders(
                resource,
                "dialogue",
                "{speaker} greets {target}",
                "{speaker}打招呼",
            ),
            Err(GenericPlaceholderError::ManualTranslationMismatch)
        ));
    }
}
