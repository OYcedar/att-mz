//! RPG Maker 翻译 Profile、Prompt 和实际执行能力的生产构造。

use super::business_log::ProductionBusinessLog;
use super::error::ProductionCommandError;
use crate::application::config::TranslateConfiguration;
use crate::application::translation_prompt::{
    PromptResourceLoadError, PromptTemplateError,
    assemble_translation_system_prompt_with_cancellation,
    ensure_no_prompt_template_variables_with_cancellation, parse_prompt_resource_with_cancellation,
    read_unparsed_prompt_resource, render_system_prompt_template_with_cancellation,
    translation_prompt_resource_paths,
};
use crate::diagnostic::{
    BoxedError, Diagnostic, DiagnosticReport, PromptProblem, RuntimeComponent, RuntimeIssue,
    RuntimeOperation, SafeIdentifier, SafePath, StateEffect, TranslationIssue,
};
use crate::execution::CooperativeCancellation;
use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::execution::llm_request::TokioAsyncDelay;
use crate::language::LanguageModuleCatalogError;
use crate::rpg_maker::project::OpenedProject;
use crate::rpg_maker::translate::asset_reader::RpgMakerTranslationAssetReadingService;
use crate::rpg_maker::translate::executor::{
    RpgMakerTranslationTaskExecutionService, TranslationTaskResponseProcessingService,
};
use crate::rpg_maker::translate::pipeline::RpgMakerTranslationService;
use crate::rpg_maker::translate::placeholder::{
    Pcre2PlaceholderConstructionError, Pcre2PlaceholderService,
};
use crate::rpg_maker::translate::planner::RpgMakerTranslationTaskPlanningService;
use crate::rpg_maker::translate::profile::{
    ResolvedRpgMakerTranslationResources, RpgMakerSystemPrompt, RpgMakerSystemPromptError,
    RpgMakerTranslationPlanningConfiguration, RpgMakerTranslationProfile,
};
use crate::rpg_maker::translate::result_store::RpgMakerTranslationResultStorageService;
use crate::rpg_maker::translate::service::{
    SelectedTranslationExecution, SelectedTranslationExecutionBuilder,
};
use crate::rpg_maker::translate::task_record::ConfiguredTranslationTaskRecordSink;
use crate::runtime::cpu::{CpuExecutorUnavailable, RayonCpuExecutor};
use crate::runtime::filesystem::{SystemFileSystem, SystemFileSystemError};
use crate::runtime::llm::{OpenAiCompatibleClient, OpenAiCompatibleExecutor};
use crate::runtime::sqlite::RusqliteStorage;
use crate::storage::file_system::{FileReader, ReadFileError};
use crate::translation::planning_resource::TranslationPlanningResourceReadingService;
use crate::translation_protocol::TranslationResponseMode;
use std::error::Error;
use std::fmt;
#[cfg(test)]
use std::io;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::sync::Arc;

pub(super) type ProductionTranslationProfile =
    Arc<RpgMakerTranslationProfile<OpenAiCompatibleClient>>;

pub(super) type ProductionTranslationAssetReader =
    RpgMakerTranslationAssetReadingService<RusqliteStorage, RayonCpuExecutor>;
pub(super) type ProductionTranslationPlanner = RpgMakerTranslationTaskPlanningService<
    TranslationPlanningResourceReadingService<SystemFileSystem, RayonCpuExecutor>,
    RayonCpuExecutor,
    OpenAiCompatibleClient,
>;
pub(super) type ProductionTranslationExecutor = RpgMakerTranslationTaskExecutionService<
    OpenAiCompatibleExecutor,
    TokioAsyncDelay,
    TranslationTaskResponseProcessingService<RayonCpuExecutor>,
    ProductionTranslationProfile,
>;
pub(super) type ProductionTranslationStore =
    RpgMakerTranslationResultStorageService<RusqliteStorage, RayonCpuExecutor>;
pub(super) type ProductionRpgMakerTranslation = RpgMakerTranslationService<
    ProductionTranslationAssetReader,
    ProductionTranslationPlanner,
    ProductionTranslationExecutor,
    ProductionTranslationStore,
    ProductionBusinessLog,
    ConfiguredTranslationTaskRecordSink,
>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PromptResourceComponent {
    System,
    Thinking,
    Rules,
    Example,
}

#[derive(Debug)]
enum RpgMakerPromptPreparationError {
    Cancelled,
    SystemResource(PromptResourceLoadError),
    ThinkingResource(PromptResourceLoadError),
    RulesResource(PromptResourceLoadError),
    ExampleResource(PromptResourceLoadError),
    SystemTemplate(PromptTemplateError),
    ThinkingTemplate(PromptTemplateError),
    RulesTemplate(PromptTemplateError),
    ExampleTemplate(PromptTemplateError),
    SystemPrompt(RpgMakerSystemPromptError),
}

fn ensure_rpg_maker_prompt_preparation_running(
    cancellation: &CooperativeCancellation,
) -> Result<(), RpgMakerPromptPreparationError> {
    if cancellation.is_requested() {
        Err(RpgMakerPromptPreparationError::Cancelled)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod prompt_template_tests {
    use super::*;

    #[test]
    fn post_await_cancellation_does_not_overwrite_an_already_formed_build_error() {
        let cancellation = CooperativeCancellation::default();
        cancellation.request();
        let failure = ProductionTranslationExecutionBuildError::prompt_template(
            PromptResourceComponent::System,
            Path::new("C:/att/prompts/translation/system.md"),
            PromptTemplateError::VariablesNotAllowed,
        );

        let error = complete_translation_execution_build_step::<()>(Err(failure), &cancellation)
            .expect_err("已经形成的 Prompt 错误必须先于后到取消返回");

        assert!(!error.is_cancelled());
        let cancelled = complete_translation_execution_build_step(Ok(()), &cancellation)
            .expect_err("成功步骤之后观察到取消时应返回类型化取消");
        assert!(cancelled.is_cancelled());
    }

    #[test]
    fn production_build_error_classifies_only_typed_cancellation_leaves() {
        let path = PathBuf::from("C:/att/prompts/translation/system.md");
        let cancelled = ProductionTranslationExecutionBuildError::prompt_resource(
            PromptResourceComponent::System,
            &path,
            PromptResourceLoadError::Read(ReadFileError::Io {
                path: path.clone(),
                source: SystemFileSystemError::Cancelled {
                    operation: "read_file",
                    path: path.clone(),
                },
            }),
        );
        assert!(cancelled.is_cancelled());

        let ordinary_io = ProductionTranslationExecutionBuildError::prompt_resource(
            PromptResourceComponent::System,
            &path,
            PromptResourceLoadError::Read(ReadFileError::Io {
                path: path.clone(),
                source: SystemFileSystemError::Io {
                    operation: "read_file",
                    path: path.clone(),
                    source: io::Error::other("disk failure"),
                },
            }),
        );
        assert!(!ordinary_io.is_cancelled());
        assert!(
            ProductionTranslationExecutionBuildError::prompt_cpu(CpuTaskExecutionError::Cancelled)
                .is_cancelled()
        );
    }
}

pub(super) struct ProductionSelectedTranslationExecutionBuilder<'a> {
    pub(super) configuration: &'a TranslateConfiguration,
    pub(super) file_system: SystemFileSystem,
    pub(super) cpu: RayonCpuExecutor,
    pub(super) sqlite: RusqliteStorage,
    pub(super) llm: OpenAiCompatibleExecutor,
    pub(super) log: ProductionBusinessLog,
    pub(super) task_records: ConfiguredTranslationTaskRecordSink,
    pub(super) record_translation_tasks: bool,
    pub(super) cancellation: CooperativeCancellation,
}

pub(super) async fn build_production_translation_profile(
    configuration: &TranslateConfiguration,
    file_system: &SystemFileSystem,
    cpu: &RayonCpuExecutor,
    project: &OpenedProject,
    cancellation: &CooperativeCancellation,
) -> Result<
    (
        ProductionTranslationProfile,
        Arc<ResolvedRpgMakerTranslationResources>,
    ),
    ProductionTranslationExecutionBuildError,
> {
    ensure_translation_execution_build_running(cancellation)?;
    let profile_configuration = configuration.profile();
    let language_pair = project.language_pair().clone();
    let response_mode =
        TranslationResponseMode::new(configuration.thinking_output(), configuration.source_echo());
    let prompt_paths =
        translation_prompt_resource_paths(configuration.prompt_root(), response_mode);
    let system_path = prompt_paths.system().to_path_buf();
    let thinking_path = prompt_paths.thinking().map(Path::to_path_buf);
    let rules_path = prompt_paths.rules().to_path_buf();
    let example_path = prompt_paths.example().to_path_buf();
    ensure_translation_execution_build_running(cancellation)?;
    let system_template = read_unparsed_prompt_resource(file_system, &system_path).await;
    let system_template = complete_translation_execution_build_step(
        system_template.map_err(|source| {
            ProductionTranslationExecutionBuildError::prompt_resource(
                PromptResourceComponent::System,
                &system_path,
                source,
            )
        }),
        cancellation,
    )?;
    let thinking = if let Some(path) = thinking_path.as_deref() {
        ensure_translation_execution_build_running(cancellation)?;
        let thinking = read_unparsed_prompt_resource(file_system, path).await;
        Some(complete_translation_execution_build_step(
            thinking.map_err(|source| {
                ProductionTranslationExecutionBuildError::prompt_resource(
                    PromptResourceComponent::Thinking,
                    path,
                    source,
                )
            }),
            cancellation,
        )?)
    } else {
        None
    };
    ensure_translation_execution_build_running(cancellation)?;
    let rules = complete_translation_execution_build_step(
        read_unparsed_prompt_resource(file_system, &rules_path)
            .await
            .map_err(|source| {
                ProductionTranslationExecutionBuildError::prompt_resource(
                    PromptResourceComponent::Rules,
                    &rules_path,
                    source,
                )
            }),
        cancellation,
    )?;
    let example = complete_translation_execution_build_step(
        read_unparsed_prompt_resource(file_system, &example_path)
            .await
            .map_err(|source| {
                ProductionTranslationExecutionBuildError::prompt_resource(
                    PromptResourceComponent::Example,
                    &example_path,
                    source,
                )
            }),
        cancellation,
    )?;

    let prompt_language_pair = language_pair.clone();
    let prompt_cancellation = cancellation.clone();
    ensure_translation_execution_build_running(cancellation)?;
    let system_prompt = cpu
        .execute(move || {
            ensure_rpg_maker_prompt_preparation_running(&prompt_cancellation)?;
            let system_template = parse_prompt_resource_with_cancellation(system_template, || {
                ensure_rpg_maker_prompt_preparation_running(&prompt_cancellation)
            })?
            .map_err(RpgMakerPromptPreparationError::SystemResource)?;
            let rendered_system = render_system_prompt_template_with_cancellation(
                &system_template,
                &prompt_language_pair,
                || ensure_rpg_maker_prompt_preparation_running(&prompt_cancellation),
            )?
            .map_err(RpgMakerPromptPreparationError::SystemTemplate)?;
            let thinking = match thinking {
                Some(thinking) => {
                    let thinking = parse_prompt_resource_with_cancellation(thinking, || {
                        ensure_rpg_maker_prompt_preparation_running(&prompt_cancellation)
                    })?
                    .map_err(RpgMakerPromptPreparationError::ThinkingResource)?;
                    ensure_no_prompt_template_variables_with_cancellation(&thinking, || {
                        ensure_rpg_maker_prompt_preparation_running(&prompt_cancellation)
                    })?
                    .map_err(RpgMakerPromptPreparationError::ThinkingTemplate)?;
                    Some(thinking)
                }
                None => None,
            };
            let rules = parse_prompt_resource_with_cancellation(rules, || {
                ensure_rpg_maker_prompt_preparation_running(&prompt_cancellation)
            })?
            .map_err(RpgMakerPromptPreparationError::RulesResource)?;
            ensure_no_prompt_template_variables_with_cancellation(&rules, || {
                ensure_rpg_maker_prompt_preparation_running(&prompt_cancellation)
            })?
            .map_err(RpgMakerPromptPreparationError::RulesTemplate)?;
            let example = parse_prompt_resource_with_cancellation(example, || {
                ensure_rpg_maker_prompt_preparation_running(&prompt_cancellation)
            })?
            .map_err(RpgMakerPromptPreparationError::ExampleResource)?;
            ensure_no_prompt_template_variables_with_cancellation(&example, || {
                ensure_rpg_maker_prompt_preparation_running(&prompt_cancellation)
            })?
            .map_err(RpgMakerPromptPreparationError::ExampleTemplate)?;
            let prompt_markdown = assemble_translation_system_prompt_with_cancellation(
                rendered_system,
                thinking,
                rules,
                example,
                || ensure_rpg_maker_prompt_preparation_running(&prompt_cancellation),
            )?;
            RpgMakerSystemPrompt::new_with_cancellation(
                prompt_language_pair,
                prompt_markdown,
                response_mode,
                || ensure_rpg_maker_prompt_preparation_running(&prompt_cancellation),
            )?
            .map_err(RpgMakerPromptPreparationError::SystemPrompt)
        })
        .await;
    let system_prompt = system_prompt
        .map_err(ProductionTranslationExecutionBuildError::prompt_cpu)?
        .map_err(|source| match source {
            RpgMakerPromptPreparationError::Cancelled => {
                ProductionTranslationExecutionBuildError::cancelled()
            }
            RpgMakerPromptPreparationError::SystemResource(source) => {
                ProductionTranslationExecutionBuildError::prompt_resource(
                    PromptResourceComponent::System,
                    &system_path,
                    source,
                )
            }
            RpgMakerPromptPreparationError::ThinkingResource(source) => {
                let path = thinking_path
                    .as_deref()
                    .expect("thinking Prompt 错误只会在启用 thinking 输出时产生");
                ProductionTranslationExecutionBuildError::prompt_resource(
                    PromptResourceComponent::Thinking,
                    path,
                    source,
                )
            }
            RpgMakerPromptPreparationError::RulesResource(source) => {
                ProductionTranslationExecutionBuildError::prompt_resource(
                    PromptResourceComponent::Rules,
                    &rules_path,
                    source,
                )
            }
            RpgMakerPromptPreparationError::ExampleResource(source) => {
                ProductionTranslationExecutionBuildError::prompt_resource(
                    PromptResourceComponent::Example,
                    &example_path,
                    source,
                )
            }
            RpgMakerPromptPreparationError::SystemTemplate(source) => {
                ProductionTranslationExecutionBuildError::prompt_template(
                    PromptResourceComponent::System,
                    &system_path,
                    source,
                )
            }
            RpgMakerPromptPreparationError::ThinkingTemplate(source) => {
                let path = thinking_path
                    .as_deref()
                    .expect("thinking Prompt 错误只会在启用 thinking 输出时产生");
                ProductionTranslationExecutionBuildError::prompt_template(
                    PromptResourceComponent::Thinking,
                    path,
                    source,
                )
            }
            RpgMakerPromptPreparationError::RulesTemplate(source) => {
                ProductionTranslationExecutionBuildError::prompt_template(
                    PromptResourceComponent::Rules,
                    &rules_path,
                    source,
                )
            }
            RpgMakerPromptPreparationError::ExampleTemplate(source) => {
                ProductionTranslationExecutionBuildError::prompt_template(
                    PromptResourceComponent::Example,
                    &example_path,
                    source,
                )
            }
            RpgMakerPromptPreparationError::SystemPrompt(source) => {
                ProductionTranslationExecutionBuildError::system_prompt(
                    PromptResourceComponent::System,
                    &system_path,
                    source,
                )
            }
        });
    let system_prompt = complete_translation_execution_build_step(system_prompt, cancellation)?;
    let source_language = configuration
        .language_modules()
        .resolve(language_pair.source())
        .map_err(|source| {
            ProductionTranslationExecutionBuildError::language_module(&language_pair, source)
        })?;
    let translation_resources = Arc::new(ResolvedRpgMakerTranslationResources::new(
        system_prompt,
        source_language,
    ));
    let planning = RpgMakerTranslationPlanningConfiguration::new(
        profile_configuration.target_task_user_message_characters(),
    );
    let profile = Arc::new(RpgMakerTranslationProfile::new(
        profile_configuration.id(),
        planning,
        profile_configuration.request().clone(),
        Arc::clone(configuration.client()),
    ));
    ensure_translation_execution_build_running(cancellation)?;
    Ok((profile, translation_resources))
}

impl SelectedTranslationExecutionBuilder for ProductionSelectedTranslationExecutionBuilder<'_> {
    type Client = OpenAiCompatibleClient;
    type Translation = ProductionRpgMakerTranslation;
    type Error = ProductionTranslationExecutionBuildError;

    fn is_cancelled_error(error: &Self::Error) -> bool {
        error.is_cancelled()
    }

    async fn build(
        &self,
        project: &crate::rpg_maker::project::OpenedProject,
    ) -> Result<SelectedTranslationExecution<Self::Client, Self::Translation>, Self::Error> {
        let (profile, translation_resources) = build_production_translation_profile(
            self.configuration,
            &self.file_system,
            &self.cpu,
            project,
            &self.cancellation,
        )
        .await?;
        ensure_translation_execution_build_running(&self.cancellation)?;
        let placeholder_cancellation = self.cancellation.clone();
        let placeholders = self
            .cpu
            .execute(move || {
                Pcre2PlaceholderService::new_with_cancellation(|| {
                    if placeholder_cancellation.is_requested() {
                        Err(TranslationExecutionBuildCancelled)
                    } else {
                        Ok(())
                    }
                })
            })
            .await;
        let placeholders = placeholders
            .map_err(ProductionTranslationExecutionBuildError::placeholder_cpu)?
            .map_err(|_cancelled| ProductionTranslationExecutionBuildError::cancelled())?
            .map_err(ProductionTranslationExecutionBuildError::builtin_placeholder_compile);
        let placeholders =
            complete_translation_execution_build_step(placeholders, &self.cancellation)?;
        let asset_reader =
            RpgMakerTranslationAssetReadingService::new(self.sqlite.clone(), self.cpu.clone());
        let resources = TranslationPlanningResourceReadingService::new(
            self.file_system.clone(),
            self.cpu.clone(),
        )
        .with_cancellation(self.cancellation.clone());
        let planner = RpgMakerTranslationTaskPlanningService::<_, _, OpenAiCompatibleClient>::new(
            resources,
            Arc::clone(&translation_resources),
            placeholders,
            self.cpu.clone(),
        )
        .with_cancellation(self.cancellation.clone());
        let processor =
            TranslationTaskResponseProcessingService::new(self.cpu.clone(), translation_resources)
                .with_cancellation(self.cancellation.clone());
        let executor =
            RpgMakerTranslationTaskExecutionService::<_, _, _, ProductionTranslationProfile>::new(
                self.llm.clone(),
                TokioAsyncDelay,
                processor,
                self.cancellation.clone(),
            )
            .with_task_recording(self.record_translation_tasks);
        let result_store =
            RpgMakerTranslationResultStorageService::new(self.sqlite.clone(), self.cpu.clone());
        let translation = RpgMakerTranslationService::new(
            asset_reader,
            planner,
            executor,
            result_store,
            self.log.clone(),
            self.cancellation.clone(),
        )
        .with_task_record_sink(self.task_records.clone());
        ensure_translation_execution_build_running(&self.cancellation)?;
        Ok(SelectedTranslationExecution::new(profile, translation))
    }
}

#[derive(Debug)]
struct TranslationExecutionBuildCancelled;

impl fmt::Display for TranslationExecutionBuildCancelled {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("翻译执行上下文构建已取消")
    }
}

impl Error for TranslationExecutionBuildCancelled {}

pub(super) fn ensure_translation_execution_build_running(
    cancellation: &CooperativeCancellation,
) -> Result<(), ProductionTranslationExecutionBuildError> {
    if cancellation.is_requested() {
        Err(ProductionTranslationExecutionBuildError::cancelled())
    } else {
        Ok(())
    }
}

pub(super) fn complete_translation_execution_build_step<T>(
    result: Result<T, ProductionTranslationExecutionBuildError>,
    cancellation: &CooperativeCancellation,
) -> Result<T, ProductionTranslationExecutionBuildError> {
    let value = result?;
    ensure_translation_execution_build_running(cancellation)?;
    Ok(value)
}

pub(super) struct ProductionTranslationExecutionBuildError {
    pub(super) class: TranslationExecutionBuildFailureClass,
    pub(super) cancelled: bool,
    pub(super) diagnostic: Box<DiagnosticReport>,
    pub(super) source: BoxedError,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TranslationExecutionBuildFailureClass {
    ConfigurationOrInput,
    Internal,
}

impl ProductionTranslationExecutionBuildError {
    pub(super) fn cancelled() -> Self {
        let diagnostic = DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::runtime(RuntimeIssue::Cancelled {
                component: RuntimeComponent::CpuExecutor,
                operation: RuntimeOperation::ExecuteTask,
            }),
        );
        let mut error = Self::new(TranslationExecutionBuildCancelled, diagnostic);
        error.cancelled = true;
        error
    }

    pub(super) fn prompt_cpu(source: CpuTaskExecutionError<CpuExecutorUnavailable>) -> Self {
        Self::cpu_task("prepare_rpg_maker_prompt", source)
    }

    pub(super) fn placeholder_cpu(source: CpuTaskExecutionError<CpuExecutorUnavailable>) -> Self {
        Self::cpu_task("compile_rpg_maker_builtin_placeholders", source)
    }

    pub(super) fn cpu_task(
        operation: &'static str,
        source: CpuTaskExecutionError<CpuExecutorUnavailable>,
    ) -> Self {
        let cancelled = matches!(&source, CpuTaskExecutionError::Cancelled);
        let operation = match operation {
            "prepare_rpg_maker_prompt" => RuntimeOperation::PrepareRpgMakerPrompt,
            "compile_rpg_maker_builtin_placeholders" => {
                RuntimeOperation::CompileRpgMakerBuiltinPlaceholders
            }
            _ => RuntimeOperation::ExecuteTask,
        };
        let diagnostic =
            DiagnosticReport::new(StateEffect::Unchanged, source.diagnostic_for(operation));
        let mut error = Self::new(source, diagnostic);
        error.cancelled = cancelled;
        error
    }

    fn prompt_resource(
        component: PromptResourceComponent,
        path: &Path,
        source: PromptResourceLoadError,
    ) -> Self {
        let _ = (component, path);
        let cancelled = matches!(
            &source,
            PromptResourceLoadError::Read(ReadFileError::Io {
                source: SystemFileSystemError::Cancelled { .. },
                ..
            })
        );
        let diagnostic = source.diagnostic_report();
        let mut error = Self::new(source, diagnostic);
        error.cancelled = cancelled;
        error
    }

    fn prompt_template(
        component: PromptResourceComponent,
        path: &Path,
        source: PromptTemplateError,
    ) -> Self {
        let _ = component;
        let diagnostic = DiagnosticReport::new(StateEffect::Unchanged, source.diagnostic(path));
        Self::new(source, diagnostic)
    }

    fn system_prompt(
        component: PromptResourceComponent,
        path: &Path,
        source: RpgMakerSystemPromptError,
    ) -> Self {
        let _ = component;
        let diagnostic = match &source {
            RpgMakerSystemPromptError::Blank => DiagnosticReport::new(
                StateEffect::Unchanged,
                Diagnostic::translation(TranslationIssue::Prompt {
                    path: SafePath::new(path),
                    problem: PromptProblem::Empty,
                }),
            ),
        };
        Self::new(source, diagnostic)
    }

    pub(super) fn language_module(
        language_pair: &crate::language::LanguagePair,
        source: LanguageModuleCatalogError,
    ) -> Self {
        let LanguageModuleCatalogError::UnknownLanguageId {
            language_id,
            available_ids,
        } = &source;
        let available_languages = available_ids
            .iter()
            .map(SafeIdentifier::from_validated)
            .collect();
        let diagnostic = DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::translation(TranslationIssue::LanguageModuleUnavailable {
                requested_language: SafeIdentifier::from_validated(language_id),
                target_language: SafeIdentifier::from_validated(language_pair.target()),
                available_languages,
            }),
        );
        Self::new(source, diagnostic)
    }

    pub(super) fn builtin_placeholder_compile(source: Pcre2PlaceholderConstructionError) -> Self {
        let diagnostic = source.diagnostic_report();
        Self::new(source, diagnostic)
    }

    pub(super) fn new(
        source: impl Error + Send + Sync + 'static,
        diagnostic: DiagnosticReport,
    ) -> Self {
        let class = if diagnostic.primary().resolution()
            == crate::diagnostic::DiagnosticResolution::ReportBug
        {
            TranslationExecutionBuildFailureClass::Internal
        } else {
            TranslationExecutionBuildFailureClass::ConfigurationOrInput
        };
        Self {
            class,
            cancelled: false,
            diagnostic: Box::new(diagnostic),
            source: Box::new(source),
        }
    }

    pub(super) const fn diagnostic(&self) -> &DiagnosticReport {
        &self.diagnostic
    }

    pub(super) const fn is_cancelled(&self) -> bool {
        self.cancelled
    }
}

impl fmt::Debug for ProductionTranslationExecutionBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProductionTranslationExecutionBuildError")
            .field("class", &self.class)
            .field("cancelled", &self.cancelled)
            .field("diagnostic", &self.diagnostic)
            .field("source", &self.source)
            .finish()
    }
}

impl fmt::Display for ProductionTranslationExecutionBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "failed to build the translation execution context ({})",
            self.diagnostic.primary().code()
        )
    }
}

impl Error for ProductionTranslationExecutionBuildError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

pub(super) async fn load_additional_pem_roots(
    file_system: &SystemFileSystem,
    configuration: &crate::application::config::SelectedLlmExecutorConfiguration,
) -> Result<Vec<Vec<u8>>, ProductionCommandError> {
    let mut roots = Vec::with_capacity(configuration.additional_pem_files().len());
    for path in configuration.additional_pem_files() {
        let file = file_system
            .read_file(path.to_path_buf())
            .await
            .map_err(ProductionCommandError::pem_read)?;
        roots.push(file.into_bytes());
    }
    Ok(roots)
}
