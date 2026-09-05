//! Generic 模型 Prompt 与额外证书的配置装配。

use super::diagnostics::{
    GenericCommandError, generic_cpu_execution_failure, generic_prompt_resource_failure,
    generic_read_file_failure,
};
use crate::application::translation_prompt::{
    PromptResourceLoadError, PromptTemplateError,
    assemble_translation_system_prompt_with_cancellation,
    ensure_no_prompt_template_variables_with_cancellation, parse_prompt_resource_with_cancellation,
    read_unparsed_prompt_resource, render_system_prompt_template_with_cancellation,
    translation_prompt_resource_paths,
};
use crate::diagnostic::{DiagnosticReport, FileSystemDiagnosticStage, StateEffect};
use crate::execution::CooperativeCancellation;
use crate::execution::cpu::CpuTaskExecutor;
use crate::runtime::cpu::RayonCpuExecutor;
use crate::runtime::filesystem::SystemFileSystem;
use crate::storage::file_system::FileReader;
use crate::translation_protocol::TranslationResponseMode;
use std::path::Path;

pub(super) struct LoadedGenericPrompt {
    pub(super) system_prompt: String,
    pub(super) response_mode: TranslationResponseMode,
}

#[derive(Debug)]
pub(super) enum GenericPromptPreparationError {
    Cancelled,
    SystemResource(PromptResourceLoadError),
    ThinkingResource(PromptResourceLoadError),
    RulesResource(PromptResourceLoadError),
    ExampleResource(PromptResourceLoadError),
    SystemTemplate(PromptTemplateError),
    ThinkingTemplate(PromptTemplateError),
    RulesTemplate(PromptTemplateError),
    ExampleTemplate(PromptTemplateError),
}

pub(super) async fn load_generic_prompt(
    file_system: &SystemFileSystem,
    cpu: &RayonCpuExecutor,
    configuration: &crate::application::config::TranslateConfiguration,
    language_pair: &crate::language::LanguagePair,
    cancellation: &CooperativeCancellation,
) -> Result<LoadedGenericPrompt, GenericCommandError> {
    let response_mode =
        TranslationResponseMode::new(configuration.thinking_output(), configuration.source_echo());
    let prompt_paths =
        translation_prompt_resource_paths(configuration.prompt_root(), response_mode);
    let template = read_unparsed_prompt_resource(file_system, prompt_paths.system())
        .await
        .map_err(generic_prompt_resource_failure)?;
    let thinking = if let Some(path) = prompt_paths.thinking() {
        Some(
            read_unparsed_prompt_resource(file_system, path)
                .await
                .map_err(generic_prompt_resource_failure)?,
        )
    } else {
        None
    };
    let rules = read_unparsed_prompt_resource(file_system, prompt_paths.rules())
        .await
        .map_err(generic_prompt_resource_failure)?;
    let example = read_unparsed_prompt_resource(file_system, prompt_paths.example())
        .await
        .map_err(generic_prompt_resource_failure)?;
    let system_path = prompt_paths.system().to_path_buf();
    let thinking_path = prompt_paths.thinking().map(Path::to_path_buf);
    let rules_path = prompt_paths.rules().to_path_buf();
    let example_path = prompt_paths.example().to_path_buf();
    let language_pair = language_pair.clone();
    let prompt_cancellation = cancellation.clone();
    cpu.execute(move || {
        ensure_generic_prompt_preparation_running(&prompt_cancellation)?;
        let template = parse_prompt_resource_with_cancellation(template, || {
            ensure_generic_prompt_preparation_running(&prompt_cancellation)
        })?
        .map_err(GenericPromptPreparationError::SystemResource)?;
        let rendered_system =
            render_system_prompt_template_with_cancellation(&template, &language_pair, || {
                ensure_generic_prompt_preparation_running(&prompt_cancellation)
            })?
            .map_err(GenericPromptPreparationError::SystemTemplate)?;
        let thinking = if let Some(thinking) = thinking {
            let thinking = parse_prompt_resource_with_cancellation(thinking, || {
                ensure_generic_prompt_preparation_running(&prompt_cancellation)
            })?
            .map_err(GenericPromptPreparationError::ThinkingResource)?;
            ensure_no_prompt_template_variables_with_cancellation(&thinking, || {
                ensure_generic_prompt_preparation_running(&prompt_cancellation)
            })?
            .map_err(GenericPromptPreparationError::ThinkingTemplate)?;
            Some(thinking)
        } else {
            None
        };
        let rules = parse_prompt_resource_with_cancellation(rules, || {
            ensure_generic_prompt_preparation_running(&prompt_cancellation)
        })?
        .map_err(GenericPromptPreparationError::RulesResource)?;
        ensure_no_prompt_template_variables_with_cancellation(&rules, || {
            ensure_generic_prompt_preparation_running(&prompt_cancellation)
        })?
        .map_err(GenericPromptPreparationError::RulesTemplate)?;

        let example = parse_prompt_resource_with_cancellation(example, || {
            ensure_generic_prompt_preparation_running(&prompt_cancellation)
        })?
        .map_err(GenericPromptPreparationError::ExampleResource)?;
        ensure_no_prompt_template_variables_with_cancellation(&example, || {
            ensure_generic_prompt_preparation_running(&prompt_cancellation)
        })?
        .map_err(GenericPromptPreparationError::ExampleTemplate)?;
        let system_prompt = assemble_translation_system_prompt_with_cancellation(
            rendered_system,
            thinking,
            rules,
            example,
            || ensure_generic_prompt_preparation_running(&prompt_cancellation),
        )?;

        Ok::<_, GenericPromptPreparationError>(LoadedGenericPrompt {
            system_prompt,
            response_mode,
        })
    })
    .await
    .map_err(generic_cpu_execution_failure)?
    .map_err(|source| match source {
        GenericPromptPreparationError::Cancelled => GenericCommandError::Cancelled,
        GenericPromptPreparationError::SystemResource(source) => {
            generic_prompt_resource_failure(source)
        }
        GenericPromptPreparationError::ThinkingResource(source) => {
            generic_prompt_resource_failure(source)
        }
        GenericPromptPreparationError::RulesResource(source) => {
            generic_prompt_resource_failure(source)
        }
        GenericPromptPreparationError::ExampleResource(source) => {
            generic_prompt_resource_failure(source)
        }
        GenericPromptPreparationError::SystemTemplate(source) => {
            generic_prompt_template_failure(&system_path, source)
        }
        GenericPromptPreparationError::ThinkingTemplate(source) => generic_prompt_template_failure(
            thinking_path
                .as_deref()
                .expect("thinking 模板失败必须对应已选择的 thinking 资源"),
            source,
        ),
        GenericPromptPreparationError::RulesTemplate(source) => {
            generic_prompt_template_failure(&rules_path, source)
        }
        GenericPromptPreparationError::ExampleTemplate(source) => {
            generic_prompt_template_failure(&example_path, source)
        }
    })
}

fn generic_prompt_template_failure(
    path: &Path,
    source: PromptTemplateError,
) -> GenericCommandError {
    let report = DiagnosticReport::new(StateEffect::Unchanged, source.diagnostic(path));
    GenericCommandError::reported(source, report)
}

fn ensure_generic_prompt_preparation_running(
    cancellation: &CooperativeCancellation,
) -> Result<(), GenericPromptPreparationError> {
    if cancellation.is_requested() {
        Err(GenericPromptPreparationError::Cancelled)
    } else {
        Ok(())
    }
}

pub(super) async fn load_additional_pem_roots(
    file_system: &SystemFileSystem,
    configuration: &crate::application::config::SelectedLlmExecutorConfiguration,
) -> Result<Vec<Vec<u8>>, GenericCommandError> {
    let mut roots = Vec::with_capacity(configuration.additional_pem_files().len());
    for path in configuration.additional_pem_files() {
        let file = file_system
            .read_file(path.to_path_buf())
            .await
            .map_err(|source| {
                generic_read_file_failure(source, FileSystemDiagnosticStage::CommandPreparation)
            })?;
        roots.push(file.into_bytes());
    }
    Ok(roots)
}
