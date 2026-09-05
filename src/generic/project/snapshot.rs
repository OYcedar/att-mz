//! Generic 当前来源快照的自然顺序读取、Extract 对账与整体替换。

use super::error::{
    GenericProjectError, invalid_database, project_optional_safe_identifier, sqlite_operation_error,
};
use super::{
    FINGERPRINT_CANCELLATION_CHECK_BYTES, GenericProject, GenericStoredFile, GenericStoredGroup,
    GenericStoredRejectedTranslation, GenericStoredSnapshot, GenericStoredTranslation,
    GenericStoredUnit, ReconciledSnapshot, bytes_equal_with_cancellation,
    clone_optional_sqlite_blob_column_with_cancellation,
    clone_optional_sqlite_text_column_with_cancellation,
    clone_sqlite_blob_column_with_cancellation, clone_sqlite_text_column_with_cancellation,
    clone_text_with_cancellation, decode_path_with_cancellation, encode_path,
    ensure_generic_operation_not_cancelled, from_i64, read_fingerprint, to_i64,
};
use crate::diagnostic::{GenericProjectDatabaseProblem, SafePath};
use crate::execution::CooperativeCancellation;
use crate::fingerprint::{Sha256Fingerprint, Sha256FramedHasher};
use crate::generic::identity::CancellableTextMap;
use crate::generic::jsonl::{GenericInputSnapshot, scan_input_tree_with_cancellation};
use crate::translation::TranslationOrigin;
use crate::translation::candidate_validation::ProvenInvariantViolation;
use rusqlite::{Connection, Transaction, params};
use std::collections::HashMap;

pub(super) const LOAD_UNITS_NATURAL_SQL: &str = "
    SELECT u.group_id, u.unit_id, u.ordinal, u.source_text,
           u.translation, u.translation_state,
           manual.translation_json, manual.applicability_fingerprint,
           rejected.readable_id, rejected.origin, rejected.source_json,
           rejected.candidate_json, rejected.translation_shape,
           rejected.group_context, rejected.violation_json, rejected.planning_state
    FROM main.generic_file AS f
    CROSS JOIN main.generic_group AS g
    CROSS JOIN main.generic_unit AS u
    LEFT JOIN main.generic_manual_translation AS manual
      ON manual.group_id = u.group_id
     AND manual.unit_id = u.unit_id
    LEFT JOIN main.generic_rejected_translation AS rejected
      ON rejected.group_id = u.group_id
     AND rejected.unit_id = u.unit_id
    WHERE g.relative_path = f.relative_path
      AND u.group_id = g.group_id
    ORDER BY f.ordinal, g.ordinal, u.ordinal
";

pub(super) fn reconcile_snapshot(
    previous: &GenericStoredSnapshot,
    scanned: &GenericInputSnapshot,
    cancellation: &CooperativeCancellation,
) -> Result<ReconciledSnapshot, GenericProjectError> {
    let mut previous_groups = HashMap::<Sha256Fingerprint, Vec<&GenericStoredGroup>>::new();
    let mut previous_translation_count = 0;
    for file in &previous.files {
        ensure_generic_operation_not_cancelled(cancellation)?;
        for group in &file.groups {
            ensure_generic_operation_not_cancelled(cancellation)?;
            let fingerprint =
                lookup_text_fingerprint_with_cancellation(group.id.as_str(), cancellation)?;
            previous_groups.entry(fingerprint).or_default().push(group);
            for unit in &group.units {
                ensure_generic_operation_not_cancelled(cancellation)?;
                previous_translation_count += usize::from(unit.translation.is_some());
            }
        }
    }

    let mut preserved_translations = 0;
    let mut files = Vec::with_capacity(scanned.files().len());
    for (file_ordinal, file) in scanned.files().iter().enumerate() {
        ensure_generic_operation_not_cancelled(cancellation)?;
        let mut groups = Vec::with_capacity(file.groups().len());
        for (group_ordinal, group) in file.groups().iter().enumerate() {
            ensure_generic_operation_not_cancelled(cancellation)?;
            let context_fingerprint = group_context_fingerprint(
                group.kind(),
                group.units().iter().map(|unit| unit.text()),
                Some(cancellation),
            )?;
            let previous_group =
                find_previous_group_with_cancellation(&previous_groups, group.id(), cancellation)?;
            let mut previous_units = HashMap::<Sha256Fingerprint, Vec<&GenericStoredUnit>>::new();
            if let Some(previous_group) = previous_group {
                for previous_unit in &previous_group.units {
                    ensure_generic_operation_not_cancelled(cancellation)?;
                    let fingerprint = lookup_text_fingerprint_with_cancellation(
                        previous_unit.id.as_str(),
                        cancellation,
                    )?;
                    previous_units
                        .entry(fingerprint)
                        .or_default()
                        .push(previous_unit);
                }
            }
            let mut units = Vec::with_capacity(group.units().len());
            for (unit_ordinal, unit) in group.units().iter().enumerate() {
                ensure_generic_operation_not_cancelled(cancellation)?;
                let previous_unit =
                    find_previous_unit_with_cancellation(&previous_units, unit.id(), cancellation)?;
                let translation = match previous_unit {
                    Some(old)
                        if bytes_equal_with_cancellation(
                            old.source_text.as_bytes(),
                            unit.text().as_bytes(),
                            cancellation,
                        )? =>
                    {
                        old.translation
                            .as_ref()
                            .map(|translation| {
                                clone_stored_translation_with_cancellation(
                                    translation,
                                    cancellation,
                                )
                            })
                            .transpose()?
                    }
                    _ => None,
                };
                let rejected = match previous_unit {
                    Some(old)
                        if bytes_equal_with_cancellation(
                            old.source_text.as_bytes(),
                            unit.text().as_bytes(),
                            cancellation,
                        )? =>
                    {
                        old.rejected
                            .as_ref()
                            .map(|rejected| {
                                clone_stored_rejected_translation_with_cancellation(
                                    rejected,
                                    cancellation,
                                )
                            })
                            .transpose()?
                    }
                    _ => None,
                };
                if translation.is_some() {
                    preserved_translations += 1;
                }
                units.push(GenericStoredUnit {
                    id: clone_text_with_cancellation(unit.id(), cancellation)?,
                    ordinal: unit_ordinal,
                    source_text: clone_text_with_cancellation(unit.text(), cancellation)?,
                    translation,
                    rejected,
                });
            }
            groups.push(GenericStoredGroup {
                id: clone_text_with_cancellation(group.id(), cancellation)?,
                ordinal: group_ordinal,
                kind: clone_text_with_cancellation(group.kind(), cancellation)?,
                context_fingerprint,
                units,
            });
        }
        files.push(GenericStoredFile {
            relative_path: file.relative_path().to_path_buf(),
            ordinal: file_ordinal,
            groups,
        });
    }

    Ok(ReconciledSnapshot {
        files,
        preserved_translations,
        cleared_translations: previous_translation_count.saturating_sub(preserved_translations),
    })
}

fn clone_stored_translation_with_cancellation(
    translation: &GenericStoredTranslation,
    cancellation: &CooperativeCancellation,
) -> Result<GenericStoredTranslation, GenericProjectError> {
    Ok(GenericStoredTranslation {
        translation: clone_text_with_cancellation(&translation.translation, cancellation)?,
        origin: translation.origin,
        state_fingerprint: translation.state_fingerprint,
    })
}

fn clone_stored_rejected_translation_with_cancellation(
    rejected: &GenericStoredRejectedTranslation,
    cancellation: &CooperativeCancellation,
) -> Result<GenericStoredRejectedTranslation, GenericProjectError> {
    let mut source = Vec::with_capacity(rejected.source.len());
    for line in &rejected.source {
        source.push(clone_text_with_cancellation(line, cancellation)?);
    }
    let translation = rejected
        .translation
        .as_ref()
        .map(|lines| {
            let mut cloned = Vec::with_capacity(lines.len());
            for line in lines {
                cloned.push(clone_text_with_cancellation(line, cancellation)?);
            }
            Ok::<_, GenericProjectError>(cloned)
        })
        .transpose()?;
    Ok(GenericStoredRejectedTranslation {
        readable_id: clone_text_with_cancellation(&rejected.readable_id, cancellation)?,
        origin: rejected.origin,
        source,
        candidate_json: clone_text_with_cancellation(&rejected.candidate_json, cancellation)?,
        translation,
        group_context: rejected.group_context,
        violation: rejected.violation.clone(),
        planning_state: rejected.planning_state,
    })
}

fn find_previous_unit_with_cancellation<'a>(
    previous_units: &HashMap<Sha256Fingerprint, Vec<&'a GenericStoredUnit>>,
    unit_id: &str,
    cancellation: &CooperativeCancellation,
) -> Result<Option<&'a GenericStoredUnit>, GenericProjectError> {
    let fingerprint = lookup_text_fingerprint_with_cancellation(unit_id, cancellation)?;
    let Some(candidates) = previous_units.get(&fingerprint) else {
        return Ok(None);
    };
    for candidate in candidates {
        if bytes_equal_with_cancellation(candidate.id.as_bytes(), unit_id.as_bytes(), cancellation)?
        {
            return Ok(Some(*candidate));
        }
    }
    Ok(None)
}

fn find_previous_group_with_cancellation<'a>(
    previous_groups: &HashMap<Sha256Fingerprint, Vec<&'a GenericStoredGroup>>,
    group_id: &str,
    cancellation: &CooperativeCancellation,
) -> Result<Option<&'a GenericStoredGroup>, GenericProjectError> {
    let fingerprint = lookup_text_fingerprint_with_cancellation(group_id, cancellation)?;
    let Some(candidates) = previous_groups.get(&fingerprint) else {
        return Ok(None);
    };
    for candidate in candidates {
        if bytes_equal_with_cancellation(
            candidate.id.as_bytes(),
            group_id.as_bytes(),
            cancellation,
        )? {
            return Ok(Some(*candidate));
        }
    }
    Ok(None)
}

fn lookup_text_fingerprint_with_cancellation(
    value: &str,
    cancellation: &CooperativeCancellation,
) -> Result<Sha256Fingerprint, GenericProjectError> {
    let mut hasher = Sha256FramedHasher::new(b"att.generic.lookup-text");
    hasher.try_frame_chunks(
        1,
        value.as_bytes(),
        FINGERPRINT_CANCELLATION_CHECK_BYTES,
        || ensure_generic_operation_not_cancelled(cancellation),
    )?;
    Ok(hasher.finish())
}

fn group_context_fingerprint<'a>(
    kind: &str,
    texts: impl IntoIterator<Item = &'a str>,
    cancellation: Option<&CooperativeCancellation>,
) -> Result<Sha256Fingerprint, GenericProjectError> {
    let mut hasher = Sha256FramedHasher::new(b"att.generic.group-context");
    frame_group_context_bytes(&mut hasher, 1, kind.as_bytes(), cancellation)?;
    for text in texts {
        frame_group_context_bytes(&mut hasher, 2, text.as_bytes(), cancellation)?;
    }
    Ok(hasher.finish())
}

fn frame_group_context_bytes(
    hasher: &mut Sha256FramedHasher,
    tag: u8,
    bytes: &[u8],
    cancellation: Option<&CooperativeCancellation>,
) -> Result<(), GenericProjectError> {
    match cancellation {
        Some(cancellation) => {
            hasher.try_frame_chunks(tag, bytes, FINGERPRINT_CANCELLATION_CHECK_BYTES, || {
                ensure_generic_operation_not_cancelled(cancellation)
            })?;
        }
        None => {
            hasher.frame(tag, bytes);
        }
    }
    Ok(())
}

pub(super) fn scan_current_input(
    stored: &GenericStoredSnapshot,
    cancellation: &CooperativeCancellation,
) -> Result<GenericInputSnapshot, GenericProjectError> {
    let live = scan_input_tree_with_cancellation(stored.project.source_root(), cancellation)?;
    if Some(live.raw_fingerprint()) != stored.project.extracted_raw_fingerprint
        || Some(live.asset_fingerprint()) != stored.project.extracted_asset_fingerprint
    {
        return Err(GenericProjectError::ExtractRequired);
    }
    validate_stored_assets_match_live(stored, &live, Some(cancellation))?;
    Ok(live)
}

/// 候选已经建立后再次完整扫描外部输入，并同时比较原始与资产指纹。
///
/// 调用方在同一项目排他租约内持有首次完整验证得到的项目事实，因此这里不再打开
/// 64 MB 级项目数据库或重复执行 SQLite 完整性检查。
#[cfg(test)]
pub(crate) fn ensure_input_fingerprints_current(
    project: &GenericProject,
) -> Result<(), GenericProjectError> {
    ensure_input_fingerprints_current_with_cancellation(
        project,
        &CooperativeCancellation::default(),
    )
}

pub(crate) fn ensure_input_fingerprints_current_with_cancellation(
    project: &GenericProject,
    cancellation: &CooperativeCancellation,
) -> Result<(), GenericProjectError> {
    if cancellation.is_requested() {
        return Err(GenericProjectError::Cancelled);
    }
    let expected_raw_fingerprint = project
        .extracted_raw_fingerprint
        .ok_or(GenericProjectError::ExtractRequired)?;
    let expected_asset_fingerprint = project
        .extracted_asset_fingerprint
        .ok_or(GenericProjectError::ExtractRequired)?;
    let live = scan_input_tree_with_cancellation(project.source_root(), cancellation)?;
    if live.raw_fingerprint() != expected_raw_fingerprint
        || live.asset_fingerprint() != expected_asset_fingerprint
    {
        return Err(GenericProjectError::ExtractRequired);
    }
    Ok(())
}

fn validate_stored_assets_match_live(
    stored: &GenericStoredSnapshot,
    live: &GenericInputSnapshot,
    cancellation: Option<&CooperativeCancellation>,
) -> Result<(), GenericProjectError> {
    let no_cancellation = CooperativeCancellation::default();
    let comparison_cancellation = cancellation.unwrap_or(&no_cancellation);
    if stored.files.len() != live.files().len() {
        return Err(invalid_database(
            GenericProjectDatabaseProblem::SnapshotFileCount {
                stored: stored.files.len(),
                extracted: live.files().len(),
            },
        ));
    }
    for (file_ordinal, (stored_file, live_file)) in
        stored.files.iter().zip(live.files()).enumerate()
    {
        if let Some(cancellation) = cancellation {
            ensure_generic_operation_not_cancelled(cancellation)?;
        }
        if stored_file.ordinal != file_ordinal
            || stored_file.relative_path != live_file.relative_path()
            || stored_file.groups.len() != live_file.groups().len()
        {
            return Err(invalid_database(
                GenericProjectDatabaseProblem::SnapshotFileMismatch {
                    relative_path: SafePath::new(live_file.relative_path()),
                },
            ));
        }
        for (group_ordinal, (stored_group, live_group)) in stored_file
            .groups
            .iter()
            .zip(live_file.groups())
            .enumerate()
        {
            if let Some(cancellation) = cancellation {
                ensure_generic_operation_not_cancelled(cancellation)?;
            }
            let expected_context = group_context_fingerprint(
                live_group.kind(),
                live_group.units().iter().map(|unit| unit.text()),
                cancellation,
            )?;
            let group_id_matches = bytes_equal_with_cancellation(
                stored_group.id.as_bytes(),
                live_group.id().as_bytes(),
                comparison_cancellation,
            )?;
            let group_kind_matches = bytes_equal_with_cancellation(
                stored_group.kind.as_bytes(),
                live_group.kind().as_bytes(),
                comparison_cancellation,
            )?;
            if stored_group.ordinal != group_ordinal
                || !group_id_matches
                || !group_kind_matches
                || stored_group.context_fingerprint != expected_context
                || stored_group.units.len() != live_group.units().len()
            {
                return Err(invalid_database(
                    GenericProjectDatabaseProblem::SnapshotGroupMismatch {
                        relative_path: SafePath::new(live_file.relative_path()),
                        group_id: project_optional_safe_identifier(live_group.id()),
                    },
                ));
            }
            for (unit_ordinal, (stored_unit, live_unit)) in stored_group
                .units
                .iter()
                .zip(live_group.units())
                .enumerate()
            {
                if let Some(cancellation) = cancellation {
                    ensure_generic_operation_not_cancelled(cancellation)?;
                }
                let unit_id_matches = bytes_equal_with_cancellation(
                    stored_unit.id.as_bytes(),
                    live_unit.id().as_bytes(),
                    comparison_cancellation,
                )?;
                let source_text_matches = bytes_equal_with_cancellation(
                    stored_unit.source_text.as_bytes(),
                    live_unit.text().as_bytes(),
                    comparison_cancellation,
                )?;
                if stored_unit.ordinal != unit_ordinal || !unit_id_matches || !source_text_matches {
                    return Err(invalid_database(
                        GenericProjectDatabaseProblem::SnapshotUnitMismatch {
                            relative_path: SafePath::new(live_file.relative_path()),
                            group_id: project_optional_safe_identifier(live_group.id()),
                            unit_id: project_optional_safe_identifier(live_unit.id()),
                        },
                    ));
                }
            }
        }
    }
    Ok(())
}

pub(super) fn replace_snapshot(
    transaction: &Transaction<'_>,
    scanned: &GenericInputSnapshot,
    files: &[GenericStoredFile],
    cancellation: &CooperativeCancellation,
) -> Result<(), GenericProjectError> {
    transaction
        .execute("DELETE FROM generic_file", [])
        .map_err(|source| GenericProjectError::Sqlite {
            operation: "清理上一份 Generic Extract 快照",
            source,
        })?;

    {
        let mut file_statement = transaction
            .prepare_cached("INSERT INTO generic_file (relative_path, ordinal) VALUES (?1, ?2)")
            .map_err(|source| GenericProjectError::Sqlite {
                operation: "写入 Generic 文件",
                source,
            })?;
        let mut group_statement = transaction
            .prepare_cached(
                "INSERT INTO generic_group (
                         group_id, relative_path, ordinal, kind, context_fingerprint
                     ) VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .map_err(|source| GenericProjectError::Sqlite {
                operation: "写入 Generic Group",
                source,
            })?;
        let mut unit_statement = transaction
            .prepare_cached(
                "INSERT INTO generic_unit (
                         group_id, unit_id, ordinal, source_text,
                             translation, translation_state
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )
            .map_err(|source| GenericProjectError::Sqlite {
                operation: "写入 Generic Unit",
                source,
            })?;
        let mut rejected_statement = transaction
            .prepare_cached(
                "INSERT INTO generic_rejected_translation (
                     group_id, unit_id, readable_id, origin, source_json, candidate_json,
                     translation_shape, group_context, violation_json, planning_state
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'free', ?7, ?8, ?9)",
            )
            .map_err(|source| GenericProjectError::Sqlite {
                operation: "写入 Generic 被拒候选",
                source,
            })?;

        for file in files {
            ensure_generic_operation_not_cancelled(cancellation)?;
            let relative_path = encode_path(&file.relative_path);
            file_statement
                .execute(params![&relative_path, to_i64(file.ordinal)?])
                .map_err(|source| GenericProjectError::Sqlite {
                    operation: "写入 Generic 文件",
                    source,
                })?;
            for group in &file.groups {
                ensure_generic_operation_not_cancelled(cancellation)?;
                group_statement
                    .execute(params![
                        group.id,
                        &relative_path,
                        to_i64(group.ordinal)?,
                        group.kind,
                        group.context_fingerprint.as_bytes().as_slice()
                    ])
                    .map_err(|source| GenericProjectError::Sqlite {
                        operation: "写入 Generic Group",
                        source,
                    })?;
                for unit in &group.units {
                    ensure_generic_operation_not_cancelled(cancellation)?;
                    // 人工译文由独立表持有，并按当前位置重新计算适用性。把加载时覆盖在
                    // Unit 上的人工正文写入自动列，会在文件移动等身份变化后把本应过期的
                    // 人工译文伪装成 Current 自动译文。
                    let (translation, state) = unit
                        .translation
                        .as_ref()
                        .filter(|translation| translation.origin == TranslationOrigin::Automatic)
                        .map_or((None, None), |translation| {
                            (
                                Some(translation.translation.as_str()),
                                Some(translation.state_fingerprint.as_bytes().as_slice()),
                            )
                        });
                    unit_statement
                        .execute(params![
                            group.id,
                            unit.id,
                            to_i64(unit.ordinal)?,
                            unit.source_text,
                            translation,
                            state
                        ])
                        .map_err(|source| GenericProjectError::Sqlite {
                            operation: "写入 Generic Unit",
                            source,
                        })?;
                    if let Some(rejected) = &unit.rejected {
                        rejected_statement
                            .execute(params![
                                group.id,
                                unit.id,
                                rejected.readable_id,
                                rejected.origin.storage_name(),
                                serde_json::to_string(&rejected.source)
                                    .expect("Generic 被拒候选原文必须可以编码"),
                                rejected.candidate_json,
                                rejected.group_context.as_bytes().as_slice(),
                                serde_json::to_string(&rejected.violation)
                                    .expect("Generic 被拒原因必须可以编码"),
                                rejected.planning_state.as_bytes().as_slice(),
                            ])
                            .map_err(|source| GenericProjectError::Sqlite {
                                operation: "写入 Generic 被拒候选",
                                source,
                            })?;
                    }
                }
            }
        }
    }
    ensure_generic_operation_not_cancelled(cancellation)?;
    transaction
        .execute(
            "UPDATE generic_project
             SET extracted_raw_fingerprint = ?1, extracted_asset_fingerprint = ?2
             WHERE singleton = 1",
            params![
                scanned.raw_fingerprint().as_bytes().as_slice(),
                scanned.asset_fingerprint().as_bytes().as_slice()
            ],
        )
        .map_err(|source| GenericProjectError::Sqlite {
            operation: "保存 Generic Extract 指纹",
            source,
        })?;
    Ok(())
}

pub(super) fn load_snapshot_rows(
    connection: &Connection,
    project: &GenericProject,
    cancellation: &CooperativeCancellation,
) -> Result<GenericStoredSnapshot, GenericProjectError> {
    let mut files = Vec::new();
    let mut file_indexes = HashMap::new();
    let mut file_statement = connection
        .prepare(
            "SELECT relative_path, ordinal
             FROM main.generic_file ORDER BY ordinal",
        )
        .map_err(|source| sqlite_operation_error("准备读取 Generic 文件", source))?;
    let mut file_rows = file_statement
        .query([])
        .map_err(|source| sqlite_operation_error("读取 Generic 文件", source))?;
    while let Some(row) = file_rows
        .next()
        .map_err(|source| sqlite_operation_error("解码 Generic 文件记录", source))?
    {
        ensure_generic_operation_not_cancelled(cancellation)?;
        let path_bytes = clone_sqlite_blob_column_with_cancellation(
            row,
            0,
            "解码 Generic 文件记录",
            cancellation,
        )?;
        let ordinal = row
            .get::<_, i64>(1)
            .map_err(|source| GenericProjectError::Sqlite {
                operation: "解码 Generic 文件记录",
                source,
            })?;
        let relative_path = decode_path_with_cancellation(&path_bytes, cancellation)?;
        ensure_generic_operation_not_cancelled(cancellation)?;
        let file_index = files.len();
        file_indexes.insert(path_bytes, file_index);
        files.push(GenericStoredFile {
            relative_path,
            ordinal: from_i64(ordinal, "file.ordinal")?,
            groups: Vec::new(),
        });
    }
    drop(file_rows);
    drop(file_statement);

    let mut group_indexes = CancellableTextMap::with_capacity(files.len());
    let mut group_statement = connection
        .prepare(
            "SELECT g.relative_path, g.group_id, g.ordinal,
                    g.kind, g.context_fingerprint
             FROM main.generic_group AS g
             JOIN main.generic_file AS f
               ON f.relative_path = g.relative_path
             ORDER BY f.ordinal, g.ordinal",
        )
        .map_err(|source| sqlite_operation_error("准备读取 Generic Group", source))?;
    let mut group_rows = group_statement
        .query([])
        .map_err(|source| sqlite_operation_error("读取 Generic Group", source))?;
    while let Some(row) = group_rows
        .next()
        .map_err(|source| sqlite_operation_error("解码 Generic Group", source))?
    {
        ensure_generic_operation_not_cancelled(cancellation)?;
        let path_bytes =
            clone_sqlite_blob_column_with_cancellation(row, 0, "解码 Generic Group", cancellation)?;
        let group_id =
            clone_sqlite_text_column_with_cancellation(row, 1, "解码 Generic Group", cancellation)?;
        let group_ordinal = row
            .get::<_, i64>(2)
            .map_err(|source| GenericProjectError::Sqlite {
                operation: "解码 Generic Group",
                source,
            })?;
        let kind =
            clone_sqlite_text_column_with_cancellation(row, 3, "解码 Generic Group", cancellation)?;
        let context =
            clone_sqlite_blob_column_with_cancellation(row, 4, "解码 Generic Group", cancellation)?;
        ensure_generic_operation_not_cancelled(cancellation)?;
        let Some(&file_index) = file_indexes.get(&path_bytes) else {
            return Err(invalid_database(
                GenericProjectDatabaseProblem::GroupReferencesMissingFile {
                    group_id: project_optional_safe_identifier(&group_id),
                },
            ));
        };
        let group_index = files[file_index].groups.len();
        let group_index_key = clone_text_with_cancellation(&group_id, cancellation)?;
        let previous = group_indexes.insert_with_cancellation(
            group_index_key,
            (file_index, group_index),
            || ensure_generic_operation_not_cancelled(cancellation),
        )?;
        debug_assert!(previous.is_none());
        files[file_index].groups.push(GenericStoredGroup {
            id: group_id,
            ordinal: from_i64(group_ordinal, "group.ordinal")?,
            kind,
            context_fingerprint: read_fingerprint(context, "context_fingerprint")?,
            units: Vec::new(),
        });
    }
    drop(group_rows);
    drop(group_statement);

    let mut unit_statement = connection
        .prepare(LOAD_UNITS_NATURAL_SQL)
        .map_err(|source| sqlite_operation_error("准备读取 Generic Unit", source))?;
    let mut unit_rows = unit_statement
        .query([])
        .map_err(|source| sqlite_operation_error("读取 Generic Unit", source))?;
    while let Some(row) = unit_rows
        .next()
        .map_err(|source| sqlite_operation_error("解码 Generic Unit", source))?
    {
        ensure_generic_operation_not_cancelled(cancellation)?;
        let group_id =
            clone_sqlite_text_column_with_cancellation(row, 0, "解码 Generic Unit", cancellation)?;
        let unit_id =
            clone_sqlite_text_column_with_cancellation(row, 1, "解码 Generic Unit", cancellation)?;
        let unit_ordinal = row
            .get::<_, i64>(2)
            .map_err(|source| GenericProjectError::Sqlite {
                operation: "解码 Generic Unit",
                source,
            })?;
        let source_text =
            clone_sqlite_text_column_with_cancellation(row, 3, "解码 Generic Unit", cancellation)?;
        let translation = clone_optional_sqlite_text_column_with_cancellation(
            row,
            4,
            "解码 Generic Unit",
            cancellation,
        )?;
        let state = clone_optional_sqlite_blob_column_with_cancellation(
            row,
            5,
            "解码 Generic Unit",
            cancellation,
        )?;
        let automatic_translation = match (translation, state) {
            (None, None) => None,
            (Some(translation), Some(state)) => Some(GenericStoredTranslation {
                translation,
                origin: TranslationOrigin::Automatic,
                state_fingerprint: read_fingerprint(state, "translation_state")?,
            }),
            _ => {
                return Err(invalid_database(
                    GenericProjectDatabaseProblem::IncompleteTranslationState {
                        group_id: project_optional_safe_identifier(&group_id),
                        unit_id: project_optional_safe_identifier(&unit_id),
                    },
                ));
            }
        };
        let manual_translation_json = clone_optional_sqlite_text_column_with_cancellation(
            row,
            6,
            "解码 Generic 人工译文",
            cancellation,
        )?;
        let manual_state = clone_optional_sqlite_blob_column_with_cancellation(
            row,
            7,
            "解码 Generic 人工译文",
            cancellation,
        )?;
        let rejected_readable_id = clone_optional_sqlite_text_column_with_cancellation(
            row,
            8,
            "解码 Generic 被拒候选",
            cancellation,
        )?;
        let rejected_origin = clone_optional_sqlite_text_column_with_cancellation(
            row,
            9,
            "解码 Generic 被拒候选",
            cancellation,
        )?;
        let rejected_source_json = clone_optional_sqlite_text_column_with_cancellation(
            row,
            10,
            "解码 Generic 被拒候选",
            cancellation,
        )?;
        let rejected_candidate_json = clone_optional_sqlite_text_column_with_cancellation(
            row,
            11,
            "解码 Generic 被拒候选",
            cancellation,
        )?;
        let rejected_shape = clone_optional_sqlite_text_column_with_cancellation(
            row,
            12,
            "解码 Generic 被拒候选",
            cancellation,
        )?;
        let rejected_group_context = clone_optional_sqlite_blob_column_with_cancellation(
            row,
            13,
            "解码 Generic 被拒候选",
            cancellation,
        )?;
        let rejected_violation_json = clone_optional_sqlite_text_column_with_cancellation(
            row,
            14,
            "解码 Generic 被拒候选",
            cancellation,
        )?;
        let rejected_planning_state = clone_optional_sqlite_blob_column_with_cancellation(
            row,
            15,
            "解码 Generic 被拒候选",
            cancellation,
        )?;
        ensure_generic_operation_not_cancelled(cancellation)?;
        let Some(&(file_index, group_index)) = group_indexes
            .get_with_cancellation(&group_id, || {
                ensure_generic_operation_not_cancelled(cancellation)
            })?
        else {
            return Err(invalid_database(
                GenericProjectDatabaseProblem::UnitReferencesMissingGroup {
                    group_id: project_optional_safe_identifier(&group_id),
                    unit_id: project_optional_safe_identifier(&unit_id),
                },
            ));
        };
        let group = &files[file_index].groups[group_index];
        let source_lines = source_text
            .split('\n')
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let readable_path = files[file_index]
            .relative_path
            .to_string_lossy()
            .replace('\\', "/");
        let expected_manual_state = crate::manual::generic_manual_applicability(
            &group_id,
            &unit_id,
            &readable_path,
            group.kind(),
            project.language_pair().source().as_str(),
            project.language_pair().target().as_str(),
            &source_lines,
        );
        let manual_translation = match (manual_translation_json, manual_state) {
            (None, None) => None,
            (Some(translation_json), Some(state)) => {
                let state = read_fingerprint(state, "applicability_fingerprint")?;
                if state != expected_manual_state {
                    None
                } else {
                    let lines =
                        serde_json::from_str::<Vec<String>>(&translation_json).map_err(|_| {
                            invalid_database(
                                GenericProjectDatabaseProblem::ManualTranslationStateFailure,
                            )
                        })?;
                    if lines.is_empty()
                        || lines.iter().any(|line| {
                            line.chars()
                                .any(|character| matches!(character, '\r' | '\n' | '\0'))
                        })
                    {
                        return Err(invalid_database(
                            GenericProjectDatabaseProblem::ManualTranslationStateFailure,
                        ));
                    }
                    Some(GenericStoredTranslation {
                        translation: lines.join("\n"),
                        origin: TranslationOrigin::Manual,
                        state_fingerprint: expected_manual_state,
                    })
                }
            }
            _ => {
                return Err(invalid_database(
                    GenericProjectDatabaseProblem::ManualTranslationStateFailure,
                ));
            }
        };
        let translation = manual_translation.or(automatic_translation);
        let rejected = match (
            rejected_readable_id,
            rejected_origin,
            rejected_source_json,
            rejected_candidate_json,
            rejected_shape,
            rejected_group_context,
            rejected_violation_json,
            rejected_planning_state,
        ) {
            (None, None, None, None, None, None, None, None) => None,
            (
                Some(readable_id),
                Some(origin),
                Some(source_json),
                Some(candidate_json),
                Some(shape),
                Some(group_context),
                Some(violation_json),
                Some(planning_state),
            ) if shape == "free" => {
                let origin = TranslationOrigin::from_storage_name(&origin).ok_or_else(|| {
                    invalid_database(GenericProjectDatabaseProblem::ManualTranslationStateFailure)
                })?;
                let source = serde_json::from_str::<Vec<String>>(&source_json).map_err(|_| {
                    invalid_database(GenericProjectDatabaseProblem::ManualTranslationStateFailure)
                })?;
                let translation = serde_json::from_str::<Vec<String>>(&candidate_json)
                    .ok()
                    .filter(|translation| !translation.is_empty());
                let violation = serde_json::from_str::<ProvenInvariantViolation>(&violation_json)
                    .map_err(|_| {
                    invalid_database(GenericProjectDatabaseProblem::ManualTranslationStateFailure)
                })?;
                if source.is_empty() {
                    return Err(invalid_database(
                        GenericProjectDatabaseProblem::ManualTranslationStateFailure,
                    ));
                }
                Some(GenericStoredRejectedTranslation {
                    readable_id,
                    origin,
                    source,
                    candidate_json,
                    translation,
                    group_context: read_fingerprint(group_context, "group_context")?,
                    violation,
                    planning_state: read_fingerprint(planning_state, "planning_state")?,
                })
            }
            _ => {
                return Err(invalid_database(
                    GenericProjectDatabaseProblem::ManualTranslationStateFailure,
                ));
            }
        };
        files[file_index].groups[group_index]
            .units
            .push(GenericStoredUnit {
                id: unit_id,
                ordinal: from_i64(unit_ordinal, "unit.ordinal")?,
                source_text,
                translation,
                rejected,
            });
    }
    drop(unit_rows);
    ensure_generic_operation_not_cancelled(cancellation)?;
    Ok(GenericStoredSnapshot {
        project: project.clone(),
        files,
    })
}
