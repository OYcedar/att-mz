//! RPG Maker 文本资产的 owner、组语义、结构化位置约束与快照指纹 framing。
//!
//! SQL 三表及各用例的行解码规则由相应的读写边界负责。

use crate::diagnostic::RpgMakerDiagnosticOwner;
use crate::fingerprint::{Sha256Fingerprint, Sha256FramedHasher};
use crate::rpg_maker::semantic_order::RpgMakerSemanticOrderKey;

/// `att.rpg_maker.rpg_maker_text_snapshot` 域指纹的唯一 framing 定义。
///
/// 写入方(Extract 资产存储)与校验方(WriteBack 资产读取)必须对同一份持久化
/// 内容产生逐字节一致的指纹,tag 布局因此只允许存在这一份实现。project_definition
/// 帧按"提供即掺入"编码:写入方传当前替换值,校验方按 owner 是否拥有项目级
/// 对话定义决定传 `Some`/`None`,两侧对同一 owner 的判断结果必须一致。
pub(crate) struct RpgMakerTextSnapshotFingerprintBuilder {
    hasher: Sha256FramedHasher,
}

impl RpgMakerTextSnapshotFingerprintBuilder {
    pub(crate) fn new(owner: RpgMakerAssetOwner, project_definition_json: Option<&str>) -> Self {
        let mut hasher = Sha256FramedHasher::new(b"att.rpg_maker.rpg_maker_text_snapshot");
        hasher.frame(1, owner.storage_name().as_bytes());
        if let Some(project_definition_json) = project_definition_json {
            hasher
                .frame(14, b"project_definition")
                .frame(15, project_definition_json.as_bytes());
        }
        Self { hasher }
    }

    /// 按持久化自然顺序写入一个文本组。
    pub(crate) fn group(
        &mut self,
        group_location: &str,
        semantic_order_key: &RpgMakerSemanticOrderKey,
        group_kind: &str,
        projection_recipes_json: &str,
    ) {
        let semantic_order_key = semantic_order_key
            .encode()
            .expect("已经建立的语义顺序键必须可编码");
        self.group_encoded(
            group_location,
            &semantic_order_key,
            group_kind,
            projection_recipes_json,
        );
    }

    /// 使用责任边界已经生成的规范顺序键 BLOB 写入一个文本组。
    pub(crate) fn group_encoded(
        &mut self,
        group_location: &str,
        semantic_order_key: &[u8],
        group_kind: &str,
        projection_recipes_json: &str,
    ) {
        self.hasher
            .frame(2, b"group")
            .frame(3, group_location.as_bytes())
            .frame(16, semantic_order_key)
            .frame(4, group_kind.as_bytes())
            .frame(5, projection_recipes_json.as_bytes());
    }

    /// 按持久化自然顺序写入一个文本单元。
    pub(crate) fn unit(
        &mut self,
        group_location: &str,
        unit_role: &str,
        semantic_order_key: &RpgMakerSemanticOrderKey,
        source_content_json: &str,
        source_context_json: &str,
    ) {
        let semantic_order_key = semantic_order_key
            .encode()
            .expect("已经建立的语义顺序键必须可编码");
        self.unit_encoded(
            group_location,
            unit_role,
            &semantic_order_key,
            source_content_json,
            source_context_json,
        );
    }

    /// 使用责任边界已经生成的规范顺序键 BLOB 写入一个文本单元。
    pub(crate) fn unit_encoded(
        &mut self,
        group_location: &str,
        unit_role: &str,
        semantic_order_key: &[u8],
        source_content_json: &str,
        source_context_json: &str,
    ) {
        self.hasher
            .frame(6, b"unit")
            .frame(7, group_location.as_bytes())
            .frame(8, unit_role.as_bytes())
            .frame(17, semantic_order_key)
            .frame(9, source_content_json.as_bytes())
            .frame(10, source_context_json.as_bytes());
    }

    /// 按持久化自然顺序写入一个 Mutation Claim 摘要行。
    pub(crate) fn claim(&mut self, resource_key: &str, access: &str, group_location: &str) {
        self.hasher
            .frame(11, b"claim")
            .frame(12, resource_key.as_bytes())
            .frame(18, access.as_bytes())
            .frame(13, group_location.as_bytes());
    }

    pub(crate) fn finish(self) -> Sha256Fingerprint {
        self.hasher.finish()
    }
}

/// 一个 RPG Maker 资产位置的提取所有者。
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum RpgMakerAssetOwner {
    Builtin,
    Rules,
}

impl RpgMakerAssetOwner {
    pub(crate) const fn diagnostic_owner(self) -> RpgMakerDiagnosticOwner {
        match self {
            Self::Builtin => RpgMakerDiagnosticOwner::Builtin,
            Self::Rules => RpgMakerDiagnosticOwner::Rules,
        }
    }

    pub(crate) const fn from_storage_name(value: &str) -> Option<Self> {
        match value.as_bytes() {
            b"builtin" => Some(Self::Builtin),
            b"rules" => Some(Self::Rules),
            _ => None,
        }
    }

    pub(crate) const fn storage_name(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Rules => "rules",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preencoded_semantic_order_keys_preserve_fingerprint_bytes() {
        let key = RpgMakerSemanticOrderKey::new(vec![1, 2, u64::MAX], 7);
        let encoded = key.encode().expect("测试顺序键应可编码");

        let mut structured = RpgMakerTextSnapshotFingerprintBuilder::new(
            RpgMakerAssetOwner::Builtin,
            Some(r#"{"rules":[]}"#),
        );
        structured.group("group", &key, "map", r#"["recipe"]"#);
        structured.unit("group", "scalar:name", &key, r#""原文""#, "{}");

        let mut preencoded = RpgMakerTextSnapshotFingerprintBuilder::new(
            RpgMakerAssetOwner::Builtin,
            Some(r#"{"rules":[]}"#),
        );
        preencoded.group_encoded("group", &encoded, "map", r#"["recipe"]"#);
        preencoded.unit_encoded("group", "scalar:name", &encoded, r#""原文""#, "{}");

        assert_eq!(structured.finish(), preencoded.finish());
    }

    #[test]
    fn storage_names_accept_only_schema_values() {
        assert_eq!(
            RpgMakerAssetOwner::from_storage_name("builtin"),
            Some(RpgMakerAssetOwner::Builtin)
        );
        assert_eq!(
            RpgMakerAssetOwner::from_storage_name("rules"),
            Some(RpgMakerAssetOwner::Rules)
        );
        assert_eq!(RpgMakerAssetOwner::from_storage_name("other"), None);
        assert_eq!(RpgMakerAssetOwner::from_storage_name("Builtin"), None);
        assert_eq!(RpgMakerAssetOwner::from_storage_name("BUILTIN"), None);
    }
}
