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
    SetTranslation,
    ClearTranslation,
    QueryDatabase,
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
            Self::SetTranslation => "set_translation",
            Self::ClearTranslation => "clear_translation",
            Self::QueryDatabase => "query_database",
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
            Self::InvalidTranslation => "invalid_translation",
            Self::UnknownUnit => "unknown_unit",
            Self::StateMismatch => "state_mismatch",
            Self::TransactionLost => "transaction_lost",
        }
    }
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        placeholder: Option<PlaceholderIssue>,
    },
    WorkerPanicked,
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
            LuaProblem::WorkerPanicked => "lua.worker_panicked",
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
            | LuaProblem::HostCall { .. } => DiagnosticResolution::FixInput,
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
                placeholder,
            } => {
                facts.push(("engine", engine.as_str().to_owned()));
                facts.push(("violation", violation.as_str().to_owned()));
                if let Some(field) = field {
                    facts.push(("field", field.to_string()));
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
            LuaProblem::ContextCreation { problem } => {
                facts.push(("context_problem", context_problem_name(*problem).to_owned()));
            }
            _ => {}
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_call_facts_keep_generic_engine_operation_and_field() {
        let issue = LuaIssue::new(LuaProblem::HostCall {
            engine: LuaEngine::Generic,
            operation: LuaOperation::SetTranslation,
            violation: LuaValueViolation::InvalidTranslation,
            field: Some(SafeIdentifier::from_validated("translation")),
            placeholder: None,
        });

        assert_eq!(
            issue.facts(),
            vec![
                ("operation", "set_translation".to_owned()),
                ("engine", "generic".to_owned()),
                ("violation", "invalid_translation".to_owned()),
                ("field", "translation".to_owned()),
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
