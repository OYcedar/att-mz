//! Lua 托管翻译集合的受信模型与项目数据库持久化边界。
//!
//! Lua Host 只负责把外部声明转换成这里的受信模型；来源身份、快照替换、译文继承、
//! 一致读取和 checkpoint CAS 均由本模块拥有。模型不包含模型请求、任务装箱或写回
//! 规则，避免把特定执行流程固化到跨阶段持久资产中。

mod store;

use std::collections::{HashMap, HashSet};

use crate::fingerprint::{Sha256Fingerprint, Sha256FramedHasher};
use crate::managed_translation::validate_translation;
pub(crate) use crate::managed_translation::{
    ManagedTranslationCollection, ManagedTranslationContent, ManagedTranslationMetadata,
    ManagedTranslationModelError, ManagedTranslationPair, ManagedTranslationShape,
    ManagedTranslationUnit,
};
use crate::rpg_maker::project_database::SourceSnapshotFingerprint;

pub(crate) use store::{
    ManagedTranslationCheckpointError, ManagedTranslationCheckpointOutcome,
    ManagedTranslationRepository, ManagedTranslationSnapshotMutation,
    ManagedTranslationSqliteRepository,
};

/// Lua 托管翻译快照的稳定内容身份。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct ManagedTranslationManifestFingerprint(Sha256Fingerprint);

impl ManagedTranslationManifestFingerprint {
    pub(crate) const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(Sha256Fingerprint::from_bytes(bytes))
    }

    pub(crate) const fn as_bytes(&self) -> &[u8; 32] {
        self.0.as_bytes()
    }
}

/// Extract 建立、Translate 更新、WriteBack 读取的完整冻结快照。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManagedTranslationSnapshot {
    source_snapshot_fingerprint: SourceSnapshotFingerprint,
    manifest_fingerprint: ManagedTranslationManifestFingerprint,
    collections: Vec<ManagedTranslationCollection>,
}

impl ManagedTranslationSnapshot {
    pub(crate) fn new(
        source_snapshot_fingerprint: SourceSnapshotFingerprint,
        collections: Vec<ManagedTranslationCollection>,
    ) -> Result<Self, ManagedTranslationModelError> {
        let mut names = HashSet::with_capacity(collections.len());
        for collection in &collections {
            if !names.insert(collection.name().to_owned()) {
                return Err(ManagedTranslationModelError::DuplicateCollectionName(
                    collection.name().to_owned(),
                ));
            }
        }
        let manifest_fingerprint = manifest_fingerprint(&collections);
        Ok(Self {
            source_snapshot_fingerprint,
            manifest_fingerprint,
            collections,
        })
    }

    pub(crate) const fn source_snapshot_fingerprint(&self) -> SourceSnapshotFingerprint {
        self.source_snapshot_fingerprint
    }

    pub(crate) const fn manifest_fingerprint(&self) -> ManagedTranslationManifestFingerprint {
        self.manifest_fingerprint
    }

    pub(crate) fn collections(&self) -> &[ManagedTranslationCollection] {
        &self.collections
    }

    pub(crate) fn collection(&self, name: &str) -> Option<&ManagedTranslationCollection> {
        self.collections
            .iter()
            .find(|collection| collection.name == name)
    }

    fn from_stored(
        source_snapshot_fingerprint: SourceSnapshotFingerprint,
        stored_manifest_fingerprint: ManagedTranslationManifestFingerprint,
        collections: Vec<ManagedTranslationCollection>,
    ) -> Result<Self, ManagedTranslationModelError> {
        let snapshot = Self::new(source_snapshot_fingerprint, collections)?;
        if snapshot.manifest_fingerprint != stored_manifest_fingerprint {
            return Err(ManagedTranslationModelError::ManifestFingerprintMismatch {
                stored: stored_manifest_fingerprint.0,
                calculated: snapshot.manifest_fingerprint.0,
            });
        }
        Ok(snapshot)
    }
}

impl crate::managed_translation::ManagedTranslationSnapshotView for ManagedTranslationSnapshot {
    fn collections(&self) -> &[ManagedTranslationCollection] {
        self.collections()
    }
}

/// 一个 frozen snapshot 上的 checkpoint 替换项。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManagedTranslationReplacement {
    collection: String,
    key: String,
    replacement: Option<ManagedTranslationPair>,
}

impl ManagedTranslationReplacement {
    pub(crate) fn new(
        collection: impl Into<String>,
        key: impl Into<String>,
        replacement: Option<ManagedTranslationPair>,
    ) -> Self {
        Self {
            collection: collection.into(),
            key: key.into(),
            replacement,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ManagedTranslationCheckpointAction {
    Guard,
    Replace(Option<ManagedTranslationPair>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ManagedTranslationCheckpointWrite {
    collection: String,
    key: String,
    collection_order: usize,
    instruction: String,
    unit_order: usize,
    kind: String,
    shape: ManagedTranslationShape,
    original: ManagedTranslationContent,
    context: String,
    metadata: Option<ManagedTranslationMetadata>,
    expected: Option<ManagedTranslationPair>,
    action: ManagedTranslationCheckpointAction,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ManagedTranslationCheckpointCollection {
    name: String,
    order: usize,
    instruction: String,
}

/// 一个 TaskBlock 可在同一短事务中提交的全部已验收结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManagedTranslationCheckpoint {
    source_snapshot_fingerprint: SourceSnapshotFingerprint,
    manifest_fingerprint: ManagedTranslationManifestFingerprint,
    complete_guard: bool,
    collections: Vec<ManagedTranslationCheckpointCollection>,
    unit_count: usize,
    writes: Vec<ManagedTranslationCheckpointWrite>,
}

impl ManagedTranslationCheckpoint {
    pub(crate) fn new(
        snapshot: &ManagedTranslationSnapshot,
        replacements: Vec<ManagedTranslationReplacement>,
    ) -> Result<Self, ManagedTranslationModelError> {
        let mut identities = HashSet::with_capacity(replacements.len());
        let mut writes = Vec::with_capacity(replacements.len());
        for replacement in replacements {
            let identity = (replacement.collection.clone(), replacement.key.clone());
            if !identities.insert(identity.clone()) {
                return Err(ManagedTranslationModelError::DuplicateCheckpointIdentity {
                    collection: identity.0,
                    key: identity.1,
                });
            }
            let (collection_order, collection) = snapshot
                .collections
                .iter()
                .enumerate()
                .find(|(_, collection)| collection.name == replacement.collection)
                .ok_or_else(|| ManagedTranslationModelError::UnknownCheckpointIdentity {
                    collection: replacement.collection.clone(),
                    key: replacement.key.clone(),
                })?;
            let (unit_order, unit) = collection
                .units
                .iter()
                .enumerate()
                .find(|(_, unit)| unit.key == replacement.key)
                .ok_or_else(|| ManagedTranslationModelError::UnknownCheckpointIdentity {
                    collection: replacement.collection.clone(),
                    key: replacement.key.clone(),
                })?;
            if let Some(pair) = &replacement.replacement {
                validate_translation(unit.shape, &unit.original, pair.content())?;
            }
            writes.push(ManagedTranslationCheckpointWrite {
                collection: replacement.collection,
                key: replacement.key,
                collection_order,
                instruction: collection.instruction.clone(),
                unit_order,
                kind: unit.kind.clone(),
                shape: unit.shape,
                original: unit.original.clone(),
                context: unit.context.clone(),
                metadata: unit.metadata.clone(),
                expected: unit.translation.clone(),
                action: ManagedTranslationCheckpointAction::Replace(replacement.replacement),
            });
        }
        Ok(Self {
            source_snapshot_fingerprint: snapshot.source_snapshot_fingerprint,
            manifest_fingerprint: snapshot.manifest_fingerprint,
            complete_guard: false,
            collections: Vec::new(),
            unit_count: 0,
            writes,
        })
    }

    /// 建立覆盖完整读取依赖的预检 CAS，并只修改显式 replacement。
    ///
    /// 未修改的 unit 仍作为 guard 进入同一事务，防止 Current、冲突与去重决策基于
    /// 已经被并发写者改变的 translation/state 或声明语义。
    pub(crate) fn guarded(
        snapshot: &ManagedTranslationSnapshot,
        replacements: Vec<ManagedTranslationReplacement>,
    ) -> Result<Self, ManagedTranslationModelError> {
        let replacements = Self::new(snapshot, replacements)?;
        let mut replacements = replacements
            .writes
            .into_iter()
            .map(|write| ((write.collection.clone(), write.key.clone()), write))
            .collect::<HashMap<_, _>>();
        let unit_count = snapshot
            .collections
            .iter()
            .map(|collection| collection.units.len())
            .sum();
        let mut writes = Vec::with_capacity(unit_count);
        for (collection_order, collection) in snapshot.collections.iter().enumerate() {
            for (unit_order, unit) in collection.units.iter().enumerate() {
                if let Some(write) =
                    replacements.remove(&(collection.name.clone(), unit.key.clone()))
                {
                    writes.push(write);
                    continue;
                }
                writes.push(ManagedTranslationCheckpointWrite {
                    collection: collection.name.clone(),
                    key: unit.key.clone(),
                    collection_order,
                    instruction: collection.instruction.clone(),
                    unit_order,
                    kind: unit.kind.clone(),
                    shape: unit.shape,
                    original: unit.original.clone(),
                    context: unit.context.clone(),
                    metadata: unit.metadata.clone(),
                    expected: unit.translation.clone(),
                    action: ManagedTranslationCheckpointAction::Guard,
                });
            }
        }
        debug_assert!(replacements.is_empty(), "replacement 身份已由 new 完整验证");
        Ok(Self {
            source_snapshot_fingerprint: snapshot.source_snapshot_fingerprint,
            manifest_fingerprint: snapshot.manifest_fingerprint,
            complete_guard: true,
            collections: checkpoint_collections(snapshot),
            unit_count,
            writes,
        })
    }

    pub(crate) const fn source_snapshot_fingerprint(&self) -> SourceSnapshotFingerprint {
        self.source_snapshot_fingerprint
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.writes.is_empty() && !self.complete_guard
    }

    /// 在内存中投影本 checkpoint 成功提交后的完整权威快照。
    ///
    /// Translate 用它把数据库重读值与唯一预期状态做完整比较，既覆盖 manifest，
    /// 也覆盖故意不进入 manifest 的 translation/state 对。
    pub(crate) fn expected_snapshot(
        &self,
        snapshot: &ManagedTranslationSnapshot,
    ) -> Result<ManagedTranslationSnapshot, ManagedTranslationModelError> {
        if snapshot.source_snapshot_fingerprint != self.source_snapshot_fingerprint
            || snapshot.manifest_fingerprint != self.manifest_fingerprint
        {
            return Err(ManagedTranslationModelError::CheckpointSnapshotMismatch);
        }

        let mut expected = snapshot.clone();
        for write in &self.writes {
            let unit = expected
                .collections
                .iter_mut()
                .find(|collection| collection.name == write.collection)
                .and_then(|collection| {
                    collection
                        .units
                        .iter_mut()
                        .find(|unit| unit.key == write.key)
                })
                .ok_or_else(|| ManagedTranslationModelError::UnknownCheckpointIdentity {
                    collection: write.collection.clone(),
                    key: write.key.clone(),
                })?;
            if unit.translation != write.expected {
                return Err(
                    ManagedTranslationModelError::CheckpointExpectedTranslationMismatch {
                        collection: write.collection.clone(),
                        key: write.key.clone(),
                    },
                );
            }
            if let ManagedTranslationCheckpointAction::Replace(replacement) = &write.action {
                unit.translation = replacement.clone();
            }
        }
        Ok(expected)
    }
}

fn checkpoint_collections(
    snapshot: &ManagedTranslationSnapshot,
) -> Vec<ManagedTranslationCheckpointCollection> {
    snapshot
        .collections
        .iter()
        .enumerate()
        .map(
            |(order, collection)| ManagedTranslationCheckpointCollection {
                name: collection.name.clone(),
                order,
                instruction: collection.instruction.clone(),
            },
        )
        .collect()
}

fn manifest_fingerprint(
    collections: &[ManagedTranslationCollection],
) -> ManagedTranslationManifestFingerprint {
    let mut hasher = Sha256FramedHasher::new(b"att.rpg_maker.managed_translation.manifest");
    for (collection_order, collection) in collections.iter().enumerate() {
        let collection_order =
            u64::try_from(collection_order).expect("集合自然顺序必须可表示为 u64");
        hasher
            .frame(1, &collection_order.to_be_bytes())
            .frame(2, collection.name.as_bytes())
            .frame(3, collection.instruction.as_bytes());
        for (unit_order, unit) in collection.units.iter().enumerate() {
            let unit_order = u64::try_from(unit_order).expect("单元自然顺序必须可表示为 u64");
            hasher
                .frame(4, &unit_order.to_be_bytes())
                .frame(5, unit.key.as_bytes())
                .frame(6, unit.kind.as_bytes())
                .frame(7, unit.shape.storage_name().as_bytes())
                .frame(8, unit.original.canonical_json().as_bytes())
                .frame(9, unit.context.as_bytes());
            match &unit.metadata {
                None => {
                    hasher.frame(10, b"");
                }
                Some(metadata) => {
                    hasher
                        .frame(10, b"present")
                        .frame(11, metadata.canonical_json().as_bytes());
                }
            }
        }
        let unit_count = u64::try_from(collection.units.len()).expect("单元总数必须可表示为 u64");
        hasher.frame(12, &unit_count.to_be_bytes());
    }
    let collection_count = u64::try_from(collections.len()).expect("集合总数必须可表示为 u64");
    hasher.frame(13, &collection_count.to_be_bytes());
    ManagedTranslationManifestFingerprint(hasher.finish())
}

#[cfg(test)]
mod model_tests {
    use super::*;

    fn source() -> SourceSnapshotFingerprint {
        SourceSnapshotFingerprint::from_bytes([0x11; 32])
    }

    fn scalar_unit(key: &str) -> ManagedTranslationUnit {
        ManagedTranslationUnit::new(
            key,
            "plugin_parameter",
            ManagedTranslationShape::Single,
            ManagedTranslationContent::scalar("原文"),
            "",
            None,
        )
        .expect("测试单元应合法")
    }

    #[test]
    fn manifest_covers_declared_data_but_not_translation_pair() {
        let collection =
            ManagedTranslationCollection::new("quests", "翻译标题", vec![scalar_unit("q:1")])
                .expect("测试 collection 应合法");
        let base = ManagedTranslationSnapshot::new(source(), vec![collection.clone()])
            .expect("测试快照应合法");
        let state = Sha256Fingerprint::from_bytes([0x22; 32]);
        let mut translated = collection;
        let pair = translated.units[0]
            .translation_pair(ManagedTranslationContent::scalar("译文"), state)
            .expect("测试译文应合法");
        translated.units[0].translation = Some(pair);
        let translated =
            ManagedTranslationSnapshot::new(source(), vec![translated]).expect("带译文快照应合法");

        assert_eq!(
            base.manifest_fingerprint(),
            translated.manifest_fingerprint()
        );
        assert_ne!(base, translated);
    }

    #[test]
    fn snapshot_requires_unique_collection_names() {
        let collection =
            ManagedTranslationCollection::new("same", "", Vec::new()).expect("空集合应合法");
        assert!(matches!(
            ManagedTranslationSnapshot::new(source(), vec![collection.clone(), collection]),
            Err(ManagedTranslationModelError::DuplicateCollectionName(_))
        ));
    }

    #[test]
    fn checkpoint_captures_old_pair_and_rejects_duplicate_or_unknown_units() {
        let snapshot = ManagedTranslationSnapshot::new(
            source(),
            vec![
                ManagedTranslationCollection::new("quests", "", vec![scalar_unit("q:1")])
                    .expect("测试集合应合法"),
            ],
        )
        .expect("测试快照应合法");
        let unit = snapshot.collection("quests").unwrap().unit("q:1").unwrap();
        let pair = unit
            .translation_pair(
                ManagedTranslationContent::scalar("译文"),
                Sha256Fingerprint::from_bytes([5; 32]),
            )
            .expect("译文应合法");
        let replacement = ManagedTranslationReplacement::new("quests", "q:1", Some(pair));
        ManagedTranslationCheckpoint::new(&snapshot, vec![replacement.clone()])
            .expect("已知 unit checkpoint 应合法");
        assert!(matches!(
            ManagedTranslationCheckpoint::new(&snapshot, vec![replacement.clone(), replacement]),
            Err(ManagedTranslationModelError::DuplicateCheckpointIdentity { .. })
        ));
        assert!(matches!(
            ManagedTranslationCheckpoint::new(
                &snapshot,
                vec![ManagedTranslationReplacement::new(
                    "quests", "missing", None
                )]
            ),
            Err(ManagedTranslationModelError::UnknownCheckpointIdentity { .. })
        ));
    }
}
