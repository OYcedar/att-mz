//! RPG Maker 对公共 Placeholder 算法的 scope 与内置控制符适配。

use crate::rpg_maker::RpgMakerEngine;
use crate::rpg_maker::text::TextGroupKind;
use crate::translation::placeholder::{CompiledBuiltinPlaceholderRule, PlaceholderService};

pub(crate) use crate::translation::placeholder::{
    CompiledPlaceholderRules, Pcre2PlaceholderConstructionError, PlaceholderProtectionError,
    PlaceholderRuleCompilationError, PlaceholderRuleDefinition, ProtectedText,
};

const MV_BUILTIN_CONTROL_PATTERN: &str = r"\\(?:[VvNnPpCcIi]\[[0-9]+\]|[Gg]|[\\{}$.|!><^])";
const MZ_BUILTIN_CONTROL_PATTERN: &str =
    r"\\(?:(?:[VvNnPpCcIi]|[Pp][Xx]|[Pp][Yy]|[Ff][Ss])\[[0-9]+\]|[Gg]|[\\{}$.|!><^])";
const BUILTIN_SEMANTIC_LABEL: &str = "RPG_MAKER_CONTROL";

/// RPG Maker 的封闭 scope 与 MV/MZ 内置控制符入口。
pub(crate) struct Pcre2PlaceholderService {
    common: PlaceholderService,
    mv_builtin: CompiledBuiltinPlaceholderRule,
    mz_builtin: CompiledBuiltinPlaceholderRule,
}

impl Clone for Pcre2PlaceholderService {
    fn clone(&self) -> Self {
        Self::new().expect("进程已成功编译过固定 RPG Maker 内置 Placeholder")
    }
}

impl Pcre2PlaceholderService {
    pub(crate) fn new() -> Result<Self, Pcre2PlaceholderConstructionError> {
        let common = PlaceholderService;
        Ok(Self {
            mv_builtin: common
                .compile_builtin(MV_BUILTIN_CONTROL_PATTERN, BUILTIN_SEMANTIC_LABEL)?,
            mz_builtin: common
                .compile_builtin(MZ_BUILTIN_CONTROL_PATTERN, BUILTIN_SEMANTIC_LABEL)?,
            common,
        })
    }

    pub(crate) fn compile_custom(
        &self,
        definitions: Vec<PlaceholderRuleDefinition>,
    ) -> Result<CompiledPlaceholderRules, PlaceholderRuleCompilationError> {
        self.common.compile_custom(definitions, |scope| {
            TextGroupKind::from_storage_name(scope).is_some()
        })
    }

    #[cfg(test)]
    pub(crate) fn protect(
        &self,
        engine: RpgMakerEngine,
        kind: TextGroupKind,
        original: &str,
        custom: &CompiledPlaceholderRules,
    ) -> Result<ProtectedText, PlaceholderProtectionError> {
        self.protect_with_line_boundaries(engine, kind, original, &[], custom)
    }

    pub(crate) fn protect_with_line_boundaries(
        &self,
        engine: RpgMakerEngine,
        kind: TextGroupKind,
        original: &str,
        line_separator_offsets: &[usize],
        custom: &CompiledPlaceholderRules,
    ) -> Result<ProtectedText, PlaceholderProtectionError> {
        let builtin = match engine {
            RpgMakerEngine::Mv => &self.mv_builtin,
            RpgMakerEngine::Mz => &self.mz_builtin,
        };
        self.common.protect(
            kind.storage_name(),
            original,
            line_separator_offsets,
            custom,
            Some(builtin),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::translation::placeholder::{PlaceholderRuleOrigin, PlaceholderSegment};

    #[test]
    fn rpg_adapter_adds_engine_builtin_and_rejects_generic_scope() {
        let service = Pcre2PlaceholderService::new().unwrap();
        assert!(matches!(
            service.compile_custom(vec![PlaceholderRuleDefinition::new(
                Some(vec!["arbitrary".to_owned()]),
                "x"
            )]),
            Err(PlaceholderRuleCompilationError::UnknownScope { .. })
        ));
        assert!(matches!(
            service.compile_custom(vec![PlaceholderRuleDefinition::new(
                Some(vec![" ".to_owned()]),
                "x"
            )]),
            Err(PlaceholderRuleCompilationError::UnknownScope { .. })
        ));
        let custom = service.compile_custom(Vec::new()).unwrap();
        let protected = service
            .protect(
                RpgMakerEngine::Mz,
                TextGroupKind::EventDialogue,
                r"\FS[20]勇者",
                &custom,
            )
            .unwrap();
        let (_, bindings) = protected.into_parts();
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].origin(), PlaceholderRuleOrigin::BuiltIn);
        assert_eq!(bindings[0].segment(), PlaceholderSegment::Whole);
    }
}
