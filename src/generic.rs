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

pub(crate) use placeholder::{
    GenericCompiledPlaceholderRules, GenericPlaceholderError, GenericPlaceholderService,
    validate_translation_placeholders_with_cancellation,
};
#[cfg(test)]
pub(crate) use project::create_current_generic_schema_for_test;
pub(crate) use project::{
    CommitTranslationResultsOutcome, CommitTranslationsOutcome, ExtractOutcome, GenericInitRequest,
    GenericProject, GenericProjectError, GenericProjectStore, RejectedTranslationWrite,
    TranslationWrite, ensure_input_fingerprints_current_with_cancellation,
    validate_current_generic_schema_with_cancellation,
};
pub(crate) use task_record::{GenericTaskRecordDocument, GenericTaskRecordState};
pub(crate) use translate::{
    GenericPlaceholderRuleSource, GenericPlanningError, GenericPlanningUnitLocator,
    GenericPreparationError, GenericUnitLocator, GenericUnitMap, GenericValidationFact,
    PlannedTask, PreparedGenericTranslation, ResponseProblem, TranslationReview,
    accept_generic_response_with_cancellation, clone_generic_cpu_text,
    collect_generic_current_translations, ensure_generic_cpu_running,
    ensure_generic_response_processing_running, generic_cpu_text_equal,
    generic_language_projection_problem, generic_placeholder_multiset_problem,
    prepare_generic_translation, readable_generic_unit_id,
};
pub(crate) use write_back::{
    GenericWriteBackCandidate, GenericWriteBackError, GenericWriteBackTextOptions,
    build_write_back_candidate_with_cancellation, compile_generic_layout_rules,
    validate_materialized_write_back_file_with_cancellation,
};

#[cfg(test)]
pub(crate) use placeholder::GenericPlaceholderRuleDefinition;
#[cfg(test)]
pub(crate) use translate::{
    GenericUnitKey, automatic_translation_state_fingerprint,
    current_translation_for_stored_with_cancellation,
};
#[cfg(test)]
pub(crate) use write_back::{GenericCurrentTranslation, build_write_back_candidate};
