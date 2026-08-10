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

pub(crate) use crate::translation::TranslationOrigin;
pub(crate) use identity::CancellableTextMap;
pub(crate) use placeholder::{
    GenericCompiledPlaceholderRules, GenericPlaceholderError, GenericPlaceholderService,
    GenericProtectedText, validate_translation_placeholders_with_cancellation,
};
pub(crate) use project::{
    CommitTranslationResultsOutcome, CommitTranslationsOutcome, ExtractOutcome, GenericInitRequest,
    GenericProject, GenericProjectError, GenericProjectStore, GenericStoredSnapshot,
    RejectedTranslationWrite, TranslationWrite,
    ensure_input_fingerprints_current_with_cancellation,
};
pub(crate) use task_record::{GenericTaskRecordDocument, GenericTaskRecordState};
pub(crate) use translate::{
    AutomaticStateResources, GenericPlanningError, GenericPlanningUnitLocator, GenericUnitKey,
    GenericUnitMap, PlannedTask, PlanningUnit, ResponseProblem, TranslationAcceptance,
    TranslationPlan, TranslationReview, ValidatedReuse, accept_parsed_response_with_cancellation,
    current_translation_for_stored_with_cancellation,
    plan_translation_with_validator_and_cancellation, readable_generic_unit_id,
    terminology_hit_fingerprint_with_cancellation,
};
pub(crate) use write_back::{
    GenericCurrentTranslation, GenericWriteBackCandidate, GenericWriteBackError,
    build_write_back_candidate_with_cancellation,
    validate_materialized_write_back_file_with_cancellation,
};

#[cfg(test)]
pub(crate) use placeholder::GenericPlaceholderRuleDefinition;
#[cfg(test)]
pub(crate) use translate::automatic_translation_state_fingerprint;
#[cfg(test)]
pub(crate) use write_back::build_write_back_candidate;
