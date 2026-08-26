//! RPG Maker 对公共 Placeholder 算法的 scope 与内置控制符适配。

use std::collections::HashSet;
#[cfg(test)]
use std::convert::Infallible;
use std::sync::Arc;

use crate::rpg_maker::RpgMakerEngine;
use crate::rpg_maker::asset::RpgMakerAssetOwner;
use crate::rpg_maker::model::TextUnitRole;
use crate::rpg_maker::text::{RpgMakerLocation, RpgMakerSource, StandardDataFile, TextGroupKind};
use crate::translation::placeholder::{
    CompiledBuiltinPlaceholderRule, PlaceholderOrderPolicy, PlaceholderService,
    candidate_placeholder_bindings_are_source_subset,
};
use crate::translation::placeholder_projection::SourceBoundPlaceholderError;

use super::pipeline::TranslationUnitIdentity;

pub(crate) use crate::translation::placeholder::{
    CompiledPlaceholderRules, Pcre2PlaceholderConstructionError, PlaceholderProtectionError,
    PlaceholderRuleCompilationError, PlaceholderRuleDefinition, ProtectedText,
};

const MV_EXTENDED_CONTROL_PATTERN: &str =
    r"(?:\\|\x1B)(?:(?:\\|\x1B)|[VvNnPp]\[[0-9]+\]|[CcIi](?:\[[0-9]+\]|(?![A-Za-z]))|[Gg{}])";
const MZ_EXTENDED_CONTROL_PATTERN: &str = r"(?:\\|\x1B)(?:(?:\\|\x1B)|[VvNnPp]\[[0-9]+\]|(?:[CcIi]|[Pp][Xx]|[Pp][Yy]|[Ff][Ss])(?:\[[0-9]+\]|(?![A-Za-z]))|[Gg{}])";
const MESSAGE_CONTROL_PATTERN: &str = r"(?:(?:\\|\x1B)[$.|!><^]|\x0C)";
const FORMAT_ARGUMENT_PATTERN: &str = r"%[0-9]+";
const EXTENDED_SEMANTIC_LABEL: &str = "RPG_MAKER_EXTENDED_CONTROL";
const MESSAGE_SEMANTIC_LABEL: &str = "RPG_MAKER_MESSAGE_CONTROL";
const FORMAT_SEMANTIC_LABEL: &str = "RPG_MAKER_FORMAT_ARGUMENT";

#[derive(Debug)]
pub(crate) enum RpgMakerSourceBoundPlaceholderError {
    Protection(PlaceholderProtectionError),
    Binding(SourceBoundPlaceholderError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RpgMakerTextConsumer {
    Plain,
    Extended,
    Message,
}

/// 只由 ATT Builtin 已确认的标准物理位置推导出的默认控制符范围。
///
/// Rules 与插件参数没有标准消费者证明，始终得到 Plain；它们只能使用精确 ID 的
/// Custom 规则。`format_arguments` 表示当前位置确定调用了 `String.format`，源中
/// 即使出现 `%0` 或超出实参数量的 `%N` 也仍由运行时消费，因而照常保护。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerBuiltinPlaceholderProfile {
    consumer: RpgMakerTextConsumer,
    format_arguments: bool,
}

impl RpgMakerBuiltinPlaceholderProfile {
    const PLAIN: Self = Self {
        consumer: RpgMakerTextConsumer::Plain,
        format_arguments: false,
    };
    const EXTENDED: Self = Self {
        consumer: RpgMakerTextConsumer::Extended,
        format_arguments: false,
    };
    const MESSAGE: Self = Self {
        consumer: RpgMakerTextConsumer::Message,
        format_arguments: false,
    };

    const fn with_format(mut self) -> Self {
        self.format_arguments = true;
        self
    }

    pub(crate) fn for_identity(engine: RpgMakerEngine, identity: &TranslationUnitIdentity) -> Self {
        Self::for_location(
            engine,
            identity.owner(),
            identity.kind(),
            identity.group_location(),
            identity.role(),
        )
    }

    pub(crate) fn for_location(
        engine: RpgMakerEngine,
        owner: RpgMakerAssetOwner,
        kind: TextGroupKind,
        location: &RpgMakerLocation,
        role: &TextUnitRole,
    ) -> Self {
        if owner != RpgMakerAssetOwner::Builtin {
            return Self::PLAIN;
        }
        match kind {
            TextGroupKind::DatabaseEntry => database_profile(engine, location, role),
            TextGroupKind::System => system_profile(location, role),
            TextGroupKind::EventDialogue
            | TextGroupKind::EventChoices
            | TextGroupKind::EventScrollingText
            | TextGroupKind::EventCommand
                if is_standard_event_source(location.source()) =>
            {
                event_profile(engine, kind, role)
            }
            TextGroupKind::Map
            | TextGroupKind::EventDialogue
            | TextGroupKind::EventChoices
            | TextGroupKind::EventScrollingText
            | TextGroupKind::EventCommand
            | TextGroupKind::PluginParameter => Self::PLAIN,
        }
    }

    pub(crate) const fn fingerprint_name(self) -> &'static str {
        match (self.consumer, self.format_arguments) {
            (RpgMakerTextConsumer::Plain, false) => "plain",
            (RpgMakerTextConsumer::Plain, true) => "plain+format",
            (RpgMakerTextConsumer::Extended, false) => "extended",
            (RpgMakerTextConsumer::Extended, true) => "extended+format",
            (RpgMakerTextConsumer::Message, false) => "message",
            (RpgMakerTextConsumer::Message, true) => "message+format",
        }
    }
}

fn database_profile(
    engine: RpgMakerEngine,
    location: &RpgMakerLocation,
    role: &TextUnitRole,
) -> RpgMakerBuiltinPlaceholderProfile {
    let RpgMakerSource::Data(file) = location.source() else {
        return RpgMakerBuiltinPlaceholderProfile::PLAIN;
    };
    let Some(field) = scalar_field(role) else {
        return RpgMakerBuiltinPlaceholderProfile::PLAIN;
    };
    match (*file, field) {
        (StandardDataFile::Skills, "message1" | "message2") => {
            RpgMakerBuiltinPlaceholderProfile::EXTENDED.with_format()
        }
        (StandardDataFile::States, "message1" | "message4") => {
            let profile = RpgMakerBuiltinPlaceholderProfile::MESSAGE;
            if engine == RpgMakerEngine::Mz {
                profile.with_format()
            } else {
                profile
            }
        }
        (StandardDataFile::States, "message2" | "message3") => {
            let profile = RpgMakerBuiltinPlaceholderProfile::EXTENDED;
            if engine == RpgMakerEngine::Mz {
                profile.with_format()
            } else {
                profile
            }
        }
        (
            StandardDataFile::Actors
            | StandardDataFile::Enemies
            | StandardDataFile::Skills
            | StandardDataFile::Items
            | StandardDataFile::Weapons
            | StandardDataFile::Armors,
            "name",
        ) => RpgMakerBuiltinPlaceholderProfile::MESSAGE,
        (StandardDataFile::Actors, "profile")
        | (
            StandardDataFile::Skills
            | StandardDataFile::Items
            | StandardDataFile::Weapons
            | StandardDataFile::Armors,
            "description",
        ) => RpgMakerBuiltinPlaceholderProfile::EXTENDED,
        _ => RpgMakerBuiltinPlaceholderProfile::PLAIN,
    }
}

fn system_profile(
    location: &RpgMakerLocation,
    role: &TextUnitRole,
) -> RpgMakerBuiltinPlaceholderProfile {
    if location.source() != &RpgMakerSource::Data(StandardDataFile::System) {
        return RpgMakerBuiltinPlaceholderProfile::PLAIN;
    }
    let Some(field) = scalar_field(role) else {
        return RpgMakerBuiltinPlaceholderProfile::PLAIN;
    };
    match field {
        "terms.basic[0]" | "terms.basic[8]" => RpgMakerBuiltinPlaceholderProfile::MESSAGE,
        "terms.basic[2]" | "terms.basic[4]" | "terms.basic[6]" => {
            RpgMakerBuiltinPlaceholderProfile::EXTENDED
        }
        field if indexed_system_field(field, "terms.params") => {
            RpgMakerBuiltinPlaceholderProfile::EXTENDED
        }
        field => system_message_profile(field),
    }
}

fn system_message_profile(field: &str) -> RpgMakerBuiltinPlaceholderProfile {
    let Some(key) = field.strip_prefix("terms.messages.") else {
        return RpgMakerBuiltinPlaceholderProfile::PLAIN;
    };
    let format = matches!(
        key,
        "expTotal"
            | "expNext"
            | "partyName"
            | "emerge"
            | "preemptive"
            | "surprise"
            | "escapeStart"
            | "victory"
            | "defeat"
            | "obtainGold"
            | "obtainItem"
            | "obtainSkill"
            | "actorNoDamage"
            | "actorNoHit"
            | "enemyNoDamage"
            | "enemyNoHit"
            | "evasion"
            | "magicEvasion"
            | "magicReflection"
            | "counterAttack"
            | "actionFailure"
            | "obtainExp"
            | "useItem"
            | "actorDamage"
            | "enemyDamage"
            | "substitute"
            | "buffAdd"
            | "debuffAdd"
            | "buffRemove"
            | "levelUp"
            | "actorRecovery"
            | "actorGain"
            | "actorLoss"
            | "actorDrain"
            | "enemyRecovery"
            | "enemyGain"
            | "enemyLoss"
            | "enemyDrain"
    );
    let profile = if matches!(
        key,
        "emerge"
            | "partyName"
            | "preemptive"
            | "surprise"
            | "victory"
            | "defeat"
            | "escapeStart"
            | "escapeFailure"
            | "obtainExp"
            | "obtainGold"
            | "obtainItem"
            | "levelUp"
            | "obtainSkill"
    ) {
        RpgMakerBuiltinPlaceholderProfile::MESSAGE
    } else if matches!(
        key,
        "saveMessage"
            | "loadMessage"
            | "criticalToEnemy"
            | "criticalToActor"
            | "actorNoDamage"
            | "actorNoHit"
            | "enemyNoDamage"
            | "enemyNoHit"
            | "evasion"
            | "magicEvasion"
            | "magicReflection"
            | "counterAttack"
            | "actionFailure"
            | "useItem"
            | "actorDamage"
            | "enemyDamage"
            | "substitute"
            | "buffAdd"
            | "debuffAdd"
            | "buffRemove"
            | "actorRecovery"
            | "actorGain"
            | "actorLoss"
            | "actorDrain"
            | "enemyRecovery"
            | "enemyGain"
            | "enemyLoss"
            | "enemyDrain"
    ) {
        RpgMakerBuiltinPlaceholderProfile::EXTENDED
    } else {
        RpgMakerBuiltinPlaceholderProfile::PLAIN
    };
    if format {
        profile.with_format()
    } else {
        profile
    }
}

fn event_profile(
    engine: RpgMakerEngine,
    kind: TextGroupKind,
    role: &TextUnitRole,
) -> RpgMakerBuiltinPlaceholderProfile {
    match (kind, role) {
        (TextGroupKind::EventDialogue, TextUnitRole::DialogueBody) => {
            RpgMakerBuiltinPlaceholderProfile::MESSAGE
        }
        (TextGroupKind::EventDialogue, TextUnitRole::DialogueSpeaker)
            if engine == RpgMakerEngine::Mz =>
        {
            RpgMakerBuiltinPlaceholderProfile::EXTENDED
        }
        (TextGroupKind::EventChoices, TextUnitRole::Choices)
        | (TextGroupKind::EventScrollingText, TextUnitRole::ScrollingText) => {
            RpgMakerBuiltinPlaceholderProfile::EXTENDED
        }
        (TextGroupKind::EventCommand, TextUnitRole::Scalar(field)) if field.as_str() == "name" => {
            RpgMakerBuiltinPlaceholderProfile::MESSAGE
        }
        (TextGroupKind::EventCommand, TextUnitRole::Scalar(field))
            if field.as_str() == "profile" =>
        {
            RpgMakerBuiltinPlaceholderProfile::EXTENDED
        }
        _ => RpgMakerBuiltinPlaceholderProfile::PLAIN,
    }
}

fn scalar_field(role: &TextUnitRole) -> Option<&str> {
    match role {
        TextUnitRole::Scalar(field) => Some(field.as_str()),
        _ => None,
    }
}

fn indexed_system_field(field: &str, prefix: &str) -> bool {
    field
        .strip_prefix(prefix)
        .and_then(|suffix| suffix.strip_prefix('['))
        .and_then(|suffix| suffix.strip_suffix(']'))
        .is_some_and(|index| !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit()))
}

fn is_standard_event_source(source: &RpgMakerSource) -> bool {
    matches!(
        source,
        RpgMakerSource::Map(_)
            | RpgMakerSource::Data(StandardDataFile::CommonEvents | StandardDataFile::Troops)
    )
}

/// RPG Maker 的封闭 scope 与 MV/MZ 内置控制符入口。
#[derive(Clone)]
pub(crate) struct Pcre2PlaceholderService {
    common: PlaceholderService,
    mv_extended: Arc<CompiledBuiltinPlaceholderRule>,
    mz_extended: Arc<CompiledBuiltinPlaceholderRule>,
    message: Arc<CompiledBuiltinPlaceholderRule>,
    format_arguments: Arc<CompiledBuiltinPlaceholderRule>,
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
        let mv_extended =
            match common.compile_builtin(MV_EXTENDED_CONTROL_PATTERN, EXTENDED_SEMANTIC_LABEL) {
                Ok(rule) => rule,
                Err(source) => return Ok(Err(source)),
            };
        ensure_running()?;
        let mz_extended =
            match common.compile_builtin(MZ_EXTENDED_CONTROL_PATTERN, EXTENDED_SEMANTIC_LABEL) {
                Ok(rule) => rule,
                Err(source) => return Ok(Err(source)),
            };
        ensure_running()?;
        let message = match common.compile_builtin(MESSAGE_CONTROL_PATTERN, MESSAGE_SEMANTIC_LABEL)
        {
            Ok(rule) => rule,
            Err(source) => return Ok(Err(source)),
        };
        ensure_running()?;
        let format_arguments = match common.compile_builtin_with_order_policy(
            FORMAT_ARGUMENT_PATTERN,
            FORMAT_SEMANTIC_LABEL,
            PlaceholderOrderPolicy::ReorderWithinSlot,
        ) {
            Ok(rule) => rule,
            Err(source) => return Ok(Err(source)),
        };
        ensure_running()?;
        Ok(Ok(Self {
            mv_extended: Arc::new(mv_extended),
            mz_extended: Arc::new(mz_extended),
            message: Arc::new(message),
            format_arguments: Arc::new(format_arguments),
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

    pub(crate) fn compile_custom_for_ids_with_cancellation<E>(
        &self,
        definitions: Vec<PlaceholderRuleDefinition>,
        valid_ids: &HashSet<String>,
        ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Result<CompiledPlaceholderRules, PlaceholderRuleCompilationError>, E> {
        self.common.compile_custom_with_targets_and_cancellation(
            definitions,
            |scope| TextGroupKind::from_storage_name(scope).is_some(),
            |id| valid_ids.contains(id),
            ensure_running,
        )
    }

    #[cfg(test)]
    pub(crate) fn protect(
        &self,
        engine: RpgMakerEngine,
        identity: &TranslationUnitIdentity,
        original: &str,
        custom: &CompiledPlaceholderRules,
    ) -> Result<ProtectedText, PlaceholderProtectionError> {
        self.protect_identity_with_line_boundaries(engine, identity, original, &[], custom)
    }

    #[cfg(test)]
    pub(crate) fn protect_identity_with_line_boundaries(
        &self,
        engine: RpgMakerEngine,
        identity: &TranslationUnitIdentity,
        original: &str,
        line_separator_offsets: &[usize],
        custom: &CompiledPlaceholderRules,
    ) -> Result<ProtectedText, PlaceholderProtectionError> {
        match self.protect_identity_with_line_boundaries_with_cancellation(
            engine,
            identity,
            original,
            line_separator_offsets,
            custom,
            || Ok::<_, Infallible>(()),
        ) {
            Ok(result) => result,
            Err(unreachable) => match unreachable {},
        }
    }

    #[cfg(test)]
    pub(crate) fn protect_identity_with_line_boundaries_with_cancellation<E>(
        &self,
        engine: RpgMakerEngine,
        identity: &TranslationUnitIdentity,
        original: &str,
        line_separator_offsets: &[usize],
        custom: &CompiledPlaceholderRules,
        ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Result<ProtectedText, PlaceholderProtectionError>, E> {
        self.protect_profile_with_cancellation(
            engine,
            identity.kind(),
            &identity.readable_id(),
            RpgMakerBuiltinPlaceholderProfile::for_identity(engine, identity),
            original,
            line_separator_offsets,
            custom,
            ensure_running,
        )
    }

    // 引擎语义、精确目标、文本边界和当前规则都是一次保护操作的直接输入。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn protect_profile_with_cancellation<E>(
        &self,
        engine: RpgMakerEngine,
        kind: TextGroupKind,
        target_id: &str,
        profile: RpgMakerBuiltinPlaceholderProfile,
        original: &str,
        line_separator_offsets: &[usize],
        custom: &CompiledPlaceholderRules,
        ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Result<ProtectedText, PlaceholderProtectionError>, E> {
        let mut builtins = Vec::with_capacity(3);
        if matches!(
            profile.consumer,
            RpgMakerTextConsumer::Extended | RpgMakerTextConsumer::Message
        ) {
            builtins.push(match engine {
                RpgMakerEngine::Mv => self.mv_extended.as_ref(),
                RpgMakerEngine::Mz => self.mz_extended.as_ref(),
            });
        }
        if profile.consumer == RpgMakerTextConsumer::Message {
            builtins.push(self.message.as_ref());
        }
        if profile.format_arguments {
            builtins.push(self.format_arguments.as_ref());
        }
        self.common
            .protect_with_target_and_builtins_with_cancellation(
                kind.storage_name(),
                Some(target_id),
                original,
                line_separator_offsets,
                custom,
                &builtins,
                ensure_running,
            )
    }

    /// 使用源文保护阶段的 binding 验收候选；完整规则扫描只拒绝源 binding 之外的新身份。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn bind_profile_candidate_with_cancellation<E>(
        &self,
        source: &ProtectedText,
        engine: RpgMakerEngine,
        kind: TextGroupKind,
        target_id: &str,
        profile: RpgMakerBuiltinPlaceholderProfile,
        candidate: &str,
        custom: &CompiledPlaceholderRules,
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Result<ProtectedText, RpgMakerSourceBoundPlaceholderError>, E> {
        if source.placeholders().is_empty() {
            let candidate = match self.protect_profile_with_cancellation(
                engine,
                kind,
                target_id,
                profile,
                candidate,
                &[],
                custom,
                &mut ensure_running,
            )? {
                Ok(candidate) => candidate,
                Err(source) => {
                    return Ok(Err(RpgMakerSourceBoundPlaceholderError::Protection(source)));
                }
            };
            if candidate.placeholders().is_empty() {
                ensure_running()?;
                return Ok(Ok(candidate));
            }
            return Ok(Err(RpgMakerSourceBoundPlaceholderError::Binding(
                SourceBoundPlaceholderError::UnexpectedPlaceholder,
            )));
        }
        let discovered = match self.protect_profile_with_cancellation(
            engine,
            kind,
            target_id,
            profile,
            candidate,
            &[],
            custom,
            &mut ensure_running,
        )? {
            Ok(discovered) => discovered,
            Err(source) => {
                return Ok(Err(RpgMakerSourceBoundPlaceholderError::Protection(source)));
            }
        };
        let candidate =
            match source.bind_candidate_with_cancellation(candidate, &mut ensure_running)? {
                Ok(candidate) => candidate,
                Err(source) => {
                    return Ok(Err(RpgMakerSourceBoundPlaceholderError::Binding(source)));
                }
            };
        if !candidate_placeholder_bindings_are_source_subset(
            source.placeholders(),
            discovered.placeholders(),
        ) {
            return Ok(Err(RpgMakerSourceBoundPlaceholderError::Binding(
                SourceBoundPlaceholderError::UnexpectedPlaceholder,
            )));
        }
        ensure_running()?;
        Ok(Ok(candidate))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpg_maker::model::{ScalarFieldKey, TextUnitContent};
    use crate::rpg_maker::text::RpgMakerLocationStep;
    use crate::translation::placeholder::{PlaceholderRuleOrigin, PlaceholderSegment};

    fn identity(
        owner: RpgMakerAssetOwner,
        kind: TextGroupKind,
        source: RpgMakerSource,
        role: TextUnitRole,
        original: &str,
    ) -> TranslationUnitIdentity {
        let content = if role.expects_lines() {
            TextUnitContent::Lines(vec![original.to_owned()])
        } else {
            TextUnitContent::Value(original.to_owned())
        };
        TranslationUnitIdentity::new(
            owner,
            kind,
            RpgMakerLocation::value(
                source,
                vec![
                    RpgMakerLocationStep::key("test"),
                    RpgMakerLocationStep::index(0),
                ],
            ),
            role,
            content,
            "{}",
        )
    }

    fn scalar(value: &str) -> TextUnitRole {
        TextUnitRole::Scalar(ScalarFieldKey::new(value).expect("测试字段必须有效"))
    }

    fn protected_originals(
        service: &Pcre2PlaceholderService,
        engine: RpgMakerEngine,
        identity: &TranslationUnitIdentity,
        original: &str,
    ) -> Vec<String> {
        let custom = service.compile_custom(Vec::new()).unwrap();
        service
            .protect(engine, identity, original, &custom)
            .expect("测试原文应可保护")
            .placeholders()
            .iter()
            .map(|binding| binding.original().to_owned())
            .collect()
    }

    #[test]
    fn clone_shares_compiled_builtin_rules() {
        let service = Pcre2PlaceholderService::new().expect("固定规则应可编译");
        let cloned = service.clone();

        assert!(Arc::ptr_eq(&service.mv_extended, &cloned.mv_extended));
        assert!(Arc::ptr_eq(&service.mz_extended, &cloned.mz_extended));
        assert!(Arc::ptr_eq(&service.message, &cloned.message));
        assert!(Arc::ptr_eq(
            &service.format_arguments,
            &cloned.format_arguments
        ));
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
    fn rpg_adapter_rejects_unknown_scope() {
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
    }

    #[test]
    fn builtin_controls_follow_confirmed_standard_consumers() {
        let service = Pcre2PlaceholderService::new().unwrap();
        let dialogue = identity(
            RpgMakerAssetOwner::Builtin,
            TextGroupKind::EventDialogue,
            RpgMakerSource::map(1),
            TextUnitRole::DialogueBody,
            "",
        );
        assert_eq!(
            protected_originals(&service, RpgMakerEngine::Mv, &dialogue, "\\C[2]\\!\u{000C}"),
            [r"\C[2]", r"\!", "\u{000C}"]
        );

        let description = identity(
            RpgMakerAssetOwner::Builtin,
            TextGroupKind::DatabaseEntry,
            RpgMakerSource::data(StandardDataFile::Items),
            scalar("description"),
            "",
        );
        assert_eq!(
            protected_originals(&service, RpgMakerEngine::Mv, &description, r"\C[2]\!"),
            [r"\C[2]"]
        );

        let plugin = identity(
            RpgMakerAssetOwner::Builtin,
            TextGroupKind::PluginParameter,
            RpgMakerSource::plugin_parameter(0, "Unknown", "text"),
            scalar("text"),
            "",
        );
        assert!(
            protected_originals(&service, RpgMakerEngine::Mz, &plugin, r"\V[1] %1 \!").is_empty()
        );

        let rules_description = identity(
            RpgMakerAssetOwner::Rules,
            TextGroupKind::DatabaseEntry,
            RpgMakerSource::data(StandardDataFile::Items),
            scalar("description"),
            "",
        );
        assert!(
            protected_originals(&service, RpgMakerEngine::Mz, &rules_description, r"\C[2]")
                .is_empty()
        );
    }

    #[test]
    fn extended_controls_do_not_consume_unknown_ascii_command_prefixes() {
        let service = Pcre2PlaceholderService::new().unwrap();
        let dialogue = identity(
            RpgMakerAssetOwner::Builtin,
            TextGroupKind::EventDialogue,
            RpgMakerSource::map(1),
            TextUnitRole::DialogueBody,
            "",
        );

        assert_eq!(
            protected_originals(
                &service,
                RpgMakerEngine::Mv,
                &dialogue,
                r"\CENTER[1] \C \C[2] \I \I[3]"
            ),
            [r"\C", r"\C[2]", r"\I", r"\I[3]"]
        );
        assert_eq!(
            protected_originals(
                &service,
                RpgMakerEngine::Mz,
                &dialogue,
                r"\FSIZE[20] \FS \FS[20] \PX \PX[5] \PY \PY[6]"
            ),
            [r"\FS", r"\FS[20]", r"\PX", r"\PX[5]", r"\PY", r"\PY[6]"]
        );
    }

    #[test]
    fn format_and_message_controls_are_separated_by_system_key() {
        let service = Pcre2PlaceholderService::new().unwrap();
        let system = |field: &str| {
            identity(
                RpgMakerAssetOwner::Builtin,
                TextGroupKind::System,
                RpgMakerSource::data(StandardDataFile::System),
                scalar(field),
                "",
            )
        };

        assert_eq!(
            protected_originals(
                &service,
                RpgMakerEngine::Mz,
                &system("terms.messages.expTotal"),
                r"%1\!"
            ),
            ["%1"]
        );
        assert_eq!(
            protected_originals(
                &service,
                RpgMakerEngine::Mz,
                &system("terms.messages.escapeFailure"),
                r"%1\!"
            ),
            [r"\!"]
        );
        assert_eq!(
            protected_originals(
                &service,
                RpgMakerEngine::Mz,
                &system("terms.messages.partyName"),
                r"%1\!"
            ),
            ["%1", r"\!"]
        );
        assert_eq!(
            protected_originals(
                &service,
                RpgMakerEngine::Mz,
                &system("terms.messages.saveMessage"),
                r"\C[1]\!"
            ),
            [r"\C[1]"]
        );
    }

    #[test]
    fn state_format_and_speaker_contracts_are_engine_specific() {
        let service = Pcre2PlaceholderService::new().unwrap();
        let state_message = identity(
            RpgMakerAssetOwner::Builtin,
            TextGroupKind::DatabaseEntry,
            RpgMakerSource::data(StandardDataFile::States),
            scalar("message1"),
            "",
        );
        assert_eq!(
            protected_originals(&service, RpgMakerEngine::Mv, &state_message, r"%1\!"),
            [r"\!"]
        );
        assert_eq!(
            protected_originals(&service, RpgMakerEngine::Mz, &state_message, r"%1\!"),
            ["%1", r"\!"]
        );

        let speaker = identity(
            RpgMakerAssetOwner::Builtin,
            TextGroupKind::EventDialogue,
            RpgMakerSource::map(1),
            TextUnitRole::DialogueSpeaker,
            "",
        );
        assert!(protected_originals(&service, RpgMakerEngine::Mv, &speaker, r"\C[1]").is_empty());
        assert_eq!(
            protected_originals(&service, RpgMakerEngine::Mz, &speaker, r"\C[1]"),
            [r"\C[1]"]
        );
    }

    #[test]
    fn inline_name_box_wrapper_is_not_a_global_builtin() {
        let service = Pcre2PlaceholderService::new().unwrap();
        let dialogue = identity(
            RpgMakerAssetOwner::Builtin,
            TextGroupKind::EventDialogue,
            RpgMakerSource::map(1),
            TextUnitRole::DialogueBody,
            "",
        );
        assert!(
            protected_originals(&service, RpgMakerEngine::Mz, &dialogue, r"\N<Alice>Hello")
                .is_empty()
        );
    }

    #[test]
    fn builtin_bindings_keep_origin_segment_and_format_order_policy() {
        let service = Pcre2PlaceholderService::new().unwrap();
        let system = identity(
            RpgMakerAssetOwner::Builtin,
            TextGroupKind::System,
            RpgMakerSource::data(StandardDataFile::System),
            scalar("terms.messages.expTotal"),
            "",
        );
        let custom = service.compile_custom(Vec::new()).unwrap();
        let protected = service
            .protect(RpgMakerEngine::Mz, &system, "%1", &custom)
            .unwrap();
        let binding = &protected.placeholders()[0];
        assert_eq!(binding.origin(), PlaceholderRuleOrigin::BuiltIn);
        assert_eq!(binding.segment(), PlaceholderSegment::Whole);
        assert_eq!(
            binding.order_policy(),
            PlaceholderOrderPolicy::ReorderWithinSlot
        );
    }
}
