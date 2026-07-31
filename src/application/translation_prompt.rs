//! 各翻译引擎共同使用的 Prompt 文件读取与模板校验。

#[cfg(test)]
use std::convert::Infallible;
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

pub(crate) struct UnparsedPromptResource {
    requested_path: PathBuf,
    bytes: Vec<u8>,
}

/// 只读取并固定 Prompt 文件身份；任意长 UTF-8 解码与正文复制由调用方交给 CPU 根。
pub(crate) async fn read_unparsed_prompt_resource(
    file_system: &SystemFileSystem,
    path: &Path,
) -> Result<UnparsedPromptResource, PromptResourceLoadError> {
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
    Ok(UnparsedPromptResource {
        requested_path: path.to_owned(),
        bytes: file.into_bytes(),
    })
}

pub(crate) fn parse_prompt_resource_with_cancellation<E>(
    resource: UnparsedPromptResource,
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Result<String, PromptResourceLoadError>, E> {
    let UnparsedPromptResource {
        requested_path,
        bytes,
    } = resource;
    ensure_running()?;
    match validate_prompt_utf8_with_cancellation(&bytes, &mut ensure_running)? {
        Ok(()) => {}
        Err(source) => {
            return Ok(Err(PromptResourceLoadError::InvalidUtf8 {
                path: requested_path,
                valid_up_to: source.valid_up_to,
                error_len: source.error_len,
            }));
        }
    }

    // SAFETY: 上面的增量校验已经覆盖 `bytes` 的全部内容；成功结果只会在每一段（含跨段
    // 码点）都通过 `str::from_utf8` 后返回。这里取得所有权，避免再做一次不可取消的全量扫描。
    let text = unsafe { String::from_utf8_unchecked(bytes) };
    ensure_running()?;
    let (start, end) = prompt_trim_bounds_with_cancellation(&text, &mut ensure_running)?;
    if start == end {
        return Ok(Err(PromptResourceLoadError::Empty {
            path: requested_path,
        }));
    }
    let mut parsed = String::with_capacity(end - start);
    append_prompt_text_with_cancellation(&mut parsed, &text[start..end], &mut ensure_running)?;
    ensure_running()?;
    Ok(Ok(parsed))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PromptUtf8ValidationError {
    valid_up_to: usize,
    error_len: Option<usize>,
}

fn validate_prompt_utf8_with_cancellation<E>(
    bytes: &[u8],
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<Result<(), PromptUtf8ValidationError>, E> {
    const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;
    const MAX_UTF8_CONTINUATION_BYTES: usize = 3;

    let mut start = 0_usize;
    let mut end = CANCELLATION_CHECK_BYTES.min(bytes.len());
    while start < bytes.len() {
        ensure_running()?;
        match std::str::from_utf8(&bytes[start..end]) {
            Ok(_) => {
                start = end;
                end = start
                    .saturating_add(CANCELLATION_CHECK_BYTES)
                    .min(bytes.len());
            }
            Err(source) => {
                let valid_up_to = start + source.valid_up_to();
                if let Some(error_len) = source.error_len() {
                    return Ok(Err(PromptUtf8ValidationError {
                        valid_up_to,
                        error_len: Some(error_len),
                    }));
                }
                if end == bytes.len() {
                    return Ok(Err(PromptUtf8ValidationError {
                        valid_up_to,
                        error_len: None,
                    }));
                }

                // 当前片段恰好截断了一个码点。从未完成码点重新开始，并把窗口最多扩展
                // 三字节；若扩展又截断下一个码点，下一轮按同一规则继续。
                start = valid_up_to;
                end = end
                    .saturating_add(MAX_UTF8_CONTINUATION_BYTES)
                    .min(bytes.len());
            }
        }
    }
    ensure_running()?;
    Ok(Ok(()))
}

fn prompt_trim_bounds_with_cancellation<E>(
    text: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<(usize, usize), E> {
    const CANCELLATION_CHECK_CHARACTERS: usize = 16 * 1024;

    let mut start = 0_usize;
    for (index, (offset, character)) in text.char_indices().enumerate() {
        if index.is_multiple_of(CANCELLATION_CHECK_CHARACTERS) {
            ensure_running()?;
        }
        if !character.is_whitespace() {
            start = offset;
            break;
        }
        start = offset + character.len_utf8();
    }
    ensure_running()?;
    if start == text.len() {
        return Ok((start, start));
    }

    let mut end = text.len();
    for (index, (offset, character)) in text.char_indices().rev().enumerate() {
        if index.is_multiple_of(CANCELLATION_CHECK_CHARACTERS) {
            ensure_running()?;
        }
        if !character.is_whitespace() {
            end = offset + character.len_utf8();
            break;
        }
        end = offset;
    }
    ensure_running()?;
    Ok((start, end))
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
#[cfg(test)]
pub(crate) fn render_system_prompt_template(
    template: &str,
    language_pair: &LanguagePair,
) -> Result<String, PromptTemplateError> {
    match render_system_prompt_template_with_cancellation(template, language_pair, || {
        Ok::<_, Infallible>(())
    }) {
        Ok(result) => result,
        Err(unreachable) => match unreachable {},
    }
}

pub(crate) fn render_system_prompt_template_with_cancellation<E>(
    template: &str,
    language_pair: &LanguagePair,
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Result<String, PromptTemplateError>, E> {
    ensure_running()?;
    let mut rendered = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut cursor = 0_usize;
    let mut literal_start = 0_usize;
    let mut source_seen = false;
    let mut target_seen = false;

    while cursor < bytes.len() {
        if cursor.is_multiple_of(64 * 1024) {
            ensure_running()?;
        }
        if bytes[cursor..].starts_with(b"}}") {
            return Ok(Err(PromptTemplateError::InvalidSyntax));
        }
        if !bytes[cursor..].starts_with(b"{{") {
            cursor += 1;
            continue;
        }

        append_prompt_text_with_cancellation(
            &mut rendered,
            &template[literal_start..cursor],
            &mut ensure_running,
        )?;
        let variable_start = cursor;
        cursor += 2;
        let variable_end = loop {
            if cursor >= bytes.len() {
                return Ok(Err(PromptTemplateError::InvalidSyntax));
            }
            if cursor.is_multiple_of(64 * 1024) {
                ensure_running()?;
            }
            if bytes[cursor..].starts_with(b"{{") {
                return Ok(Err(PromptTemplateError::InvalidSyntax));
            }
            if bytes[cursor..].starts_with(b"}}") {
                break cursor + 2;
            }
            cursor += 1;
        };
        let variable = &template[variable_start..variable_end];
        match variable {
            SOURCE_LANGUAGE_TEMPLATE_VARIABLE => {
                append_prompt_text_with_cancellation(
                    &mut rendered,
                    language_pair.source().as_str(),
                    &mut ensure_running,
                )?;
                source_seen = true;
            }
            TARGET_LANGUAGE_TEMPLATE_VARIABLE => {
                append_prompt_text_with_cancellation(
                    &mut rendered,
                    language_pair.target().as_str(),
                    &mut ensure_running,
                )?;
                target_seen = true;
            }
            _ => return Ok(Err(PromptTemplateError::UnknownVariable)),
        }
        cursor = variable_end;
        literal_start = cursor;
    }
    append_prompt_text_with_cancellation(
        &mut rendered,
        &template[literal_start..],
        &mut ensure_running,
    )?;

    if !source_seen {
        return Ok(Err(PromptTemplateError::MissingSourceLanguage));
    }
    if !target_seen {
        return Ok(Err(PromptTemplateError::MissingTargetLanguage));
    }
    ensure_running()?;
    Ok(Ok(rendered))
}

/// Thinking Prompt 是固定正文，不接受任何模板变量。
#[cfg(test)]
pub(crate) fn ensure_no_prompt_template_variables(text: &str) -> Result<(), PromptTemplateError> {
    match ensure_no_prompt_template_variables_with_cancellation(text, || Ok::<_, Infallible>(())) {
        Ok(result) => result,
        Err(unreachable) => match unreachable {},
    }
}

pub(crate) fn ensure_no_prompt_template_variables_with_cancellation<E>(
    text: &str,
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Result<(), PromptTemplateError>, E> {
    let bytes = text.as_bytes();
    for index in 0..bytes.len().saturating_sub(1) {
        if index.is_multiple_of(64 * 1024) {
            ensure_running()?;
        }
        if matches!(&bytes[index..index + 2], b"{{" | b"}}") {
            return Ok(Err(PromptTemplateError::VariablesNotAllowed));
        }
    }
    ensure_running()?;
    Ok(Ok(()))
}

fn append_prompt_text_with_cancellation<E>(
    output: &mut String,
    text: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<(), E> {
    const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;

    let mut start = 0_usize;
    while start < text.len() {
        ensure_running()?;
        let mut end = start
            .saturating_add(CANCELLATION_CHECK_BYTES)
            .min(text.len());
        while end < text.len() && !text.is_char_boundary(end) {
            end -= 1;
        }
        output.push_str(&text[start..end]);
        start = end;
    }
    ensure_running()
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

    #[test]
    fn long_prompt_scans_observe_cancellation_between_chunks() {
        let template = format!(
            "{{{{source_language}}}}{}{{{{target_language}}}}",
            "文".repeat(64 * 1024)
        );
        let mut render_polls = 0_usize;
        let rendered =
            render_system_prompt_template_with_cancellation(&template, &language_pair(), || {
                render_polls += 1;
                if render_polls >= 7 {
                    Err("cancelled")
                } else {
                    Ok(())
                }
            });
        assert_eq!(rendered, Err("cancelled"));

        let thinking = "文".repeat(64 * 1024);
        let mut validation_polls = 0_usize;
        let validated = ensure_no_prompt_template_variables_with_cancellation(&thinking, || {
            validation_polls += 1;
            if validation_polls >= 2 {
                Err("cancelled")
            } else {
                Ok(())
            }
        });
        assert_eq!(validated, Err("cancelled"));
    }

    #[test]
    fn unparsed_prompt_preserves_trim_and_utf8_error_contract() {
        let parsed = parse_prompt_resource_with_cancellation(
            UnparsedPromptResource {
                requested_path: PathBuf::from("system.md"),
                bytes: "\u{3000}\n正文\t".as_bytes().to_vec(),
            },
            || Ok::<_, ()>(()),
        )
        .expect("未取消的 Prompt 应完成解析")
        .expect("Prompt 应有效");
        assert_eq!(parsed, "正文");

        let invalid = parse_prompt_resource_with_cancellation(
            UnparsedPromptResource {
                requested_path: PathBuf::from("system.md"),
                bytes: vec![b'a', 0xff],
            },
            || Ok::<_, ()>(()),
        )
        .expect("未取消的无效 Prompt 应返回输入错误")
        .expect_err("无效 UTF-8 必须被拒绝");
        assert!(matches!(
            invalid,
            PromptResourceLoadError::InvalidUtf8 {
                valid_up_to: 1,
                error_len: Some(1),
                ..
            }
        ));
    }

    #[test]
    fn incremental_utf8_validation_preserves_cross_chunk_error_location() {
        const CHUNK_BYTES: usize = 64 * 1024;

        let mut valid = vec![b'a'; CHUNK_BYTES - 1];
        valid.extend_from_slice("译文".as_bytes());
        let parsed = parse_prompt_resource_with_cancellation(
            UnparsedPromptResource {
                requested_path: PathBuf::from("system.md"),
                bytes: valid,
            },
            || Ok::<_, ()>(()),
        )
        .expect("未取消的 Prompt 应完成解析")
        .expect("跨块 UTF-8 码点必须有效");
        assert!(parsed.ends_with("译文"));

        for invalid_suffix in [vec![0xe8, 0xff], vec![0xe8, 0xaf]] {
            let mut invalid = vec![b'a'; CHUNK_BYTES - 1];
            invalid.extend_from_slice(&invalid_suffix);
            let expected = std::str::from_utf8(&invalid).expect_err("夹具必须是无效 UTF-8");
            let expected_valid_up_to = expected.valid_up_to();
            let expected_error_len = expected.error_len();

            let error = parse_prompt_resource_with_cancellation(
                UnparsedPromptResource {
                    requested_path: PathBuf::from("system.md"),
                    bytes: invalid,
                },
                || Ok::<_, ()>(()),
            )
            .expect("未取消的无效 Prompt 应返回输入错误")
            .expect_err("无效 UTF-8 必须被拒绝");
            assert!(matches!(
                error,
                PromptResourceLoadError::InvalidUtf8 {
                    valid_up_to,
                    error_len,
                    ..
                } if valid_up_to == expected_valid_up_to && error_len == expected_error_len
            ));
        }
    }

    #[test]
    fn incremental_utf8_validation_observes_cancellation() {
        let mut polls = 0_usize;
        let parsed = parse_prompt_resource_with_cancellation(
            UnparsedPromptResource {
                requested_path: PathBuf::from("system.md"),
                bytes: vec![b'a'; 4 * 64 * 1024],
            },
            || {
                polls += 1;
                if polls >= 3 { Err("cancelled") } else { Ok(()) }
            },
        );

        assert!(matches!(parsed, Err("cancelled")));
        assert_eq!(polls, 3);
    }

    #[test]
    fn unparsed_prompt_trim_and_copy_observe_cancellation() {
        let leading = format!("{}正文", "\u{3000}".repeat(128 * 1024));
        let mut trim_polls = 0_usize;
        let trimmed = parse_prompt_resource_with_cancellation(
            UnparsedPromptResource {
                requested_path: PathBuf::from("system.md"),
                bytes: leading.into_bytes(),
            },
            || {
                trim_polls += 1;
                if trim_polls >= 5 {
                    Err("cancelled")
                } else {
                    Ok(())
                }
            },
        );
        assert!(matches!(trimmed, Err("cancelled")));

        let body = "文".repeat(128 * 1024);
        let mut copy_polls = 0_usize;
        let copied = parse_prompt_resource_with_cancellation(
            UnparsedPromptResource {
                requested_path: PathBuf::from("system.md"),
                bytes: body.into_bytes(),
            },
            || {
                copy_polls += 1;
                if copy_polls >= 9 {
                    Err("cancelled")
                } else {
                    Ok(())
                }
            },
        );
        assert!(matches!(copied, Err("cancelled")));
    }
}
