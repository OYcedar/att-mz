//! RPG Maker 对公共 Placeholder 算法的 scope 与内置控制符适配。

#[cfg(test)]
use std::convert::Infallible;
use std::sync::Arc;

use crate::rpg_maker::RpgMakerEngine;
use crate::rpg_maker::text::TextGroupKind;
use crate::translation::placeholder::{CompiledBuiltinPlaceholderRule, PlaceholderService};

pub(crate) use crate::translation::placeholder::{
    CompiledPlaceholderRules, Pcre2PlaceholderConstructionError, PlaceholderProtectionError,
    PlaceholderRuleCompilationError, PlaceholderRuleDefinition, ProtectedText,
};

const MV_BUILTIN_CONTROL_PATTERN: &str =
    r"\\(?:[Nn]<[^\x00-\x1F\x7F-\x9F>]*>|[VvNnPpCcIi]\[[0-9]+\]|[Gg]|[\\{}$.|!><^])";
const MZ_BUILTIN_CONTROL_PATTERN: &str = r"\\(?:[Nn]<[^\x00-\x1F\x7F-\x9F>]*>|(?:[VvNnPpCcIi]|[Pp][Xx]|[Pp][Yy]|[Ff][Ss])\[[0-9]+\]|[Gg]|[\\{}$.|!><^])";
const BUILTIN_SEMANTIC_LABEL: &str = "RPG_MAKER_CONTROL";

/// RPG Maker 的封闭 scope 与 MV/MZ 内置控制符入口。
#[derive(Clone)]
pub(crate) struct Pcre2PlaceholderService {
    common: PlaceholderService,
    mv_builtin: Arc<CompiledBuiltinPlaceholderRule>,
    mz_builtin: Arc<CompiledBuiltinPlaceholderRule>,
}

impl Pcre2PlaceholderService {
    #[cfg(test)]
    pub(crate) fn new() -> Result<Self, Pcre2PlaceholderConstructionError> {
        match Self::new_with_cancellation(|| Ok::<_, Infallible>(())) {
            Ok(result) => result,
            Err(unreachable) => match unreachable {},
        }
    }

    pub(crate) fn new_with_cancellation<E>(
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Result<Self, Pcre2PlaceholderConstructionError>, E> {
        ensure_running()?;
        let common = PlaceholderService;
        let mv_builtin =
            match common.compile_builtin(MV_BUILTIN_CONTROL_PATTERN, BUILTIN_SEMANTIC_LABEL) {
                Ok(rule) => rule,
                Err(source) => return Ok(Err(source)),
            };
        ensure_running()?;
        let mz_builtin =
            match common.compile_builtin(MZ_BUILTIN_CONTROL_PATTERN, BUILTIN_SEMANTIC_LABEL) {
                Ok(rule) => rule,
                Err(source) => return Ok(Err(source)),
            };
        ensure_running()?;
        Ok(Ok(Self {
            mv_builtin: Arc::new(mv_builtin),
            mz_builtin: Arc::new(mz_builtin),
            common,
        }))
    }

    #[cfg(test)]
    pub(crate) fn compile_custom(
        &self,
        definitions: Vec<PlaceholderRuleDefinition>,
    ) -> Result<CompiledPlaceholderRules, PlaceholderRuleCompilationError> {
        self.common.compile_custom(definitions, |scope| {
            TextGroupKind::from_storage_name(scope).is_some()
        })
    }

    pub(crate) fn compile_custom_with_cancellation<E>(
        &self,
        definitions: Vec<PlaceholderRuleDefinition>,
        ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Result<CompiledPlaceholderRules, PlaceholderRuleCompilationError>, E> {
        self.common.compile_custom_with_cancellation(
            definitions,
            |scope| TextGroupKind::from_storage_name(scope).is_some(),
            ensure_running,
        )
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

    #[cfg(test)]
    pub(crate) fn protect_with_line_boundaries(
        &self,
        engine: RpgMakerEngine,
        kind: TextGroupKind,
        original: &str,
        line_separator_offsets: &[usize],
        custom: &CompiledPlaceholderRules,
    ) -> Result<ProtectedText, PlaceholderProtectionError> {
        match self.protect_with_line_boundaries_with_cancellation(
            engine,
            kind,
            original,
            line_separator_offsets,
            custom,
            || Ok::<_, Infallible>(()),
        ) {
            Ok(result) => result,
            Err(unreachable) => match unreachable {},
        }
    }

    pub(crate) fn protect_with_line_boundaries_with_cancellation<E>(
        &self,
        engine: RpgMakerEngine,
        kind: TextGroupKind,
        original: &str,
        line_separator_offsets: &[usize],
        custom: &CompiledPlaceholderRules,
        ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Result<ProtectedText, PlaceholderProtectionError>, E> {
        let builtin = match engine {
            RpgMakerEngine::Mv => self.mv_builtin.as_ref(),
            RpgMakerEngine::Mz => self.mz_builtin.as_ref(),
        };
        self.common.protect_with_cancellation(
            kind.storage_name(),
            original,
            line_separator_offsets,
            custom,
            Some(builtin),
            ensure_running,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::translation::placeholder::{PlaceholderRuleOrigin, PlaceholderSegment};

    #[test]
    fn clone_shares_compiled_builtin_rules() {
        let service = Pcre2PlaceholderService::new().expect("固定规则应可编译");
        let cloned = service.clone();

        assert!(Arc::ptr_eq(&service.mv_builtin, &cloned.mv_builtin));
        assert!(Arc::ptr_eq(&service.mz_builtin, &cloned.mz_builtin));
    }

    #[test]
    fn construction_observes_cancellation_between_builtin_rules() {
        let mut polls = 0_usize;
        let result = Pcre2PlaceholderService::new_with_cancellation(|| {
            polls += 1;
            if polls >= 2 { Err("cancelled") } else { Ok(()) }
        });

        assert!(matches!(result, Err("cancelled")));
        assert_eq!(polls, 2);
    }

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

    #[test]
    fn name_box_controls_are_builtin_and_require_a_closed_safe_body() {
        let service = Pcre2PlaceholderService::new().unwrap();
        let custom = service.compile_custom(Vec::new()).unwrap();
        let protected = service
            .protect(
                RpgMakerEngine::Mv,
                TextGroupKind::EventDialogue,
                r"\n<\n[145]>Hello",
                &custom,
            )
            .unwrap();
        let (_, bindings) = protected.into_parts();
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].original(), r"\n<\n[145]>");
        assert_eq!(bindings[0].origin(), PlaceholderRuleOrigin::BuiltIn);

        let unclosed = service
            .protect(
                RpgMakerEngine::Mz,
                TextGroupKind::EventDialogue,
                r"\n<Actor Hello",
                &custom,
            )
            .unwrap();
        assert!(unclosed.placeholders().is_empty());

        let control_in_body = service
            .protect(
                RpgMakerEngine::Mv,
                TextGroupKind::EventDialogue,
                "\\n<A\u{0085}lice>Hello",
                &custom,
            )
            .unwrap();
        assert!(control_in_body.placeholders().is_empty());
    }
}
