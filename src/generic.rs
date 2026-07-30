//! 面向任意文本来源的 Generic JSONL 翻译领域。
//!
//! 外部操作者负责生成和消费 JSONL；本模块只拥有固定 JSONL 契约、动态来源同步、
//! 翻译任务规划、结果验收以及 JSONL 候选输出。

mod jsonl;
mod placeholder;
mod project;
mod task_record;
mod translate;
mod write_back;

pub(crate) use placeholder::{
    GenericCompiledPlaceholderRules, GenericPlaceholderError, GenericPlaceholderService,
    GenericProtectedText, validate_translation_placeholders,
};
pub(crate) use project::{
    CommitTranslationsOutcome, ExtractOutcome, GenericInitRequest, GenericProject,
    GenericProjectError, GenericProjectStore, GenericStoredSnapshot, TranslationOrigin,
    ensure_input_fingerprints_current, manual_translation_state_for_connection,
    validate_project_connection, validated_manual_translation_state_for_connection,
};
pub(crate) use task_record::{
    GenericTaskRecordDocument, GenericTaskRecordIssue, GenericTaskRecordState,
    GenericTaskResponseRecord,
};
pub(crate) use translate::{
    AutomaticStateResources, GenericPlanningError, GenericUnitKey, PlannedGroup, PlannedTask,
    PlanningUnit, ResponseProblem, TranslationAcceptance, TranslationPlan, accept_parsed_response,
    current_translation_for_stored, plan_translation, split_tasks_by_rendered_size,
};
pub(crate) use write_back::{
    GenericWriteBackCandidate, GenericWriteBackError, build_write_back_candidate,
    validate_materialized_write_back_file,
};

#[cfg(test)]
pub(crate) use placeholder::GenericPlaceholderRuleDefinition;
#[cfg(test)]
pub(crate) use project::TranslationWrite;
#[cfg(test)]
pub(crate) use translate::{
    automatic_translation_state_fingerprint, manual_translation_state_fingerprint,
};
