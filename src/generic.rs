//! 面向任意文本来源的 Generic JSONL 翻译领域。
//!
//! 外部操作者负责生成和消费 JSONL；本模块只拥有固定 JSONL 契约、动态来源同步、
//! 翻译任务规划、结果验收以及 JSONL 候选输出。

mod identity;
mod jsonl;
mod placeholder;
mod project;
mod task_record;
mod translate;
mod write_back;

pub(crate) use identity::CancellableTextMap;
pub(crate) use placeholder::{
    GenericCompiledPlaceholderRules, GenericPlaceholderError, GenericPlaceholderService,
    GenericProtectedText, validate_translation_placeholders_and_binding_with_cancellation,
    validate_translation_placeholders_with_cancellation,
};
pub(crate) use project::{
    CommitTranslationsOutcome, ExtractOutcome, GenericCompiledPlaceholderResource,
    GenericCompiledTerminologyResource, GenericInitRequest, GenericProject, GenericProjectError,
    GenericProjectStore, GenericStoredSnapshot, TranslationOrigin, TranslationWrite,
    compiled_placeholder_resource_for_connection_with_cancellation,
    compiled_terminology_resource_for_connection_with_cancellation,
    ensure_input_fingerprints_current_with_cancellation,
    validate_current_generic_schema_with_cancellation,
    validate_project_connection_with_compiled_resources_and_cancellation,
    validated_manual_translation_state_with_compiled_rules_for_connection_with_cancellation,
};
pub(crate) use task_record::{
    GenericTaskRecordDocument, GenericTaskRecordIssue, GenericTaskRecordState,
    GenericTaskResponseRecord,
};
pub(crate) use translate::{
    AutomaticStateResources, GenericPlanningError, GenericUnitKey, GenericUnitMap, PlannedGroup,
    PlannedTask, PlanningUnit, ResponseProblem, TranslationAcceptance, TranslationPlan,
    ValidatedReuse, accept_parsed_response_with_cancellation,
    current_translation_for_stored_with_cancellation,
    plan_translation_with_validator_and_cancellation,
    terminology_hit_fingerprint_with_cancellation,
};
pub(crate) use write_back::{
    GenericWriteBackCandidate, GenericWriteBackError, build_write_back_candidate_with_cancellation,
    validate_materialized_write_back_file_with_cancellation,
};

#[cfg(test)]
pub(crate) use placeholder::{GenericPlaceholderRuleDefinition, validate_translation_placeholders};
#[cfg(test)]
pub(crate) use project::manual_translation_state_for_connection;
#[cfg(test)]
pub(crate) use translate::{
    automatic_translation_state_fingerprint, manual_translation_state_fingerprint,
};
#[cfg(test)]
pub(crate) use write_back::build_write_back_candidate;
