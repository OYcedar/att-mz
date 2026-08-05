//! Project Lua 边界在仍掌握调用和事务事实时建立的封闭问题。

use serde::{Deserialize, Serialize};

use super::model::DiagnosticResolution;
use super::safe_value::{SafeIdentifier, SafePath};
use super::{DiagnosticStage, PlaceholderIssue};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LuaEngine {
    Generic,
    Mv,
    Mz,
}

impl LuaEngine {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Generic => "generic",
            Self::Mv => "mv",
            Self::Mz => "mz",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LuaOperation {
    CreateContext,
    CompileScript,
    ExecuteScript,
    ValidatePrerequisites,
    SetTranslation,
    ClearTranslation,
    QueryDatabase,
    ValidateDatabase,
    BeginTransaction,
    CommitTransaction,
    RollbackTransaction,
    InstallAuthorizer,
    RemoveAuthorizer,
}

impl LuaOperation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::CreateContext => "create_context",
            Self::CompileScript => "compile_script",
            Self::ExecuteScript => "execute_script",
            Self::ValidatePrerequisites => "validate_prerequisites",
            Self::SetTranslation => "set_translation",
            Self::ClearTranslation => "clear_translation",
            Self::QueryDatabase => "query_database",
            Self::ValidateDatabase => "validate_database",
            Self::BeginTransaction => "begin_transaction",
            Self::CommitTransaction => "commit_transaction",
            Self::RollbackTransaction => "rollback_transaction",
            Self::InstallAuthorizer => "install_authorizer",
            Self::RemoveAuthorizer => "remove_authorizer",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LuaValueViolation {
    MissingField,
    UnexpectedType,
    InvalidArrayIndex,
    InvalidTable,
    CyclicTable,
    SparseArray,
    InvalidUtf8,
    InvalidBlob,
    InvalidLocator,
    InvalidTranslation,
    UnknownUnit,
    StateMismatch,
    TransactionLost,
}

impl LuaValueViolation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::MissingField => "missing_field",
            Self::UnexpectedType => "unexpected_type",
            Self::InvalidArrayIndex => "invalid_array_index",
            Self::InvalidTable => "invalid_table",
            Self::CyclicTable => "cyclic_table",
            Self::SparseArray => "sparse_array",
            Self::InvalidUtf8 => "invalid_utf8",
            Self::InvalidBlob => "invalid_blob",
            Self::InvalidLocator => "invalid_locator",
            Self::InvalidTranslation => "invalid_translation",
            Self::UnknownUnit => "unknown_unit",
            Self::StateMismatch => "state_mismatch",
            Self::TransactionLost => "transaction_lost",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LuaTransactionState {
    NotStarted,
    RolledBack,
    RollbackOutcomeUnknown,
    CommitOutcomeUnknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LuaCompilerCategory {
    Syntax,
    Memory,
    Safety,
    Callback,
    External,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LuaContextProblem {
    InterruptRegistration,
    CancellationGuard,
    InstructionHook,
    ContextTable,
    PublishContext,
    Arguments,
    PrintBinding,
    RuntimeCreation,
    RemoveExternalCapability,
    ProtectedCallWrapper,
    ThreadCreation,
    ConcurrentSqliteWait,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum LuaCompilationProblem {
    InvalidIdentity,
    InvalidUtf8,
    Backend { category: LuaCompilerCategory },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum LuaScriptProblem {
    Yielded,
    NonErrorValue,
    Backend { category: LuaCompilerCategory },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LuaValidationProblem {
    AdapterState,
    ProtectedSchemaChanged,
    TemporarySchemaObject,
    ForeignKey,
    QuickCheck,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "engine", rename_all = "snake_case")]
pub(crate) enum LuaLocator {
    Generic {
        relative_path: Option<SafePath>,
        group_id: Option<SafeIdentifier>,
        unit_id: Option<SafeIdentifier>,
    },
    RpgMaker {
        owner: Option<SafeIdentifier>,
        group_location: Option<SafeIdentifier>,
        unit_role: Option<SafeIdentifier>,
    },
}

impl LuaCompilerCategory {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Syntax => "syntax",
            Self::Memory => "memory",
            Self::Safety => "safety",
            Self::Callback => "callback",
            Self::External => "external",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum LuaProblem {
    Cancelled,
    ContextCreation {
        problem: LuaContextProblem,
    },
    Compilation {
        script: SafePath,
        problem: LuaCompilationProblem,
        line: Option<usize>,
    },
    ScriptExecution {
        script: SafePath,
        problem: LuaScriptProblem,
    },
    HostCall {
        engine: LuaEngine,
        operation: LuaOperation,
        violation: LuaValueViolation,
        field: Option<SafeIdentifier>,
        locator: Option<LuaLocator>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        placeholder: Option<PlaceholderIssue>,
    },
    DatabasePrerequisite {
        engine: Option<LuaEngine>,
        violation: LuaValueViolation,
    },
    Validation {
        engine: LuaEngine,
        problem: LuaValidationProblem,
    },
    WorkerPanicked,
    TransactionFinalization {
        database_path: SafePath,
        state: LuaTransactionState,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LuaIssue {
    problem: LuaProblem,
}

impl LuaIssue {
    pub(crate) const fn new(problem: LuaProblem) -> Self {
        Self { problem }
    }

    const fn operation(&self) -> LuaOperation {
        match self.problem {
            LuaProblem::Cancelled | LuaProblem::ScriptExecution { .. } => {
                LuaOperation::ExecuteScript
            }
            LuaProblem::ContextCreation { .. } | LuaProblem::WorkerPanicked => {
                LuaOperation::CreateContext
            }
            LuaProblem::Compilation { .. } => LuaOperation::CompileScript,
            LuaProblem::HostCall { operation, .. } => operation,
            LuaProblem::DatabasePrerequisite { .. } => LuaOperation::ValidatePrerequisites,
            LuaProblem::Validation { .. } => LuaOperation::ValidateDatabase,
            LuaProblem::TransactionFinalization { state, .. } => match state {
                LuaTransactionState::NotStarted => LuaOperation::BeginTransaction,
                LuaTransactionState::RolledBack | LuaTransactionState::RollbackOutcomeUnknown => {
                    LuaOperation::RollbackTransaction
                }
                LuaTransactionState::CommitOutcomeUnknown => LuaOperation::CommitTransaction,
            },
        }
    }

    pub(crate) const fn stage(&self) -> DiagnosticStage {
        DiagnosticStage::Lua
    }

    pub(crate) const fn code(&self) -> &'static str {
        match &self.problem {
            LuaProblem::Cancelled => "lua.cancelled",
            LuaProblem::ContextCreation { .. } => "lua.context_creation",
            LuaProblem::Compilation { .. } => "lua.compilation",
            LuaProblem::ScriptExecution { .. } => "lua.script_execution",
            LuaProblem::HostCall { .. } => "lua.host_call",
            LuaProblem::DatabasePrerequisite { .. } => "lua.database_prerequisite",
            LuaProblem::Validation { .. } => "lua.validation",
            LuaProblem::WorkerPanicked => "lua.worker_panicked",
            LuaProblem::TransactionFinalization { state, .. } => match state {
                LuaTransactionState::NotStarted => "lua.transaction.not_started",
                LuaTransactionState::RolledBack => "lua.transaction.rolled_back",
                LuaTransactionState::RollbackOutcomeUnknown => {
                    "lua.transaction.rollback_outcome_unknown"
                }
                LuaTransactionState::CommitOutcomeUnknown => {
                    "lua.transaction.commit_outcome_unknown"
                }
            },
        }
    }

    pub(crate) const fn resolution(&self) -> DiagnosticResolution {
        match &self.problem {
            LuaProblem::Cancelled => DiagnosticResolution::Retry,
            LuaProblem::ContextCreation { .. } | LuaProblem::WorkerPanicked => {
                DiagnosticResolution::ReportBug
            }
            LuaProblem::HostCall {
                placeholder: Some(PlaceholderIssue::WorkerStart { .. }),
                ..
            } => DiagnosticResolution::Retry,
            LuaProblem::Compilation { .. }
            | LuaProblem::ScriptExecution { .. }
            | LuaProblem::HostCall { .. }
            | LuaProblem::Validation { .. } => DiagnosticResolution::FixInput,
            LuaProblem::DatabasePrerequisite { .. } => DiagnosticResolution::CheckProjectState,
            LuaProblem::TransactionFinalization { .. } => {
                DiagnosticResolution::PreserveRecoveryArtifacts
            }
        }
    }

    pub(crate) const fn summary_code(&self) -> &'static str {
        match &self.problem {
            LuaProblem::Cancelled => "lock_cancelled",
            LuaProblem::ContextCreation { .. } | LuaProblem::WorkerPanicked => "internal_invariant",
            LuaProblem::Compilation { .. } => "lua_compilation_failed",
            LuaProblem::ScriptExecution { .. } => "lua_execution_failed",
            LuaProblem::HostCall {
                placeholder: Some(problem),
                ..
            } => problem.summary_code(),
            LuaProblem::HostCall {
                placeholder: None, ..
            } => "lua_execution_failed",
            LuaProblem::DatabasePrerequisite { .. } | LuaProblem::Validation { .. } => {
                "state_mismatch"
            }
            LuaProblem::TransactionFinalization { .. } => "transaction_outcome_unknown",
        }
    }

    pub(crate) fn subject(&self) -> String {
        self.operation().as_str().to_owned()
    }

    pub(crate) fn facts(&self) -> Vec<(&'static str, String)> {
        let mut facts = vec![("operation", self.operation().as_str().to_owned())];
        match &self.problem {
            LuaProblem::Compilation {
                script,
                problem,
                line,
            } => {
                facts.push(("script", script.to_string()));
                facts.push((
                    "compilation_problem",
                    compilation_problem_name(*problem).to_owned(),
                ));
                if let LuaCompilationProblem::Backend { category } = problem {
                    facts.push(("compiler_category", category.as_str().to_owned()));
                }
                if let Some(line) = line {
                    facts.push(("line", line.to_string()));
                }
            }
            LuaProblem::ScriptExecution { script, problem } => {
                facts.push(("script", script.to_string()));
                facts.push(("script_problem", script_problem_name(*problem).to_owned()));
                if let LuaScriptProblem::Backend { category } = problem {
                    facts.push(("runtime_category", category.as_str().to_owned()));
                }
            }
            LuaProblem::HostCall {
                engine,
                operation: _,
                violation,
                field,
                locator,
                placeholder,
            } => {
                facts.push(("engine", engine.as_str().to_owned()));
                facts.push(("violation", violation.as_str().to_owned()));
                if let Some(field) = field {
                    facts.push(("field", field.to_string()));
                }
                if let Some(locator) = locator {
                    match locator {
                        LuaLocator::Generic {
                            relative_path,
                            group_id,
                            unit_id,
                        } => {
                            if let Some(relative_path) = relative_path {
                                facts.push(("relative_path", relative_path.to_string()));
                            }
                            if let Some(group_id) = group_id {
                                facts.push(("group_id", group_id.to_string()));
                            }
                            if let Some(unit_id) = unit_id {
                                facts.push(("unit_id", unit_id.to_string()));
                            }
                        }
                        LuaLocator::RpgMaker {
                            owner,
                            group_location,
                            unit_role,
                        } => {
                            if let Some(owner) = owner {
                                facts.push(("owner", owner.to_string()));
                            }
                            if let Some(group_location) = group_location {
                                facts.push(("group_location", group_location.to_string()));
                            }
                            if let Some(unit_role) = unit_role {
                                facts.push(("unit_role", unit_role.to_string()));
                            }
                        }
                    }
                }
                if let Some(problem) = placeholder {
                    facts.push(("placeholder_problem", problem.code().to_owned()));
                    facts.extend(problem.facts().into_iter().map(|(name, value)| {
                        let name = if name == "operation" {
                            "placeholder_operation"
                        } else {
                            name
                        };
                        (name, value)
                    }));
                }
            }
            LuaProblem::DatabasePrerequisite { engine, violation } => {
                if let Some(engine) = engine {
                    facts.push(("engine", engine.as_str().to_owned()));
                }
                facts.push(("violation", violation.as_str().to_owned()));
            }
            LuaProblem::Validation { engine, problem } => {
                facts.push(("engine", engine.as_str().to_owned()));
                facts.push((
                    "validation_problem",
                    validation_problem_name(*problem).to_owned(),
                ));
            }
            LuaProblem::ContextCreation { problem } => {
                facts.push(("context_problem", context_problem_name(*problem).to_owned()));
            }
            _ => {}
        }
        if let LuaProblem::TransactionFinalization {
            database_path,
            state,
            ..
        } = &self.problem
        {
            facts.push(("database_path", database_path.to_string()));
            facts.push((
                "transaction_state",
                transaction_state_name(*state).to_owned(),
            ));
        }
        facts
    }
}

const fn context_problem_name(problem: LuaContextProblem) -> &'static str {
    match problem {
        LuaContextProblem::InterruptRegistration => "interrupt_registration",
        LuaContextProblem::CancellationGuard => "cancellation_guard",
        LuaContextProblem::InstructionHook => "instruction_hook",
        LuaContextProblem::ContextTable => "context_table",
        LuaContextProblem::PublishContext => "publish_context",
        LuaContextProblem::Arguments => "arguments",
        LuaContextProblem::PrintBinding => "print_binding",
        LuaContextProblem::RuntimeCreation => "runtime_creation",
        LuaContextProblem::RemoveExternalCapability => "remove_external_capability",
        LuaContextProblem::ProtectedCallWrapper => "protected_call_wrapper",
        LuaContextProblem::ThreadCreation => "thread_creation",
        LuaContextProblem::ConcurrentSqliteWait => "concurrent_sqlite_wait",
    }
}

const fn compilation_problem_name(problem: LuaCompilationProblem) -> &'static str {
    match problem {
        LuaCompilationProblem::InvalidIdentity => "invalid_identity",
        LuaCompilationProblem::InvalidUtf8 => "invalid_utf8",
        LuaCompilationProblem::Backend { .. } => "backend",
    }
}

const fn script_problem_name(problem: LuaScriptProblem) -> &'static str {
    match problem {
        LuaScriptProblem::Yielded => "yielded",
        LuaScriptProblem::NonErrorValue => "non_error_value",
        LuaScriptProblem::Backend { .. } => "backend",
    }
}

const fn validation_problem_name(problem: LuaValidationProblem) -> &'static str {
    match problem {
        LuaValidationProblem::AdapterState => "adapter_state",
        LuaValidationProblem::ProtectedSchemaChanged => "protected_schema_changed",
        LuaValidationProblem::TemporarySchemaObject => "temporary_schema_object",
        LuaValidationProblem::ForeignKey => "foreign_key",
        LuaValidationProblem::QuickCheck => "quick_check",
    }
}

const fn transaction_state_name(state: LuaTransactionState) -> &'static str {
    match state {
        LuaTransactionState::NotStarted => "not_started",
        LuaTransactionState::RolledBack => "rolled_back",
        LuaTransactionState::RollbackOutcomeUnknown => "rollback_outcome_unknown",
        LuaTransactionState::CommitOutcomeUnknown => "commit_outcome_unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_call_facts_keep_generic_engine_operation_field_and_locator() {
        let issue = LuaIssue::new(LuaProblem::HostCall {
            engine: LuaEngine::Generic,
            operation: LuaOperation::SetTranslation,
            violation: LuaValueViolation::InvalidTranslation,
            field: Some(SafeIdentifier::from_validated("translation")),
            locator: Some(LuaLocator::Generic {
                relative_path: Some(SafePath::new("data/dialogue.jsonl")),
                group_id: Some(SafeIdentifier::from_validated("group-7")),
                unit_id: Some(SafeIdentifier::from_validated("unit-3")),
            }),
            placeholder: None,
        });

        assert_eq!(
            issue.facts(),
            vec![
                ("operation", "set_translation".to_owned()),
                ("engine", "generic".to_owned()),
                ("violation", "invalid_translation".to_owned()),
                ("field", "translation".to_owned()),
                ("relative_path", "data/dialogue.jsonl".to_owned()),
                ("group_id", "group-7".to_owned()),
                ("unit_id", "unit-3".to_owned()),
            ]
        );
    }

    #[test]
    fn prerequisite_and_validation_facts_keep_specific_problem() {
        let prerequisite = LuaIssue::new(LuaProblem::DatabasePrerequisite {
            engine: Some(LuaEngine::Mz),
            violation: LuaValueViolation::StateMismatch,
        });
        assert_eq!(
            prerequisite.facts(),
            vec![
                ("operation", "validate_prerequisites".to_owned()),
                ("engine", "mz".to_owned()),
                ("violation", "state_mismatch".to_owned()),
            ]
        );

        let validation = LuaIssue::new(LuaProblem::Validation {
            engine: LuaEngine::Generic,
            problem: LuaValidationProblem::AdapterState,
        });
        assert_eq!(
            validation.facts(),
            vec![
                ("operation", "validate_database".to_owned()),
                ("engine", "generic".to_owned()),
                ("validation_problem", "adapter_state".to_owned()),
            ]
        );
    }

    #[test]
    fn context_compilation_and_script_facts_keep_specific_backend_problem() {
        let context = LuaIssue::new(LuaProblem::ContextCreation {
            problem: LuaContextProblem::ConcurrentSqliteWait,
        });
        assert_eq!(
            context.facts(),
            vec![
                ("operation", "create_context".to_owned()),
                ("context_problem", "concurrent_sqlite_wait".to_owned()),
            ]
        );

        let compilation = LuaIssue::new(LuaProblem::Compilation {
            script: SafePath::new("review.lua"),
            problem: LuaCompilationProblem::Backend {
                category: LuaCompilerCategory::Syntax,
            },
            line: Some(7),
        });
        assert_eq!(
            compilation.facts(),
            vec![
                ("operation", "compile_script".to_owned()),
                ("script", "review.lua".to_owned()),
                ("compilation_problem", "backend".to_owned()),
                ("compiler_category", "syntax".to_owned()),
                ("line", "7".to_owned()),
            ]
        );

        let script = LuaIssue::new(LuaProblem::ScriptExecution {
            script: SafePath::new("review.lua"),
            problem: LuaScriptProblem::Yielded,
        });
        assert_eq!(
            script.facts(),
            vec![
                ("operation", "execute_script".to_owned()),
                ("script", "review.lua".to_owned()),
                ("script_problem", "yielded".to_owned()),
            ]
        );
    }
}
