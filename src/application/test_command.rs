//! 根 `att test` 命令的只读配置与 LLM Client 验证纵向切片。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::application::config::{ConfiguredTestClient, ConfiguredTestCommand};
use crate::application::termination::{
    TerminationOutcome, TerminationSignals, drive_with_termination,
};
use crate::diagnostic::{
    Diagnostic, DiagnosticReport, IoFailure, RuntimeComponent, RuntimeIssue, RuntimeOperation,
    StateEffect,
};
use crate::llm::{
    ApiKeyRedactor, ChatMessage, ChatMessageRole, LlmRequestError, LlmRequestExecutor,
    LlmRequestFailure,
};
use crate::runtime::filesystem::SystemFileSystem;
use crate::runtime::llm::{OpenAiCompatibleExecutor, OpenAiProtocol};
use crate::runtime::performance::RunPerformanceCounters;
use crate::storage::file_system::FileReader;

const TEST_SYSTEM_MESSAGE: &str = "Reply with OK.";
const TEST_USER_MESSAGE: &str = "OK";

pub(crate) struct TestCommandReport {
    pub(crate) clients: Vec<TestClientResult>,
    pub(crate) total_clients: usize,
    pub(crate) interrupted: bool,
    pub(crate) command_diagnostics: Vec<DiagnosticReport>,
    pub(crate) redactors: Vec<Arc<ApiKeyRedactor>>,
}

impl TestCommandReport {
    pub(crate) fn failed_clients(&self) -> usize {
        self.clients
            .iter()
            .filter(|result| result.diagnostic.is_some())
            .count()
    }

    pub(crate) fn passed_clients(&self) -> usize {
        self.clients.len() - self.failed_clients()
    }

    pub(crate) fn skipped_clients(&self) -> usize {
        self.total_clients - self.clients.len()
    }

    pub(crate) fn succeeded(&self) -> bool {
        !self.interrupted
            && self.failed_clients() == 0
            && self.command_diagnostics.is_empty()
            && self.clients.len() == self.total_clients
    }
}

pub(crate) struct TestClientResult {
    pub(crate) id: String,
    pub(crate) protocol: OpenAiProtocol,
    pub(crate) stream: bool,
    pub(crate) diagnostic: Option<DiagnosticReport>,
}

pub(crate) async fn run_test_command(
    command: ConfiguredTestCommand,
    termination_signals: &mut TerminationSignals,
) -> TestCommandReport {
    let total_clients = command.clients().len();
    let redactors = command
        .clients()
        .iter()
        .map(ConfiguredTestClient::api_key_redactor)
        .collect::<Vec<_>>();
    let file_system =
        match SystemFileSystem::new_with_performance(Arc::new(RunPerformanceCounters::default())) {
            Ok(file_system) => file_system,
            Err(source) => {
                return TestCommandReport {
                    clients: Vec::new(),
                    total_clients,
                    interrupted: false,
                    command_diagnostics: vec![DiagnosticReport::new(
                        StateEffect::Unchanged,
                        source.diagnostic(),
                    )],
                    redactors,
                };
            }
        };

    let mut clients = Vec::with_capacity(total_clients);
    let mut interrupted = false;
    let mut command_diagnostics = Vec::new();
    for client in command.clients() {
        let result = prepare_and_test_client(client, &file_system, termination_signals).await;
        if let Some(diagnostic) = result.signal_diagnostic {
            command_diagnostics.push(diagnostic);
        }
        interrupted |= result.interrupted;
        if result.executed {
            clients.push(TestClientResult {
                id: client.id().to_owned(),
                protocol: client.protocol(),
                stream: client.stream(),
                diagnostic: result.diagnostic,
            });
        }
        if interrupted {
            break;
        }
    }

    if let Err(source) = file_system.shutdown().await {
        command_diagnostics.push(source.shutdown_diagnostic_report());
    }
    TestCommandReport {
        clients,
        total_clients,
        interrupted,
        command_diagnostics,
        redactors,
    }
}

struct DrivenClientResult {
    executed: bool,
    diagnostic: Option<DiagnosticReport>,
    interrupted: bool,
    signal_diagnostic: Option<DiagnosticReport>,
}

async fn prepare_and_test_client(
    client: &ConfiguredTestClient,
    file_system: &SystemFileSystem,
    termination_signals: &mut TerminationSignals,
) -> DrivenClientResult {
    let mut pem_roots = Vec::with_capacity(client.executor().additional_pem_files().len());
    for path in client.executor().additional_pem_files() {
        match file_system.read_file(path.to_path_buf()).await {
            Ok(file) => pem_roots.push(file.into_bytes()),
            Err(source) => {
                return DrivenClientResult {
                    executed: true,
                    diagnostic: Some(source.command_preparation_diagnostic_report()),
                    interrupted: false,
                    signal_diagnostic: None,
                };
            }
        }
    }
    let executor = match OpenAiCompatibleExecutor::new(client.executor().with_pem_roots(pem_roots))
    {
        Ok(executor) => executor,
        Err(source) => {
            return DrivenClientResult {
                executed: true,
                diagnostic: Some(DiagnosticReport::new(
                    StateEffect::Unchanged,
                    source.diagnostic(),
                )),
                interrupted: false,
                signal_diagnostic: None,
            };
        }
    };
    let attempted = Arc::new(AtomicBool::new(false));
    let observed_attempt = Arc::clone(&attempted);
    let messages = [
        ChatMessage::new(ChatMessageRole::System, TEST_SYSTEM_MESSAGE),
        ChatMessage::new(ChatMessageRole::User, TEST_USER_MESSAGE),
    ];
    let executor_for_cancel = executor.clone();
    let file_system_for_cancel = file_system.clone();
    let driven = drive_with_termination(
        executor.request_with_attempt_observer(
            client.client(),
            &messages,
            Box::new(move || observed_attempt.store(true, Ordering::Release)),
        ),
        termination_signals,
        move || {
            executor_for_cancel.cancel_waits();
            file_system_for_cancel.cancel_waits();
        },
        || {},
    )
    .await;

    let (request, interrupted, signal_diagnostic) = match driven {
        TerminationOutcome::Finished(request) => (request, false, None),
        TerminationOutcome::Interrupted(request) => (request, true, None),
        TerminationOutcome::SignalFailed { source, result } => (
            result,
            false,
            Some(DiagnosticReport::new(
                StateEffect::Unchanged,
                Diagnostic::runtime(RuntimeIssue::Io {
                    component: RuntimeComponent::TerminationSignals,
                    operation: RuntimeOperation::ReceiveTerminationSignal,
                    failure: IoFailure::from_error(&source),
                }),
            )),
        ),
    };
    let was_attempted = attempted.load(Ordering::Acquire);
    let diagnostic = match request {
        Ok(_) => None,
        Err(LlmRequestError::Retryable {
            source,
            retry_after,
        }) => {
            executor.continue_after_retryable(source.service_status());
            Some(DiagnosticReport::new(
                StateEffect::Unchanged,
                executor.request_diagnostic(client.client(), &source, retry_after),
            ))
        }
        Err(LlmRequestError::Fatal(source)) => Some(DiagnosticReport::new(
            StateEffect::Unchanged,
            executor.request_diagnostic(client.client(), &source, None),
        )),
        Err(LlmRequestError::AdmissionStopped { diagnostic, .. }) => Some(diagnostic),
    };
    executor.shutdown().await;

    DrivenClientResult {
        executed: !interrupted || was_attempted,
        diagnostic,
        interrupted,
        signal_diagnostic,
    }
}
