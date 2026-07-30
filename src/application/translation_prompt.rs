//! 各翻译引擎共同使用的 Prompt 文件读取与模板校验。

use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::diagnostic::{
    DiagnosticAction, DiagnosticCode, DiagnosticFailureKind, DiagnosticImpact, DiagnosticReason,
    DiagnosticStage, DiagnosticSubject, RecoveryFact, SafeDiagnostic, SafeDiagnosticSource,
};
use crate::language::LanguagePair;
use crate::runtime::filesystem::{SystemFileSystem, SystemFileSystemError};
use crate::storage::file_system::{FileReader, ReadFileError};

pub(crate) const SYSTEM_PROMPT_FILE_NAME: &str = "system.md";
pub(crate) const THINKING_PROMPT_FILE_NAME: &str = "thinking.md";
const SOURCE_LANGUAGE_TEMPLATE_VARIABLE: &str = "{{source_language}}";
const TARGET_LANGUAGE_TEMPLATE_VARIABLE: &str = "{{target_language}}";

/// 从生产文件系统读取一份非空 UTF-8 Prompt。
pub(crate) async fn read_prompt_resource(
    file_system: &SystemFileSystem,
    path: &Path,
) -> Result<String, PromptResourceLoadError> {
    let file = file_system
        .read_file(path.to_owned())
        .await
        .map_err(PromptResourceLoadError::Read)?;
    if file.resolved_path().file_name() != path.file_name() {
        return Err(PromptResourceLoadError::ResolvedFileNameMismatch {
            requested_path: path.to_owned(),
            resolved_path: file.resolved_path().to_owned(),
        });
    }
    let text = String::from_utf8(file.into_bytes()).map_err(|source| {
        let utf8 = source.utf8_error();
        PromptResourceLoadError::InvalidUtf8 {
            path: path.to_owned(),
            valid_up_to: utf8.valid_up_to(),
            error_len: utf8.error_len(),
        }
    })?;
    let text = text.trim();
    if text.is_empty() {
        return Err(PromptResourceLoadError::Empty {
            path: path.to_owned(),
        });
    }
    Ok(text.to_owned())
}

#[derive(Debug)]
pub(crate) enum PromptResourceLoadError {
    Read(ReadFileError<SystemFileSystemError>),
    ResolvedFileNameMismatch {
        requested_path: PathBuf,
        resolved_path: PathBuf,
    },
    InvalidUtf8 {
        path: PathBuf,
        valid_up_to: usize,
        error_len: Option<usize>,
    },
    Empty {
        path: PathBuf,
    },
}

impl PromptResourceLoadError {
    pub(crate) fn safe_diagnostic(&self) -> SafeDiagnostic {
        match self {
            Self::Read(ReadFileError::NotFound { path }) => SafeDiagnostic::new(
                DiagnosticCode::PromptUnavailable,
                DiagnosticStage::CommandPreparation,
                DiagnosticSubject::path(path),
                DiagnosticReason::failure(DiagnosticFailureKind::NotFound),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixConfiguration,
            ),
            Self::Read(ReadFileError::NotFile { path }) => SafeDiagnostic::new(
                DiagnosticCode::PromptUnavailable,
                DiagnosticStage::CommandPreparation,
                DiagnosticSubject::path(path),
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::InvalidValue,
                    "expected=file; actual=not_file",
                ),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixConfiguration,
            ),
            Self::Read(ReadFileError::Io { path, source }) => source
                .safe_diagnostic_source(
                    DiagnosticStage::CommandPreparation,
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::CheckPathAndPermissions,
                )
                .with_recovery(RecoveryFact::path(path)),
            Self::ResolvedFileNameMismatch {
                requested_path,
                resolved_path,
            } => SafeDiagnostic::new(
                DiagnosticCode::PromptUnavailable,
                DiagnosticStage::CommandPreparation,
                DiagnosticSubject::path(requested_path),
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::FileIdentityChanged,
                    format!(
                        "expected_file_name={}; actual_file_name={}",
                        requested_path
                            .file_name()
                            .map_or_else(|| "none".into(), |name| name.to_string_lossy()),
                        resolved_path
                            .file_name()
                            .map_or_else(|| "none".into(), |name| name.to_string_lossy())
                    ),
                ),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckPathAndPermissions,
            )
            .with_recovery(RecoveryFact::path(resolved_path)),
            Self::InvalidUtf8 {
                path,
                valid_up_to,
                error_len,
            } => SafeDiagnostic::new(
                DiagnosticCode::PromptUnavailable,
                DiagnosticStage::CommandPreparation,
                DiagnosticSubject::path(path),
                DiagnosticReason::InvalidUtf8 {
                    valid_up_to: u64::try_from(*valid_up_to).unwrap_or(u64::MAX),
                    error_len: error_len.map(|length| u64::try_from(length).unwrap_or(u64::MAX)),
                },
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixConfiguration,
            ),
            Self::Empty { path } => SafeDiagnostic::new(
                DiagnosticCode::PromptUnavailable,
                DiagnosticStage::CommandPreparation,
                DiagnosticSubject::path(path),
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::MissingRequiredValue,
                    "resource=prompt; content=blank",
                ),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixConfiguration,
            ),
        }
    }
}

impl fmt::Display for PromptResourceLoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(source) => write!(formatter, "无法读取 Prompt 资源：{source}"),
            Self::ResolvedFileNameMismatch {
                requested_path,
                resolved_path,
            } => write!(
                formatter,
                "Prompt 资源文件身份不匹配：请求 {}，固定后为 {}",
                requested_path.display(),
                resolved_path.display()
            ),
            Self::InvalidUtf8 {
                path,
                valid_up_to,
                error_len,
            } => write!(
                formatter,
                "Prompt 资源不是 UTF-8：{}（valid_up_to={valid_up_to}, error_len={error_len:?}）",
                path.display()
            ),
            Self::Empty { path } => write!(formatter, "Prompt 资源正文为空：{}", path.display()),
        }
    }
}

impl Error for PromptResourceLoadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read(source) => Some(source),
            Self::ResolvedFileNameMismatch { .. }
            | Self::InvalidUtf8 { .. }
            | Self::Empty { .. } => None,
        }
    }
}

/// 渲染仅允许源、目标语言变量的 system Prompt。
pub(crate) fn render_system_prompt_template(
    template: &str,
    language_pair: &LanguagePair,
) -> Result<String, PromptTemplateError> {
    let mut rendered = String::with_capacity(template.len());
    let mut remaining = template;
    let mut source_seen = false;
    let mut target_seen = false;

    loop {
        let next_open = remaining.find("{{");
        let next_close = remaining.find("}}");
        let Some(open) = next_open else {
            if next_close.is_some() {
                return Err(PromptTemplateError::InvalidSyntax);
            }
            rendered.push_str(remaining);
            break;
        };
        if next_close.is_some_and(|close| close < open) {
            return Err(PromptTemplateError::InvalidSyntax);
        }

        rendered.push_str(&remaining[..open]);
        let after_open = &remaining[open + 2..];
        let close = after_open
            .find("}}")
            .ok_or(PromptTemplateError::InvalidSyntax)?;
        if after_open[..close].contains("{{") {
            return Err(PromptTemplateError::InvalidSyntax);
        }
        let variable = &remaining[open..open + 2 + close + 2];
        match variable {
            SOURCE_LANGUAGE_TEMPLATE_VARIABLE => {
                rendered.push_str(language_pair.source().as_str());
                source_seen = true;
            }
            TARGET_LANGUAGE_TEMPLATE_VARIABLE => {
                rendered.push_str(language_pair.target().as_str());
                target_seen = true;
            }
            _ => return Err(PromptTemplateError::UnknownVariable),
        }
        remaining = &after_open[close + 2..];
    }

    if !source_seen {
        return Err(PromptTemplateError::MissingSourceLanguage);
    }
    if !target_seen {
        return Err(PromptTemplateError::MissingTargetLanguage);
    }
    if rendered.contains("{{") || rendered.contains("}}") {
        return Err(PromptTemplateError::InvalidSyntax);
    }
    Ok(rendered)
}

/// Thinking Prompt 是固定正文，不接受任何模板变量。
pub(crate) fn ensure_no_prompt_template_variables(text: &str) -> Result<(), PromptTemplateError> {
    if text.contains("{{") || text.contains("}}") {
        return Err(PromptTemplateError::VariablesNotAllowed);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PromptTemplateError {
    InvalidSyntax,
    UnknownVariable,
    MissingSourceLanguage,
    MissingTargetLanguage,
    VariablesNotAllowed,
}

impl fmt::Display for PromptTemplateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSyntax => formatter.write_str("Prompt 模板变量语法无效"),
            Self::UnknownVariable => formatter.write_str("Prompt 模板包含不受支持的变量"),
            Self::MissingSourceLanguage => {
                formatter.write_str("Prompt 模板缺少 source_language 变量")
            }
            Self::MissingTargetLanguage => {
                formatter.write_str("Prompt 模板缺少 target_language 变量")
            }
            Self::VariablesNotAllowed => formatter.write_str("该 Prompt 组件不允许包含模板变量"),
        }
    }
}

impl Error for PromptTemplateError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::{LanguageId, LanguagePair};

    fn language_pair() -> LanguagePair {
        LanguagePair::new(
            LanguageId::parse("ja").expect("语言应合法"),
            LanguageId::parse("zh-Hans").expect("语言应合法"),
        )
    }

    #[test]
    fn system_template_replaces_every_supported_variable() {
        let rendered = render_system_prompt_template(
            "{{source_language}} -> {{target_language}} / {{source_language}}",
            &language_pair(),
        )
        .expect("模板应渲染");
        assert_eq!(rendered, "ja -> zh-Hans / ja");
    }

    #[test]
    fn system_and_thinking_template_boundaries_are_strict() {
        for template in [
            "{{target_language}}",
            "{{source_language}}",
            "{{source_language}} {{target_language}} {{other}}",
            "{{source_language}} {{target_language}",
        ] {
            assert!(
                render_system_prompt_template(template, &language_pair()).is_err(),
                "必须拒绝模板：{template}"
            );
        }
        assert!(ensure_no_prompt_template_variables("固定要求").is_ok());
        assert!(ensure_no_prompt_template_variables("{{source_language}}").is_err());
    }
}
