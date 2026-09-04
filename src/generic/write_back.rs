//! Generic JSONL 写回候选的构造与往返验证。

use std::borrow::Cow;
use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::diagnostic::{
    Diagnostic, DiagnosticReport, GenericDiagnosticStage, GenericIssue, GenericProblem,
    GenericUnitLocator, GenericWriteBackSnapshotProblem, GenericWriteBackTextSide,
    GenericWriteBackUnitProblem, SafeIdentifier, SafePath, StateEffect,
};
use crate::execution::CooperativeCancellation;
use crate::translation::candidate_validation::{
    ProvenInvariantViolation, validate_reflowed_candidate_text_with_cancellation,
};
use crate::translation::layout_rules::{
    LayoutMaterialization, LayoutRuleEngine, LayoutRuleSet, LayoutRuleTarget, LayoutRulesError,
    compile_layout_rules,
};
use crate::translation::placeholder::{PlaceholderProtectionError, PlaceholderRestoreError};
use crate::translation::placeholder_projection::LanguageTextProjectionError;
use crate::translation::text_layout::layout_text;
use crate::translation::write_back_text::{
    PunctuationRepairError, PunctuationRepairOutcome, repair_punctuation_with_cancellation,
};

#[cfg(test)]
use super::jsonl::parse_file;
use super::jsonl::{
    GenericFile, GenericInputSnapshot, GenericJsonlError, parse_file_with_cancellation,
    serialize_groups_with_cancellation,
};
use super::placeholder::{
    GenericCompiledPlaceholderRules, GenericPlaceholderService, GenericSourceBoundPlaceholderError,
};
use super::project::GenericStoredSnapshot;
use super::translate::{GenericUnitKey, GenericUnitMap, generic_language_projection_problem};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GenericCurrentTranslation {
    text: String,
    manual: bool,
}

impl GenericCurrentTranslation {
    pub(crate) fn new(text: String, manual: bool) -> Self {
        Self { text, manual }
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) const fn is_manual(&self) -> bool {
        self.manual
    }
}

/// 候选中的一个 JSONL 文件。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GenericWriteBackFile {
    validated: GenericFile,
}

impl GenericWriteBackFile {
    pub(crate) fn relative_path(&self) -> &Path {
        self.validated.relative_path()
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        self.validated.raw_bytes()
    }
}

/// 已通过生产解析器往返验证、可以交给目录发布能力的完整候选。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GenericWriteBackCandidate {
    files: Vec<GenericWriteBackFile>,
    translated_units: usize,
    retained_source_units: usize,
}

impl GenericWriteBackCandidate {
    pub(crate) fn files(&self) -> &[GenericWriteBackFile] {
        &self.files
    }

    pub(crate) const fn translated_units(&self) -> usize {
        self.translated_units
    }

    pub(crate) const fn retained_source_units(&self) -> usize {
        self.retained_source_units
    }
}

/// 候选无法证明只修改了 Unit text。
#[derive(Debug)]
pub(crate) enum GenericWriteBackError {
    SourceChanged,
    SnapshotMismatch(GenericWriteBackSnapshotProblem),
    PlaceholderProtection {
        unit: GenericUnitLocator,
        side: GenericWriteBackTextSide,
        source: PlaceholderProtectionError,
    },
    PlaceholderBindingMismatch {
        unit: GenericUnitLocator,
    },
    LanguageProjection {
        unit: GenericUnitLocator,
        side: GenericWriteBackTextSide,
        source: LanguageTextProjectionError,
    },
    LayoutRestoration {
        unit: GenericUnitLocator,
        source: PlaceholderRestoreError,
    },
    CandidateViolation {
        unit: GenericUnitLocator,
        problem: ProvenInvariantViolation,
    },
    MaterializedMismatch {
        path: PathBuf,
        bytes_changed: bool,
        structure_changed: bool,
    },
    Jsonl(GenericJsonlError),
}

impl fmt::Display for GenericWriteBackError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SourceChanged => formatter.write_str("Generic 输入已变化，请先运行 Extract"),
            Self::SnapshotMismatch(problem) => {
                write!(formatter, "Generic 数据库快照与当前输入不一致：{problem:?}")
            }
            Self::PlaceholderProtection { source, .. } => source.fmt(formatter),
            Self::PlaceholderBindingMismatch { .. } => {
                formatter.write_str("当前译文没有完整保留原文实际命中的 Placeholder")
            }
            Self::LanguageProjection { source, .. } => source.fmt(formatter),
            Self::LayoutRestoration { source, .. } => source.fmt(formatter),
            Self::CandidateViolation { problem, .. } => {
                write!(formatter, "当前译文违反强不变量：{problem:?}")
            }
            Self::MaterializedMismatch {
                path,
                bytes_changed,
                structure_changed,
            } => write!(
                formatter,
                "暂存 Generic JSONL 与已验证候选不一致：{}（字节变化：{bytes_changed}，结构变化：{structure_changed}）",
                path.display()
            ),
            Self::Jsonl(source) => source.fmt(formatter),
        }
    }
}

impl Error for GenericWriteBackError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Jsonl(source) => Some(source),
            Self::PlaceholderProtection { source, .. } => Some(source),
            Self::LanguageProjection { source, .. } => Some(source),
            Self::LayoutRestoration { source, .. } => Some(source),
            Self::SourceChanged
            | Self::SnapshotMismatch(_)
            | Self::PlaceholderBindingMismatch { .. }
            | Self::CandidateViolation { .. }
            | Self::MaterializedMismatch { .. } => None,
        }
    }
}

impl GenericWriteBackError {
    pub(crate) fn is_cancelled(&self) -> bool {
        matches!(self, Self::Jsonl(source) if source.is_cancelled())
    }

    pub(crate) fn diagnostic_report(&self, effect: StateEffect) -> DiagnosticReport {
        let diagnostic = match self {
            Self::SourceChanged => Diagnostic::generic(GenericIssue::project(
                GenericDiagnosticStage::WriteBack,
                GenericProblem::WriteBackSourceChanged,
            )),
            Self::SnapshotMismatch(problem) => Diagnostic::generic(GenericIssue::project(
                GenericDiagnosticStage::WriteBack,
                GenericProblem::WriteBackSnapshotMismatch {
                    problem: problem.clone(),
                },
            )),
            Self::PlaceholderProtection { unit, side, source } => {
                Diagnostic::generic(GenericIssue::project(
                    GenericDiagnosticStage::WriteBack,
                    GenericProblem::WriteBackUnit {
                        unit: unit.clone(),
                        problem: GenericWriteBackUnitProblem::PlaceholderProtection {
                            side: *side,
                            problem: source.diagnostic_issue(),
                        },
                    },
                ))
            }
            Self::PlaceholderBindingMismatch { unit } => {
                Diagnostic::generic(GenericIssue::project(
                    GenericDiagnosticStage::WriteBack,
                    GenericProblem::WriteBackUnit {
                        unit: unit.clone(),
                        problem: GenericWriteBackUnitProblem::PlaceholderBindingMismatch {
                            side: GenericWriteBackTextSide::Translation,
                        },
                    },
                ))
            }
            Self::LanguageProjection { unit, side, source } => {
                Diagnostic::generic(GenericIssue::project(
                    GenericDiagnosticStage::WriteBack,
                    GenericProblem::WriteBackUnit {
                        unit: unit.clone(),
                        problem: GenericWriteBackUnitProblem::LanguageProjection {
                            side: *side,
                            problem: generic_language_projection_problem(source),
                        },
                    },
                ))
            }
            Self::LayoutRestoration { unit, .. } => Diagnostic::generic(GenericIssue::project(
                GenericDiagnosticStage::WriteBack,
                GenericProblem::WriteBackUnit {
                    unit: unit.clone(),
                    problem: GenericWriteBackUnitProblem::LayoutRestoration,
                },
            )),
            Self::CandidateViolation { unit, problem } => {
                Diagnostic::generic(GenericIssue::project(
                    GenericDiagnosticStage::WriteBack,
                    GenericProblem::WriteBackUnit {
                        unit: unit.clone(),
                        problem: GenericWriteBackUnitProblem::CandidateViolation {
                            problem: problem.clone(),
                        },
                    },
                ))
            }
            Self::MaterializedMismatch {
                path,
                bytes_changed,
                structure_changed,
            } => Diagnostic::generic(GenericIssue::project(
                GenericDiagnosticStage::WriteBack,
                GenericProblem::WriteBackMaterializedMismatch {
                    path: SafePath::new(path),
                    bytes_changed: *bytes_changed,
                    structure_changed: *structure_changed,
                },
            )),
            Self::Jsonl(source) => source.diagnostic(GenericDiagnosticStage::WriteBack),
        };
        DiagnosticReport::new(effect, diagnostic)
    }
}

/// 已按当前 Generic 输入完整匹配且确认不存在重叠的排版宽度。
pub(crate) struct GenericCompiledLayoutRules {
    widths: GenericUnitMap<u32>,
}

impl GenericCompiledLayoutRules {
    fn width_with_cancellation<E>(
        &self,
        group_id: &str,
        unit_id: &str,
        ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Option<u32>, E> {
        self.widths
            .get_parts_with_cancellation(group_id, unit_id, ensure_running)
            .map(|width| width.copied())
    }
}

/// Generic WriteBack 的两个独立正文选择；聚合传递只用于避免相邻布尔参数被写反。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GenericWriteBackTextOptions {
    repair_punctuation: bool,
    complete_continuation_whitespace: bool,
}

impl GenericWriteBackTextOptions {
    pub(crate) const fn new(
        repair_punctuation: bool,
        complete_continuation_whitespace: bool,
    ) -> Self {
        Self {
            repair_punctuation,
            complete_continuation_whitespace,
        }
    }
}

/// 规则总是针对当前完整输入编译；即使某个 Unit 暂无译文，也必须参与选择器校验。
pub(crate) fn compile_generic_layout_rules(
    live: &GenericInputSnapshot,
    rules: &LayoutRuleSet,
) -> Result<GenericCompiledLayoutRules, LayoutRulesError> {
    let mut targets = Vec::with_capacity(live.unit_count());
    let mut keys = Vec::with_capacity(live.unit_count());
    for file in live.files() {
        let source_file = file.relative_path().to_string_lossy().replace('\\', "/");
        for (group_index, group) in file.groups().iter().enumerate() {
            for (unit_index, unit) in group.units().iter().enumerate() {
                targets.push(LayoutRuleTarget::new(
                    group.kind(),
                    super::readable_generic_unit_id(
                        file.relative_path(),
                        group_index + 1,
                        unit_index + 1,
                    ),
                    source_file.clone(),
                    None,
                    None,
                    None,
                    Some(group.id().to_owned()),
                    Some(unit.id().to_owned()),
                    LayoutMaterialization::StringLf,
                ));
                keys.push(GenericUnitKey::new(
                    group.id().to_owned(),
                    unit.id().to_owned(),
                ));
            }
        }
    }
    let compiled = compile_layout_rules(LayoutRuleEngine::Generic, rules, &targets)?;
    let mut widths = GenericUnitMap::with_capacity(compiled.len());
    for (key, width) in keys.into_iter().zip(compiled) {
        let Some(width) = width else {
            continue;
        };
        let previous =
            widths.insert_with_cancellation(key, width, || Ok::<_, LayoutRulesError>(()))?;
        debug_assert!(
            previous.is_none(),
            "Generic Unit 身份已经由输入格式保证唯一"
        );
    }
    Ok(GenericCompiledLayoutRules { widths })
}

impl From<GenericJsonlError> for GenericWriteBackError {
    fn from(source: GenericJsonlError) -> Self {
        Self::Jsonl(source)
    }
}

/// 以当前外部 JSONL 为结构来源，按 WriteBack 正文选项建立当前 `text`。
#[cfg(test)]
pub(crate) fn build_write_back_candidate(
    stored: &GenericStoredSnapshot,
    live: &GenericInputSnapshot,
    current_translations: &GenericUnitMap<GenericCurrentTranslation>,
) -> Result<GenericWriteBackCandidate, GenericWriteBackError> {
    let placeholder_rules = GenericPlaceholderService::default()
        .compile(Vec::new())
        .expect("空 Generic Placeholder 规则必须有效");
    let layout_rules = LayoutRuleSet::from_canonical_json("[]")
        .and_then(|rules| compile_generic_layout_rules(live, &rules))
        .expect("空排版规则必须对任意 Generic 输入有效");
    build_write_back_candidate_with_cancellation(
        stored,
        live,
        current_translations,
        &placeholder_rules,
        &layout_rules,
        GenericWriteBackTextOptions::new(true, true),
        &CooperativeCancellation::default(),
    )
}

/// 构造写回候选，并在文件、Group、Unit、长文本及 JSON 往返边界响应取消。
pub(crate) fn build_write_back_candidate_with_cancellation(
    stored: &GenericStoredSnapshot,
    live: &GenericInputSnapshot,
    current_translations: &GenericUnitMap<GenericCurrentTranslation>,
    placeholder_rules: &GenericCompiledPlaceholderRules,
    layout_rules: &GenericCompiledLayoutRules,
    text_options: GenericWriteBackTextOptions,
    cancellation: &CooperativeCancellation,
) -> Result<GenericWriteBackCandidate, GenericWriteBackError> {
    ensure_write_back_running(cancellation)?;
    if stored.project().extracted_raw_fingerprint() != Some(live.raw_fingerprint()) {
        return Err(GenericWriteBackError::SourceChanged);
    }
    if stored.files().len() != live.files().len() {
        return Err(GenericWriteBackError::SnapshotMismatch(
            GenericWriteBackSnapshotProblem::FileCount {
                stored: stored.files().len(),
                input: live.files().len(),
            },
        ));
    }

    let built_files = stored
        .files()
        .par_iter()
        .zip(live.files().par_iter())
        .map(|(stored_file, live_file)| {
            build_write_back_file(
                stored_file,
                live_file,
                current_translations,
                placeholder_rules,
                layout_rules,
                text_options,
                cancellation,
            )
        })
        .collect::<Vec<_>>();

    let mut translated_units = 0;
    let mut retained_source_units = 0;
    let mut files = Vec::with_capacity(live.files().len());
    // Rayon 的 indexed collect 保留文件顺序；这里再按自然顺序取出结果，
    // 因而多个文件同时失败时仍返回自然顺序最早的错误。
    for result in built_files {
        ensure_write_back_running(cancellation)?;
        let built = result?;
        translated_units += built.translated_units;
        retained_source_units += built.retained_source_units;
        files.push(built.file);
    }
    ensure_write_back_running(cancellation)?;

    Ok(GenericWriteBackCandidate {
        files,
        translated_units,
        retained_source_units,
    })
}

struct BuiltWriteBackFile {
    file: GenericWriteBackFile,
    translated_units: usize,
    retained_source_units: usize,
}

fn build_write_back_file(
    stored_file: &super::project::GenericStoredFile,
    live_file: &GenericFile,
    translations: &GenericUnitMap<GenericCurrentTranslation>,
    placeholder_rules: &GenericCompiledPlaceholderRules,
    layout_rules: &GenericCompiledLayoutRules,
    text_options: GenericWriteBackTextOptions,
    cancellation: &CooperativeCancellation,
) -> Result<BuiltWriteBackFile, GenericWriteBackError> {
    ensure_write_back_running(cancellation)?;
    if stored_file.relative_path() != live_file.relative_path() {
        return Err(GenericWriteBackError::SnapshotMismatch(
            GenericWriteBackSnapshotProblem::FilePath {
                stored_path: SafePath::new(stored_file.relative_path()),
                input_path: SafePath::new(live_file.relative_path()),
            },
        ));
    }
    if stored_file.groups().len() != live_file.groups().len() {
        return Err(GenericWriteBackError::SnapshotMismatch(
            GenericWriteBackSnapshotProblem::GroupCount {
                relative_path: SafePath::new(live_file.relative_path()),
                stored: stored_file.groups().len(),
                input: live_file.groups().len(),
            },
        ));
    }

    let mut translated_units = 0;
    let mut retained_source_units = 0;
    let mut output_groups = Vec::with_capacity(live_file.groups().len());
    for (group_ordinal, (stored_group, live_group)) in stored_file
        .groups()
        .iter()
        .zip(live_file.groups())
        .enumerate()
    {
        ensure_write_back_running(cancellation)?;
        validate_group_shape(
            stored_group,
            live_group,
            live_file.relative_path(),
            group_ordinal,
            cancellation,
        )?;
        let mut output_units = Vec::with_capacity(live_group.units().len());
        for (unit_ordinal, unit) in live_group.units().iter().enumerate() {
            ensure_write_back_text_running(unit.text(), cancellation)?;
            let translation =
                translations.get_parts_with_cancellation(live_group.id(), unit.id(), || {
                    ensure_write_back_running(cancellation)
                })?;
            let text = if let Some(translation) = translation {
                ensure_write_back_text_running(translation.text(), cancellation)?;
                translated_units += 1;
                let context = GenericWriteBackUnitContext {
                    relative_path: live_file.relative_path(),
                    group_id: live_group.id(),
                    unit_id: unit.id(),
                    kind: live_group.kind(),
                    line: group_ordinal + 1,
                    unit: unit_ordinal + 1,
                };
                let mut validated = validate_translation_candidate(
                    &context,
                    unit.text(),
                    translation.text(),
                    placeholder_rules,
                    cancellation,
                )?;
                let punctuated = if text_options.repair_punctuation && !translation.is_manual() {
                    match repair_punctuation_with_cancellation(
                        &validated.source,
                        &validated.translation,
                        || ensure_write_back_running(cancellation),
                    )? {
                        Ok(PunctuationRepairOutcome::Repaired(text)) => Cow::Owned(text),
                        Ok(
                            PunctuationRepairOutcome::Unchanged | PunctuationRepairOutcome::Skipped,
                        ) => Cow::Borrowed(translation.text()),
                        Err(PunctuationRepairError::SourceProjection(source)) => {
                            return Err(GenericWriteBackError::LanguageProjection {
                                unit: context.diagnostic_locator(),
                                side: GenericWriteBackTextSide::Source,
                                source,
                            });
                        }
                        Err(PunctuationRepairError::TranslationProjection(source)) => {
                            return Err(GenericWriteBackError::LanguageProjection {
                                unit: context.diagnostic_locator(),
                                side: GenericWriteBackTextSide::Translation,
                                source,
                            });
                        }
                    }
                } else {
                    Cow::Borrowed(translation.text())
                };
                let max_width =
                    layout_rules.width_with_cancellation(live_group.id(), unit.id(), || {
                        ensure_write_back_running(cancellation)
                    })?;
                if max_width.is_some() || text_options.complete_continuation_whitespace {
                    if matches!(punctuated, Cow::Owned(_)) {
                        validated.translation = validate_replacement_candidate(
                            &context,
                            &validated.source,
                            punctuated.as_ref(),
                            placeholder_rules,
                            cancellation,
                        )?;
                    }
                    if let Some(layout) = layout_text(
                        validated.translation.text(),
                        max_width,
                        text_options.complete_continuation_whitespace,
                    ) {
                        let protected = layout.joined_text();
                        let restored = match validated
                            .translation
                            .restore_with_cancellation(&protected, || {
                                ensure_write_back_running(cancellation)
                            })? {
                            Ok(restored) => restored,
                            Err(source) => {
                                return Err(GenericWriteBackError::LayoutRestoration {
                                    unit: context.diagnostic_locator(),
                                    source,
                                });
                            }
                        };
                        if !text_equal_with_cancellation(
                            &restored,
                            punctuated.as_ref(),
                            cancellation,
                        )? {
                            validate_replacement_candidate(
                                &context,
                                &validated.source,
                                &restored,
                                placeholder_rules,
                                cancellation,
                            )?;
                        }
                        Cow::Owned(restored)
                    } else {
                        punctuated
                    }
                } else {
                    punctuated
                }
            } else {
                retained_source_units += 1;
                Cow::Borrowed(unit.text())
            };
            output_units.push(unit.clone_with_text_with_cancellation(&text, cancellation)?);
        }
        output_groups
            .push(live_group.clone_with_units_with_cancellation(output_units, cancellation)?);
    }

    let bytes = serialize_groups_with_cancellation(&output_groups, cancellation)?;
    let validated = validate_round_trip(live_file, bytes, &output_groups, cancellation)?;
    Ok(BuiltWriteBackFile {
        file: GenericWriteBackFile { validated },
        translated_units,
        retained_source_units,
    })
}

struct GenericWriteBackUnitContext<'unit> {
    relative_path: &'unit Path,
    group_id: &'unit str,
    unit_id: &'unit str,
    kind: &'unit str,
    line: usize,
    unit: usize,
}

impl GenericWriteBackUnitContext<'_> {
    fn diagnostic_locator(&self) -> GenericUnitLocator {
        GenericUnitLocator::new(
            self.relative_path,
            self.group_id,
            self.unit_id,
            Some(self.kind),
        )
        .with_natural_position(self.line, self.unit)
    }

    fn readable_id(&self) -> String {
        super::readable_generic_unit_id(self.relative_path, self.line, self.unit)
    }
}

struct ValidatedGenericTranslation {
    source: super::placeholder::GenericProtectedText,
    translation: super::placeholder::GenericProtectedText,
}

fn validate_translation_candidate(
    context: &GenericWriteBackUnitContext<'_>,
    source: &str,
    translation: &str,
    placeholder_rules: &GenericCompiledPlaceholderRules,
    cancellation: &CooperativeCancellation,
) -> Result<ValidatedGenericTranslation, GenericWriteBackError> {
    if let Err(problem) = validate_reflowed_candidate_text_with_cancellation(translation, || {
        ensure_write_back_running(cancellation)
    })? {
        return Err(GenericWriteBackError::CandidateViolation {
            unit: context.diagnostic_locator(),
            problem,
        });
    }
    let service = GenericPlaceholderService::default();
    let unit_locator = || context.diagnostic_locator();
    let target_id = context.readable_id();
    let source_view = match service.protect_compiled_target_with_cancellation(
        &target_id,
        context.kind,
        source,
        placeholder_rules,
        || ensure_write_back_running(cancellation),
    )? {
        Ok(source) => source,
        Err(source) => {
            return Err(GenericWriteBackError::PlaceholderProtection {
                unit: unit_locator(),
                side: GenericWriteBackTextSide::Source,
                source,
            });
        }
    };
    let translation_view = bind_translation_candidate(
        context,
        &source_view,
        translation,
        placeholder_rules,
        cancellation,
    )?;
    Ok(ValidatedGenericTranslation {
        source: source_view,
        translation: translation_view,
    })
}

fn validate_replacement_candidate(
    context: &GenericWriteBackUnitContext<'_>,
    source: &super::placeholder::GenericProtectedText,
    translation: &str,
    placeholder_rules: &GenericCompiledPlaceholderRules,
    cancellation: &CooperativeCancellation,
) -> Result<super::placeholder::GenericProtectedText, GenericWriteBackError> {
    if let Err(problem) = validate_reflowed_candidate_text_with_cancellation(translation, || {
        ensure_write_back_running(cancellation)
    })? {
        return Err(GenericWriteBackError::CandidateViolation {
            unit: context.diagnostic_locator(),
            problem,
        });
    }
    bind_translation_candidate(
        context,
        source,
        translation,
        placeholder_rules,
        cancellation,
    )
}

fn bind_translation_candidate(
    context: &GenericWriteBackUnitContext<'_>,
    source: &super::placeholder::GenericProtectedText,
    translation: &str,
    placeholder_rules: &GenericCompiledPlaceholderRules,
    cancellation: &CooperativeCancellation,
) -> Result<super::placeholder::GenericProtectedText, GenericWriteBackError> {
    let unit_locator = || context.diagnostic_locator();
    match GenericPlaceholderService::default().bind_target_candidate_with_cancellation(
        source,
        &context.readable_id(),
        context.kind,
        translation,
        placeholder_rules,
        || ensure_write_back_running(cancellation),
    )? {
        Ok(translation) => Ok(translation),
        Err(GenericSourceBoundPlaceholderError::Protection(source)) => {
            Err(GenericWriteBackError::PlaceholderProtection {
                unit: unit_locator(),
                side: GenericWriteBackTextSide::Translation,
                source,
            })
        }
        Err(GenericSourceBoundPlaceholderError::Projection(source)) => {
            Err(GenericWriteBackError::LanguageProjection {
                unit: unit_locator(),
                side: GenericWriteBackTextSide::Translation,
                source,
            })
        }
        Err(GenericSourceBoundPlaceholderError::Mismatch) => {
            Err(GenericWriteBackError::PlaceholderBindingMismatch {
                unit: unit_locator(),
            })
        }
    }
}

fn validate_group_shape(
    stored: &super::project::GenericStoredGroup,
    live: &super::jsonl::GenericGroup,
    path: &Path,
    group_ordinal: usize,
    cancellation: &CooperativeCancellation,
) -> Result<(), GenericWriteBackError> {
    ensure_write_back_running(cancellation)?;
    if !text_equal_with_cancellation(stored.id(), live.id(), cancellation)?
        || !text_equal_with_cancellation(stored.kind(), live.kind(), cancellation)?
        || stored.units().len() != live.units().len()
    {
        return Err(GenericWriteBackError::SnapshotMismatch(
            GenericWriteBackSnapshotProblem::GroupShape {
                relative_path: SafePath::new(path),
                group_ordinal,
                group_id: SafeIdentifier::new(live.id()).ok(),
            },
        ));
    }
    for (unit_ordinal, (stored_unit, live_unit)) in
        stored.units().iter().zip(live.units()).enumerate()
    {
        ensure_write_back_text_running(live_unit.text(), cancellation)?;
        if !text_equal_with_cancellation(stored_unit.id(), live_unit.id(), cancellation)?
            || !text_equal_with_cancellation(
                stored_unit.source_text(),
                live_unit.text(),
                cancellation,
            )?
        {
            return Err(GenericWriteBackError::SnapshotMismatch(
                GenericWriteBackSnapshotProblem::UnitShapeOrSource {
                    relative_path: SafePath::new(path),
                    group_ordinal,
                    unit_ordinal,
                    group_id: SafeIdentifier::new(live.id()).ok(),
                    unit_id: SafeIdentifier::new(live_unit.id()).ok(),
                },
            ));
        }
    }
    Ok(())
}

fn validate_round_trip(
    source: &GenericFile,
    candidate_bytes: Vec<u8>,
    expected_groups: &[super::jsonl::GenericGroup],
    cancellation: &CooperativeCancellation,
) -> Result<GenericFile, GenericWriteBackError> {
    let candidate = parse_file_with_cancellation(
        source.relative_path().to_path_buf(),
        candidate_bytes,
        cancellation,
    )?;
    if source.groups().len() != candidate.groups().len() {
        return Err(GenericWriteBackError::SnapshotMismatch(
            GenericWriteBackSnapshotProblem::RoundTripGroupCount {
                relative_path: SafePath::new(source.relative_path()),
                source: source.groups().len(),
                candidate: candidate.groups().len(),
            },
        ));
    }
    for (group_ordinal, ((original_group, expected_group), candidate_group)) in source
        .groups()
        .iter()
        .zip(expected_groups)
        .zip(candidate.groups())
        .enumerate()
    {
        ensure_write_back_running(cancellation)?;
        if !text_equal_with_cancellation(original_group.id(), candidate_group.id(), cancellation)?
            || !text_equal_with_cancellation(
                original_group.kind(),
                candidate_group.kind(),
                cancellation,
            )?
            || original_group.units().len() != candidate_group.units().len()
        {
            return Err(GenericWriteBackError::SnapshotMismatch(
                GenericWriteBackSnapshotProblem::RoundTripGroupShape {
                    relative_path: SafePath::new(source.relative_path()),
                    group_ordinal,
                    group_id: SafeIdentifier::new(original_group.id()).ok(),
                },
            ));
        }
        for (unit_ordinal, ((original, expected), candidate)) in original_group
            .units()
            .iter()
            .zip(expected_group.units())
            .zip(candidate_group.units())
            .enumerate()
        {
            ensure_write_back_text_running(candidate.text(), cancellation)?;
            if !text_equal_with_cancellation(original.id(), candidate.id(), cancellation)? {
                return Err(GenericWriteBackError::SnapshotMismatch(
                    GenericWriteBackSnapshotProblem::RoundTripUnitId {
                        relative_path: SafePath::new(source.relative_path()),
                        group_ordinal,
                        unit_ordinal,
                        group_id: SafeIdentifier::new(original_group.id()).ok(),
                        expected_unit_id: SafeIdentifier::new(original.id()).ok(),
                        actual_unit_id: SafeIdentifier::new(candidate.id()).ok(),
                    },
                ));
            }
            if !text_equal_with_cancellation(candidate.text(), expected.text(), cancellation)? {
                return Err(GenericWriteBackError::SnapshotMismatch(
                    GenericWriteBackSnapshotProblem::RoundTripUnitText {
                        relative_path: SafePath::new(source.relative_path()),
                        group_ordinal,
                        unit_ordinal,
                        group_id: SafeIdentifier::new(original_group.id()).ok(),
                        unit_id: SafeIdentifier::new(original.id()).ok(),
                    },
                ));
            }
        }
    }
    ensure_write_back_running(cancellation)?;
    Ok(candidate)
}

/// 用生产解析器复查实际落盘内容，并与已经通过候选验证的文件逐字节、逐结构比较。
#[cfg(test)]
pub(crate) fn validate_materialized_write_back_file(
    expected: &GenericWriteBackFile,
    materialized_bytes: Vec<u8>,
) -> Result<(), GenericWriteBackError> {
    validate_materialized_write_back_file_with_cancellation(
        expected,
        materialized_bytes,
        &CooperativeCancellation::default(),
    )
}

pub(crate) fn validate_materialized_write_back_file_with_cancellation(
    expected: &GenericWriteBackFile,
    materialized_bytes: Vec<u8>,
    cancellation: &CooperativeCancellation,
) -> Result<(), GenericWriteBackError> {
    let materialized = parse_file_with_cancellation(
        expected.relative_path().to_path_buf(),
        materialized_bytes,
        cancellation,
    )?;
    let bytes_changed =
        !bytes_equal_with_cancellation(materialized.raw_bytes(), expected.bytes(), cancellation)?;
    let structure_changed = !groups_equal_with_cancellation(
        materialized.groups(),
        expected.validated.groups(),
        cancellation,
    )?;
    if bytes_changed || structure_changed {
        return Err(GenericWriteBackError::MaterializedMismatch {
            path: expected.relative_path().to_path_buf(),
            bytes_changed,
            structure_changed,
        });
    }
    Ok(())
}

fn ensure_write_back_running(
    cancellation: &CooperativeCancellation,
) -> Result<(), GenericWriteBackError> {
    if cancellation.is_requested() {
        Err(GenericJsonlError::Cancelled.into())
    } else {
        Ok(())
    }
}

fn ensure_write_back_text_running(
    text: &str,
    cancellation: &CooperativeCancellation,
) -> Result<(), GenericWriteBackError> {
    const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;

    for _ in text.as_bytes().chunks(CANCELLATION_CHECK_BYTES) {
        ensure_write_back_running(cancellation)?;
    }
    ensure_write_back_running(cancellation)
}

fn bytes_equal_with_cancellation(
    left: &[u8],
    right: &[u8],
    cancellation: &CooperativeCancellation,
) -> Result<bool, GenericWriteBackError> {
    const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;

    ensure_write_back_running(cancellation)?;
    if left.len() != right.len() {
        return Ok(false);
    }
    for (left, right) in left
        .chunks(CANCELLATION_CHECK_BYTES)
        .zip(right.chunks(CANCELLATION_CHECK_BYTES))
    {
        ensure_write_back_running(cancellation)?;
        if left != right {
            return Ok(false);
        }
    }
    ensure_write_back_running(cancellation)?;
    Ok(true)
}

fn text_equal_with_cancellation(
    left: &str,
    right: &str,
    cancellation: &CooperativeCancellation,
) -> Result<bool, GenericWriteBackError> {
    bytes_equal_with_cancellation(left.as_bytes(), right.as_bytes(), cancellation)
}

fn groups_equal_with_cancellation(
    left: &[super::jsonl::GenericGroup],
    right: &[super::jsonl::GenericGroup],
    cancellation: &CooperativeCancellation,
) -> Result<bool, GenericWriteBackError> {
    ensure_write_back_running(cancellation)?;
    if left.len() != right.len() {
        return Ok(false);
    }
    for (left_group, right_group) in left.iter().zip(right) {
        ensure_write_back_running(cancellation)?;
        if !text_equal_with_cancellation(left_group.id(), right_group.id(), cancellation)?
            || !text_equal_with_cancellation(left_group.kind(), right_group.kind(), cancellation)?
            || left_group.units().len() != right_group.units().len()
        {
            return Ok(false);
        }
        for (left_unit, right_unit) in left_group.units().iter().zip(right_group.units()) {
            ensure_write_back_running(cancellation)?;
            if !text_equal_with_cancellation(left_unit.id(), right_unit.id(), cancellation)?
                || !text_equal_with_cancellation(left_unit.text(), right_unit.text(), cancellation)?
            {
                return Ok(false);
            }
        }
    }
    ensure_write_back_running(cancellation)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use crate::fingerprint::Sha256Fingerprint;
    use crate::generic::placeholder::GenericPlaceholderRuleDefinition;
    use crate::generic::project::{GenericInitRequest, GenericProjectStore, TranslationWrite};
    use crate::language::LanguageId;

    use super::*;

    fn empty_layout_rules(live: &GenericInputSnapshot) -> GenericCompiledLayoutRules {
        let rules = LayoutRuleSet::from_canonical_json("[]").expect("空排版规则必须有效");
        compile_generic_layout_rules(live, &rules).expect("空排版规则必须适用于任意 Generic 输入")
    }

    #[test]
    fn candidate_changes_only_translated_text_and_keeps_empty_files() {
        let temp = tempdir().unwrap();
        let source_root = temp.path().join("source");
        fs::create_dir(&source_root).unwrap();
        fs::write(
            source_root.join("main.jsonl"),
            "{\"id\":\"g\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"a\",\"text\":\"原文\"},{\"id\":\"b\",\"text\":\"保留\"}]}\n",
        )
        .unwrap();
        fs::write(source_root.join("empty.jsonl"), []).unwrap();
        let (store, _) = GenericProjectStore::initialize(GenericInitRequest {
            project_name: "game".parse().unwrap(),
            workspace_root: temp.path().join("project"),
            source_root: Some(source_root),
            source_language: Some(LanguageId::parse("ja").unwrap()),
            target_language: Some(LanguageId::parse("zh-Hans").unwrap()),
        })
        .unwrap();
        store.extract().unwrap();
        let snapshot = store.load_snapshot().unwrap();
        let group = snapshot
            .files()
            .iter()
            .flat_map(|file| file.groups())
            .find(|group| group.id() == "g")
            .unwrap();
        let unit = &group.units()[0];
        store
            .commit_translations(
                snapshot.project().extracted_raw_fingerprint().unwrap(),
                &[TranslationWrite {
                    group_id: group.id().to_owned(),
                    unit_id: unit.id().to_owned(),
                    expected_source_text: unit.source_text().to_owned(),
                    expected_group_context: group.context_fingerprint(),
                    translation: "译文\n第二行".to_owned(),
                    state_fingerprint: Sha256Fingerprint::from_bytes([9; 32]),
                    expected_translation: None,
                    was_current_rejected: false,
                }],
            )
            .unwrap();

        let (stored, live) = store.ensure_input_current().unwrap();
        let mut current_translations = GenericUnitMap::new();
        let previous = current_translations
            .insert_with_cancellation(
                GenericUnitKey::new("g".to_owned(), "a".to_owned()),
                GenericCurrentTranslation::new("译文\n第二行".to_owned(), false),
                || Ok::<_, std::convert::Infallible>(()),
            )
            .unwrap_or_else(|never| match never {});
        assert!(previous.is_none());
        let candidate = build_write_back_candidate(&stored, &live, &current_translations).unwrap();
        assert_eq!(candidate.translated_units(), 1);
        assert_eq!(candidate.retained_source_units(), 1);
        assert_eq!(candidate.files().len(), 2);
        assert_eq!(
            candidate
                .files()
                .iter()
                .map(|file| file.relative_path())
                .collect::<Vec<_>>(),
            [Path::new("empty.jsonl"), Path::new("main.jsonl")]
        );
        let main = candidate
            .files()
            .iter()
            .find(|file| file.relative_path() == Path::new("main.jsonl"))
            .unwrap();
        assert_eq!(
            std::str::from_utf8(main.bytes()).unwrap(),
            "{\"id\":\"g\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"a\",\"text\":\"译文\\n第二行\"},{\"id\":\"b\",\"text\":\"保留\"}]}\n"
        );
        assert!(
            candidate
                .files()
                .iter()
                .find(|file| file.relative_path() == Path::new("empty.jsonl"))
                .unwrap()
                .bytes()
                .is_empty()
        );
    }

    #[test]
    fn generic_layout_inserts_lf_inside_unit_text_without_adding_jsonl_records() {
        let temp = tempdir().unwrap();
        let source_root = temp.path().join("source");
        fs::create_dir(&source_root).unwrap();
        fs::write(
            source_root.join("scene.jsonl"),
            "{\"id\":\"g\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"u\",\"text\":\"原文\"}]}\n",
        )
        .unwrap();
        let (store, _) = GenericProjectStore::initialize(GenericInitRequest {
            project_name: "generic-layout".parse().unwrap(),
            workspace_root: temp.path().join("project"),
            source_root: Some(source_root),
            source_language: Some(LanguageId::parse("ja").unwrap()),
            target_language: Some(LanguageId::parse("zh-Hans").unwrap()),
        })
        .unwrap();
        store.extract().unwrap();
        let (stored, live) = store.ensure_input_current().unwrap();
        let mut translations = GenericUnitMap::new();
        translations
            .insert_with_cancellation(
                GenericUnitKey::new("g".to_owned(), "u".to_owned()),
                GenericCurrentTranslation::new("「甲乙，丙丁」".to_owned(), false),
                || Ok::<_, std::convert::Infallible>(()),
            )
            .unwrap_or_else(|never| match never {});
        let placeholder_rules = GenericPlaceholderService::default()
            .compile(Vec::new())
            .expect("空 Placeholder 规则必须有效");
        let rules = LayoutRuleSet::parse_toml(
            b"[[rule]]\nmax_fullwidth_chars = 4\ngroup_ids = ['g']\nunit_ids = ['u']\n",
        )
        .expect("Generic 精确位置规则必须有效");
        let layout_rules =
            compile_generic_layout_rules(&live, &rules).expect("规则必须命中唯一 Generic Unit");

        let candidate = build_write_back_candidate_with_cancellation(
            &stored,
            &live,
            &translations,
            &placeholder_rules,
            &layout_rules,
            GenericWriteBackTextOptions::new(false, true),
            &CooperativeCancellation::default(),
        )
        .unwrap();
        let output = std::str::from_utf8(candidate.files()[0].bytes()).unwrap();
        assert_eq!(
            output.lines().count(),
            1,
            "Generic 仍须保持一条物理 JSONL 记录"
        );
        assert_eq!(
            output,
            "{\"id\":\"g\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"u\",\"text\":\"「甲乙，\\n　丙丁」\"}]}\n"
        );
    }

    #[test]
    fn generic_continuation_whitespace_switch_is_independent_of_layout_rules() {
        let temp = tempdir().unwrap();
        let source_root = temp.path().join("source");
        fs::create_dir(&source_root).unwrap();
        fs::write(
            source_root.join("scene.jsonl"),
            "{\"id\":\"g\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"u\",\"text\":\"原文\"}]}\n",
        )
        .unwrap();
        let (store, _) = GenericProjectStore::initialize(GenericInitRequest {
            project_name: "generic-indent".parse().unwrap(),
            workspace_root: temp.path().join("project"),
            source_root: Some(source_root),
            source_language: Some(LanguageId::parse("ja").unwrap()),
            target_language: Some(LanguageId::parse("zh-Hans").unwrap()),
        })
        .unwrap();
        store.extract().unwrap();
        let (stored, live) = store.ensure_input_current().unwrap();
        let mut translations = GenericUnitMap::new();
        translations
            .insert_with_cancellation(
                GenericUnitKey::new("g".to_owned(), "u".to_owned()),
                GenericCurrentTranslation::new("「甲\n乙」".to_owned(), false),
                || Ok::<_, std::convert::Infallible>(()),
            )
            .unwrap_or_else(|never| match never {});
        let placeholder_rules = GenericPlaceholderService::default()
            .compile(Vec::new())
            .expect("空 Placeholder 规则必须有效");
        let layout_rules = empty_layout_rules(&live);

        let unchanged = build_write_back_candidate_with_cancellation(
            &stored,
            &live,
            &translations,
            &placeholder_rules,
            &layout_rules,
            GenericWriteBackTextOptions::new(false, false),
            &CooperativeCancellation::default(),
        )
        .unwrap();
        assert!(
            std::str::from_utf8(unchanged.files()[0].bytes())
                .unwrap()
                .contains("甲\\n乙")
        );

        let completed = build_write_back_candidate_with_cancellation(
            &stored,
            &live,
            &translations,
            &placeholder_rules,
            &layout_rules,
            GenericWriteBackTextOptions::new(false, true),
            &CooperativeCancellation::default(),
        )
        .unwrap();
        assert!(
            std::str::from_utf8(completed.files()[0].bytes())
                .unwrap()
                .contains("甲\\n　乙")
        );
    }

    #[test]
    fn punctuation_repair_switch_controls_generic_write_back_text() {
        let temp = tempdir().unwrap();
        let source_root = temp.path().join("source");
        fs::create_dir(&source_root).unwrap();
        fs::write(
            source_root.join("scene.jsonl"),
            "{\"id\":\"g\",\"kind\":\"settings\",\"units\":[{\"id\":\"u\",\"text\":\"General, Misc, Audio, Toggle\"}]}\n",
        )
        .unwrap();
        let (store, _) = GenericProjectStore::initialize(GenericInitRequest {
            project_name: "symbol-write-back".parse().unwrap(),
            workspace_root: temp.path().join("project"),
            source_root: Some(source_root),
            source_language: Some(LanguageId::parse("en").unwrap()),
            target_language: Some(LanguageId::parse("zh-Hans").unwrap()),
        })
        .unwrap();
        store.extract().unwrap();
        let (stored, live) = store.ensure_input_current().unwrap();
        let mut translations = GenericUnitMap::new();
        translations
            .insert_with_cancellation(
                GenericUnitKey::new("g".to_owned(), "u".to_owned()),
                GenericCurrentTranslation::new("常规、杂项、声音、开关".to_owned(), false),
                || Ok::<_, std::convert::Infallible>(()),
            )
            .unwrap_or_else(|never| match never {});
        let placeholder_rules = GenericPlaceholderService::default()
            .compile(Vec::new())
            .expect("空 Placeholder 规则必须有效");
        let layout_rules = empty_layout_rules(&live);

        let unchanged = build_write_back_candidate_with_cancellation(
            &stored,
            &live,
            &translations,
            &placeholder_rules,
            &layout_rules,
            GenericWriteBackTextOptions::new(false, true),
            &CooperativeCancellation::default(),
        )
        .unwrap();

        assert_eq!(
            std::str::from_utf8(unchanged.files()[0].bytes()).unwrap(),
            "{\"id\":\"g\",\"kind\":\"settings\",\"units\":[{\"id\":\"u\",\"text\":\"常规、杂项、声音、开关\"}]}\n"
        );

        let repaired = build_write_back_candidate_with_cancellation(
            &stored,
            &live,
            &translations,
            &placeholder_rules,
            &layout_rules,
            GenericWriteBackTextOptions::new(true, true),
            &CooperativeCancellation::default(),
        )
        .unwrap();
        assert_eq!(
            std::str::from_utf8(repaired.files()[0].bytes()).unwrap(),
            "{\"id\":\"g\",\"kind\":\"settings\",\"units\":[{\"id\":\"u\",\"text\":\"常规,杂项,声音,开关\"}]}\n"
        );
    }

    #[test]
    fn candidate_materializes_natural_text_around_opaque_placeholders_exactly() {
        let temp = tempdir().unwrap();
        let source_root = temp.path().join("source");
        fs::create_dir(&source_root).unwrap();
        fs::write(
            source_root.join("scene.jsonl"),
            "{\"id\":\"g\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"u\",\"text\":\"Open [A,B], now.\"}]}\n",
        )
        .unwrap();
        let (store, _) = GenericProjectStore::initialize(GenericInitRequest {
            project_name: "opaque-symbol-write-back".parse().unwrap(),
            workspace_root: temp.path().join("project"),
            source_root: Some(source_root),
            source_language: Some(LanguageId::parse("en").unwrap()),
            target_language: Some(LanguageId::parse("zh-Hans").unwrap()),
        })
        .unwrap();
        store.extract().unwrap();
        let (stored, live) = store.ensure_input_current().unwrap();
        let mut translations = GenericUnitMap::new();
        translations
            .insert_with_cancellation(
                GenericUnitKey::new("g".to_owned(), "u".to_owned()),
                GenericCurrentTranslation::new("打开 [A,B]、现在。".to_owned(), false),
                || Ok::<_, std::convert::Infallible>(()),
            )
            .unwrap_or_else(|never| match never {});
        let placeholder_rules = GenericPlaceholderService::default()
            .compile(vec![GenericPlaceholderRuleDefinition::new(
                Some(vec!["dialogue".to_owned()]),
                r"\[[^]]+\]",
            )])
            .expect("对话 Placeholder 规则必须有效");
        let layout_rules = empty_layout_rules(&live);

        let candidate = build_write_back_candidate_with_cancellation(
            &stored,
            &live,
            &translations,
            &placeholder_rules,
            &layout_rules,
            GenericWriteBackTextOptions::new(true, true),
            &CooperativeCancellation::default(),
        )
        .unwrap();

        assert_eq!(
            std::str::from_utf8(candidate.files()[0].bytes()).unwrap(),
            "{\"id\":\"g\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"u\",\"text\":\"打开 [A,B],现在.\"}]}\n"
        );
    }

    #[test]
    fn candidate_materializes_wrapper_translation_exactly() {
        let temp = tempdir().unwrap();
        let source_root = temp.path().join("source");
        fs::create_dir(&source_root).unwrap();
        fs::write(
            source_root.join("scene.jsonl"),
            "{\"id\":\"g\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"u\",\"text\":\"<msg>General, Misc</msg>\"}]}\n",
        )
        .unwrap();
        let (store, _) = GenericProjectStore::initialize(GenericInitRequest {
            project_name: "wrapper-symbol-write-back".parse().unwrap(),
            workspace_root: temp.path().join("project"),
            source_root: Some(source_root),
            source_language: Some(LanguageId::parse("en").unwrap()),
            target_language: Some(LanguageId::parse("zh-Hans").unwrap()),
        })
        .unwrap();
        store.extract().unwrap();
        let (stored, live) = store.ensure_input_current().unwrap();
        let mut translations = GenericUnitMap::new();
        translations
            .insert_with_cancellation(
                GenericUnitKey::new("g".to_owned(), "u".to_owned()),
                GenericCurrentTranslation::new("<msg>常规、杂项</msg>".to_owned(), false),
                || Ok::<_, std::convert::Infallible>(()),
            )
            .unwrap_or_else(|never| match never {});
        let placeholder_rules = GenericPlaceholderService::default()
            .compile(vec![GenericPlaceholderRuleDefinition::new(
                Some(vec!["dialogue".to_owned()]),
                r"<msg>(?<text>.*?)</msg>",
            )])
            .expect("wrapper Placeholder 规则必须有效");
        let layout_rules = empty_layout_rules(&live);

        let candidate = build_write_back_candidate_with_cancellation(
            &stored,
            &live,
            &translations,
            &placeholder_rules,
            &layout_rules,
            GenericWriteBackTextOptions::new(true, true),
            &CooperativeCancellation::default(),
        )
        .unwrap();

        assert_eq!(
            std::str::from_utf8(candidate.files()[0].bytes()).unwrap(),
            "{\"id\":\"g\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"u\",\"text\":\"<msg>常规,杂项</msg>\"}]}\n"
        );
    }

    #[test]
    fn candidate_write_back_uses_source_binding_when_the_label_is_translated() {
        let temp = tempdir().unwrap();
        let source_root = temp.path().join("source");
        fs::create_dir(&source_root).unwrap();
        fs::write(
            source_root.join("scene.jsonl"),
            "{\"id\":\"g\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"u\",\"text\":\"Name: abc-123\"}]}\n",
        )
        .unwrap();
        let (store, _) = GenericProjectStore::initialize(GenericInitRequest {
            project_name: "source-bound-write-back".parse().unwrap(),
            workspace_root: temp.path().join("project"),
            source_root: Some(source_root),
            source_language: Some(LanguageId::parse("en").unwrap()),
            target_language: Some(LanguageId::parse("zh-Hans").unwrap()),
        })
        .unwrap();
        store.extract().unwrap();
        let (stored, live) = store.ensure_input_current().unwrap();
        let mut translations = GenericUnitMap::new();
        translations
            .insert_with_cancellation(
                GenericUnitKey::new("g".to_owned(), "u".to_owned()),
                GenericCurrentTranslation::new("名称：abc-123".to_owned(), true),
                || Ok::<_, std::convert::Infallible>(()),
            )
            .unwrap_or_else(|never| match never {});
        let placeholder_rules = GenericPlaceholderService::default()
            .compile(vec![GenericPlaceholderRuleDefinition::new(
                Some(vec!["dialogue".to_owned()]),
                r"(?<=Name: )[A-Za-z0-9-]+",
            )])
            .expect("lookbehind Placeholder 规则必须有效");
        let layout_rules = empty_layout_rules(&live);

        let candidate = build_write_back_candidate_with_cancellation(
            &stored,
            &live,
            &translations,
            &placeholder_rules,
            &layout_rules,
            GenericWriteBackTextOptions::new(false, false),
            &CooperativeCancellation::default(),
        )
        .expect("译文标签变化不应使已绑定的凭据失效");

        assert_eq!(
            std::str::from_utf8(candidate.files()[0].bytes()).unwrap(),
            "{\"id\":\"g\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"u\",\"text\":\"名称：abc-123\"}]}\n"
        );
    }

    #[test]
    fn candidate_rejects_translation_when_placeholder_binding_does_not_match() {
        let temp = tempdir().unwrap();
        let source_root = temp.path().join("source");
        fs::create_dir(&source_root).unwrap();
        fs::write(
            source_root.join("scene.jsonl"),
            "{\"id\":\"g\",\"kind\":\"settings\",\"units\":[{\"id\":\"u\",\"text\":\"General, Misc\"}]}\n",
        )
        .unwrap();
        let (store, _) = GenericProjectStore::initialize(GenericInitRequest {
            project_name: "skipped-symbol-write-back".parse().unwrap(),
            workspace_root: temp.path().join("project"),
            source_root: Some(source_root),
            source_language: Some(LanguageId::parse("en").unwrap()),
            target_language: Some(LanguageId::parse("zh-Hans").unwrap()),
        })
        .unwrap();
        store.extract().unwrap();
        let (stored, live) = store.ensure_input_current().unwrap();
        let mut translations = GenericUnitMap::new();
        translations
            .insert_with_cancellation(
                GenericUnitKey::new("g".to_owned(), "u".to_owned()),
                GenericCurrentTranslation::new("[常规、杂项]".to_owned(), false),
                || Ok::<_, std::convert::Infallible>(()),
            )
            .unwrap_or_else(|never| match never {});
        let placeholder_rules = GenericPlaceholderService::default()
            .compile(vec![GenericPlaceholderRuleDefinition::new(
                Some(vec!["settings".to_owned()]),
                r"\[[^]]+\]",
            )])
            .expect("设置 Placeholder 规则必须有效");
        let layout_rules = empty_layout_rules(&live);

        let error = build_write_back_candidate_with_cancellation(
            &stored,
            &live,
            &translations,
            &placeholder_rules,
            &layout_rules,
            GenericWriteBackTextOptions::new(true, true),
            &CooperativeCancellation::default(),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            GenericWriteBackError::PlaceholderBindingMismatch {
                unit,
            } if unit.relative_path.to_string() == "scene.jsonl"
                && unit.group_id.as_ref().is_some_and(|value| value.to_string() == "g")
                && unit.unit_id.as_ref().is_some_and(|value| value.to_string() == "u")
                && unit.role.as_ref().is_some_and(|value| value.to_string() == "settings")
        ));
    }

    #[test]
    fn materialized_file_validation_uses_production_parser_and_keeps_empty_file_valid() {
        let expected = GenericWriteBackFile {
            validated: parse_file(
                PathBuf::from("scene.jsonl"),
                concat!(
                    r#"{"id":"g","kind":"dialogue","units":[{"id":"u","text":"译文"}]}"#,
                    "\n"
                )
                .as_bytes()
                .to_vec(),
            )
            .unwrap(),
        };
        validate_materialized_write_back_file(&expected, expected.bytes().to_vec()).unwrap();

        let mut byte_changed = expected.bytes().to_vec();
        byte_changed.insert(1, b' ');
        assert!(matches!(
            validate_materialized_write_back_file(&expected, byte_changed),
            Err(GenericWriteBackError::MaterializedMismatch {
                bytes_changed: true,
                structure_changed: false,
                ..
            })
        ));
        assert!(matches!(
            validate_materialized_write_back_file(
                &expected,
                concat!(
                    r#"{"id":"g","kind":"dialogue","units":[{"id":"u","text":"被改写"}]}"#,
                    "\n"
                )
                .as_bytes()
                .to_vec()
            ),
            Err(GenericWriteBackError::MaterializedMismatch {
                bytes_changed: true,
                structure_changed: true,
                ..
            })
        ));
        assert!(matches!(
            validate_materialized_write_back_file(&expected, b"not-json\n".to_vec()),
            Err(GenericWriteBackError::Jsonl(_))
        ));

        let empty = GenericWriteBackFile {
            validated: parse_file(PathBuf::from("empty.jsonl"), Vec::new()).unwrap(),
        };
        validate_materialized_write_back_file(&empty, Vec::new()).unwrap();
    }
}
