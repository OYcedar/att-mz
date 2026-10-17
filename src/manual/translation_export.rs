//! 面向译后 QA 的完整导出：项目语言只写一次，正文与自然位置仍逐 Unit 输出。

use super::*;
use crate::language::{
    LanguageOperationCancelled, LanguageQaReview, LanguageText, LanguageTextSegment,
    translation_qa_reviews,
};

#[derive(Serialize)]
struct TranslationExportHeader<'a> {
    source_language: &'a str,
    target_language: &'a str,
}

#[derive(Serialize)]
struct TranslationExportRecord<'a> {
    manual_id: &'a str,
    group: &'a str,
    source: &'a [String],
    #[serde(flatten)]
    status: TranslationExportStatus<'a>,
    #[serde(rename = "type")]
    kind: ManualTranslationType,
    #[serde(skip_serializing_if = "Option::is_none")]
    owner: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rule_number: Option<usize>,
    reviews: Vec<LanguageQaReview>,
    #[serde(skip_serializing_if = "Option::is_none")]
    natural_source: Option<Vec<Option<&'a str>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    natural_translation: Option<Vec<Option<&'a str>>>,
}

fn natural_projection<'a>(
    text: &'a LanguageText,
    original: &[String],
) -> Option<Vec<Option<&'a str>>> {
    if matches!(text.segments(), [LanguageTextSegment::NaturalText(value)] if *value == original.join("\n"))
        || (text.segments().is_empty() && original.join("\n").is_empty())
    {
        return None;
    }
    Some(
        text.segments()
            .iter()
            .map(|segment| match segment {
                LanguageTextSegment::NaturalText(value) => Some(value.as_str()),
                LanguageTextSegment::OpaqueBoundary => None,
            })
            .collect(),
    )
}

fn projection_error(path: &Path, id: &str) -> ManualDocumentError {
    ManualDocumentError::Write {
        path: path.to_path_buf(),
        source: io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{id} 无法按当前 Placeholder 建立审校文本；重新核对当前项目"),
        ),
    }
}

fn review_projection(
    validator: &ManualPlaceholderValidator,
    entry: &ManualTranslationEntry,
    path: &Path,
    cancellation: &CooperativeCancellation,
) -> Result<(LanguageText, Option<LanguageText>), ManualDocumentError> {
    let source_text = entry.source.join("\n");
    let target_text = entry
        .current_translation
        .as_ref()
        .map(|lines| lines.join("\n"));
    let mut source_segments = Vec::new();
    let mut target_segments = Vec::new();
    let mut append = |source: LanguageText, target: Option<LanguageText>, separator: bool| {
        if separator {
            source_segments.push(LanguageTextSegment::NaturalText("\n".to_owned()));
            target_segments.push(LanguageTextSegment::NaturalText("\n".to_owned()));
        }
        source_segments.extend(source.segments().iter().cloned());
        if let Some(target) = target {
            target_segments.extend(target.segments().iter().cloned());
        }
    };
    match validator {
        ManualPlaceholderValidator::Generic { service, compiled } => {
            let source = service
                .protect_target_with_cancellation(
                    &entry.id,
                    &entry.placeholder_scope,
                    &source_text,
                    compiled,
                    || ensure_manual_document_running(cancellation),
                )?
                .map_err(|_| projection_error(path, &entry.id))?;
            let target = target_text
                .as_ref()
                .map(|target| {
                    let bound = service
                        .bind_target_candidate_with_cancellation(
                            &source,
                            &entry.id,
                            &entry.placeholder_scope,
                            target,
                            compiled,
                            || ensure_manual_document_running(cancellation),
                        )?
                        .map_err(|_| projection_error(path, &entry.id))?;
                    bound
                        .language_text_with_cancellation(|| {
                            ensure_manual_document_running(cancellation)
                        })?
                        .map_err(|_| projection_error(path, &entry.id))
                })
                .transpose()?;
            let source = source
                .language_text_with_cancellation(|| ensure_manual_document_running(cancellation))?
                .map_err(|_| projection_error(path, &entry.id))?;
            append(source, target, false);
        }
        ManualPlaceholderValidator::RpgMaker {
            engine,
            service,
            compiled,
            profiles,
        } => {
            let kind = TextGroupKind::from_storage_name(&entry.placeholder_scope)
                .ok_or_else(|| projection_error(path, &entry.id))?;
            let profile = *profiles
                .get(&entry.id)
                .ok_or_else(|| projection_error(path, &entry.id))?;
            let slots: Vec<(&str, Option<&str>)> = match entry.kind {
                ManualTranslationType::Free => vec![(&source_text, target_text.as_deref())],
                ManualTranslationType::Fixed => entry
                    .source
                    .iter()
                    .enumerate()
                    .map(|(index, source)| {
                        (
                            source.as_str(),
                            entry
                                .current_translation
                                .as_ref()
                                .map(|target| target[index].as_str()),
                        )
                    })
                    .collect(),
            };
            for (index, (source, target)) in slots.into_iter().enumerate() {
                let source = service
                    .protect_profile_with_cancellation(
                        *engine,
                        kind,
                        &entry.id,
                        profile,
                        source,
                        &[],
                        compiled,
                        || ensure_manual_document_running(cancellation),
                    )?
                    .map_err(|_| projection_error(path, &entry.id))?;
                let target = target
                    .map(|target| {
                        let bound = service
                            .bind_profile_candidate_with_cancellation(
                                &source,
                                *engine,
                                kind,
                                &entry.id,
                                profile,
                                target,
                                compiled,
                                || ensure_manual_document_running(cancellation),
                            )?
                            .map_err(|_| projection_error(path, &entry.id))?;
                        bound
                            .language_text_with_cancellation(|| {
                                ensure_manual_document_running(cancellation)
                            })?
                            .map_err(|_| projection_error(path, &entry.id))
                    })
                    .transpose()?;
                let source = source
                    .language_text_with_cancellation(|| {
                        ensure_manual_document_running(cancellation)
                    })?
                    .map_err(|_| projection_error(path, &entry.id))?;
                append(source, target, index > 0);
            }
        }
    }
    let source = LanguageText::new_with_cancellation(source_segments, || {
        ensure_manual_document_running(cancellation)
    })?;
    let target = target_text
        .map(|_| {
            LanguageText::new_with_cancellation(target_segments, || {
                ensure_manual_document_running(cancellation)
            })
        })
        .transpose()?;
    Ok((source, target))
}

pub(super) fn export_translation_document_with_cancellation(
    path: &Path,
    snapshot: &ManualProjectSnapshot,
    cancellation: &CooperativeCancellation,
) -> Result<usize, ManualDocumentError> {
    ensure_manual_document_running(cancellation)?;
    let (language, module) = snapshot
        .language
        .as_ref()
        .expect("Translation export 必须读取项目语言和当前语言配置");
    let mut encoded = serde_json::to_string(&TranslationExportHeader {
        source_language: language.source().as_str(),
        target_language: language.target().as_str(),
    })
    .expect("语言标签必须可编码");
    encoded.push('\n');
    let mut group_ids = HashMap::new();
    for entry in snapshot.index.entries() {
        ensure_manual_document_running(cancellation)?;
        let group_key = match &entry.locator {
            ManualTranslationLocator::Generic { group_id, .. } => ("generic", group_id.as_str()),
            ManualTranslationLocator::RpgMaker { group_location, .. } => {
                ("rpg_maker", group_location.as_str())
            }
        };
        let group = *group_ids.entry(group_key).or_insert(entry.id.as_str());
        let status = match (
            entry.current_translation.as_deref(),
            entry.origin,
            entry.rejected.as_ref(),
        ) {
            (Some(translation), Some(origin), _) => TranslationExportStatus::Current {
                translation,
                origin: origin.as_str(),
            },
            (None, None, Some(rejected)) => TranslationExportStatus::Rejected {
                translation: (),
                origin: rejected.origin.as_str(),
                rejected_candidate_json: &rejected.candidate_json,
            },
            (None, None, None) => TranslationExportStatus::Pending {
                translation: (),
                origin: "none",
            },
            _ => unreachable!("Manual 快照的译文、来源和 Rejected 状态必须一致"),
        };
        let (owner, rule_number) = match entry.rpg_maker_owner {
            Some(ManualRpgMakerOwner::Builtin) => (Some("builtin"), None),
            Some(ManualRpgMakerOwner::Rules { rule_number }) => (Some("rules"), Some(rule_number)),
            None => (None, None),
        };
        let (source, target) =
            review_projection(&snapshot.placeholders, entry, path, cancellation)?;
        let reviews = target
            .as_ref()
            .map(|target| {
                translation_qa_reviews(
                    module.as_ref(),
                    language.target(),
                    &source,
                    target,
                    &mut || {
                        ensure_manual_document_running(cancellation)
                            .map_err(|_| LanguageOperationCancelled)
                    },
                )
                .map_err(|LanguageOperationCancelled| ManualDocumentError::Cancelled)
            })
            .transpose()?
            .unwrap_or_default();
        let natural_source = natural_projection(&source, &entry.source);
        let natural_translation = target
            .as_ref()
            .zip(entry.current_translation.as_ref())
            .and_then(|(target, original)| natural_projection(target, original));
        encoded.push_str(
            &serde_json::to_string(&TranslationExportRecord {
                manual_id: &entry.id,
                group,
                source: &entry.source,
                status,
                kind: entry.kind,
                owner,
                rule_number,
                reviews,
                natural_source,
                natural_translation,
            })
            .expect("已校验的 Translation export 记录必须可编码"),
        );
        encoded.push('\n');
    }
    ensure_manual_document_running(cancellation)?;
    atomic_replace(path, encoded.as_bytes(), cancellation)?;
    Ok(snapshot.index.entries().len())
}
