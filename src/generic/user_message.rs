//! 把 Generic 已规划的完整 TaskBlock 投影为公共模型消息。
//!
//! 临时 ID、语境正文与术语的选择由 Generic 规划结果提供，公共渲染器只负责协议编码。

use crate::execution::CooperativeCancellation;
use crate::translation::planning_resource::CompiledTerminology;
use crate::translation::user_message::{
    TranslationReturnType, TranslationUserGroup, TranslationUserMessage,
    TranslationUserTerminology, TranslationUserUnit, render_translation_user_message,
};

use super::{GenericPlanningError, PlannedTask};

#[cfg(test)]
pub(crate) fn render_generic_user_message(
    task: &PlannedTask,
    terminology: &CompiledTerminology,
) -> String {
    render_generic_user_message_with_cancellation(
        task,
        terminology,
        &CooperativeCancellation::default(),
    )
    .expect("不取消的受信模型消息必须可以渲染")
}

pub(crate) fn render_generic_user_message_with_cancellation(
    task: &PlannedTask,
    terminology: &CompiledTerminology,
    cancellation: &CooperativeCancellation,
) -> Result<String, GenericPlanningError> {
    ensure_message_render_running(cancellation)?;
    let mut selected_terminology = Vec::with_capacity(task.terminology_indices().len());
    for index in task.terminology_indices() {
        ensure_message_render_running(cancellation)?;
        let entry = &terminology.entries()[*index];
        selected_terminology.push(TranslationUserTerminology::new(
            entry.term(),
            entry.translation(),
        ));
    }
    let mut groups = Vec::with_capacity(task.groups().len());
    for group in task.groups() {
        ensure_message_render_running(cancellation)?;
        let mut units = Vec::with_capacity(group.units().len());
        for unit in group.units() {
            ensure_message_render_running(cancellation)?;
            units.push(match unit.output_id() {
                Some(id) => TranslationUserUnit::translated(
                    id,
                    None,
                    TranslationReturnType::Free,
                    unit.text(),
                ),
                None => TranslationUserUnit::context(None, unit.text()),
            });
        }
        groups.push(TranslationUserGroup::new(group.kind(), units));
    }
    ensure_message_render_running(cancellation)?;
    render_translation_user_message(
        &TranslationUserMessage::new(selected_terminology, groups),
        cancellation,
    )
    .map_err(|_| GenericPlanningError::Cancelled)
}

fn ensure_message_render_running(
    cancellation: &CooperativeCancellation,
) -> Result<(), GenericPlanningError> {
    if cancellation.is_requested() {
        Err(GenericPlanningError::Cancelled)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::num::NonZeroUsize;
    use std::sync::Arc;

    use crate::generic::{
        GenericInitRequest, GenericPlaceholderRuleDefinition, GenericPlaceholderRuleSource,
        GenericPlaceholderService, GenericProjectStore, GenericUnitKey,
        automatic_translation_state_fingerprint, prepare_generic_translation,
    };
    use crate::language::{
        JapaneseLanguageModule, JapaneseResidualPolicy, LanguageId, LanguageModule,
    };
    use crate::translation::planning_resource::{TerminologyEntry, compile_terminology};

    use super::*;

    fn compact_json(message: &str) -> String {
        let json = message
            .strip_prefix("```json\n")
            .and_then(|value| value.strip_suffix("\n```"))
            .expect("模型 user message 必须是单一 JSON 围栏");
        serde_json::to_string(
            &serde_json::from_str::<serde_json::Value>(json)
                .expect("模型 user message 必须是有效 JSON"),
        )
        .expect("模型 user message 应该可以重新序列化")
    }

    #[test]
    fn model_message_keeps_terms_and_uses_safe_current_or_source_context() {
        let temporary = tempfile::tempdir().expect("应该可建立临时目录");
        let source_root = temporary.path().join("source");
        fs::create_dir_all(source_root.join("nested")).expect("应该可建立输入目录");
        fs::write(
            source_root.join("nested/scene.jsonl"),
            concat!(
                r#"{"id":"secret-context-group","kind":"dialogue","units":["#,
                r#"{"id":"secret-context","text":"魔王 {hero}"}]}"#,
                "\n",
                r#"{"id":"secret-invalid-current-group","kind":"dialogue","units":["#,
                r#"{"id":"secret-invalid-current","text":"あ {rival}"}]}"#,
                "\n",
                r#"{"id":"secret-reuse-group","kind":"dialogue","units":["#,
                r#"{"id":"secret-reuse","text":"あ {rival}"}]}"#,
                "\n",
                r#"{"id":"secret-output-group","kind":"dialogue","units":["#,
                r#"{"id":"secret-output","text":"こんにちは"}]}"#,
                "\n"
            ),
        )
        .expect("应该可写入 Generic 输入");
        let (store, _) = GenericProjectStore::initialize(GenericInitRequest {
            project_name: "message-test".parse().expect("项目名应该合法"),
            workspace_root: temporary.path().join("project"),
            source_root: Some(source_root),
            source_language: Some(LanguageId::parse("ja").expect("源语言应该合法")),
            target_language: Some(LanguageId::parse("zh-Hans").expect("目标语言应该合法")),
        })
        .expect("Generic 项目应该可初始化");
        store.extract().expect("Generic 输入应该可提取");
        let snapshot = store.load_snapshot().expect("应该可读取 Generic 快照");
        let terminology = Arc::new(
            compile_terminology(vec![TerminologyEntry::new(
                "魔王",
                "魔王（Demon King）",
                vec!["魔王".to_owned()],
            )])
            .expect("术语应该可编译"),
        );
        let placeholder_rules = GenericPlaceholderService::default()
            .compile(vec![GenericPlaceholderRuleDefinition::new(
                Some(vec!["dialogue".to_owned()]),
                r"\{[^}]+\}",
            )])
            .expect("Placeholder 规则应该合法");
        let current_group = snapshot.files()[0].groups()[0].clone();
        let current_unit = current_group.units()[0].clone();
        let current_state = automatic_translation_state_fingerprint(
            snapshot.project().language_pair(),
            &GenericUnitKey::new(current_group.id().to_owned(), current_unit.id().to_owned()),
            current_unit.source_text(),
            current_group.context_fingerprint(),
        );
        let invalid_current_group = snapshot.files()[0].groups()[1].clone();
        let invalid_current_unit = invalid_current_group.units()[0].clone();
        let invalid_current_state = automatic_translation_state_fingerprint(
            snapshot.project().language_pair(),
            &GenericUnitKey::new(
                invalid_current_group.id().to_owned(),
                invalid_current_unit.id().to_owned(),
            ),
            invalid_current_unit.source_text(),
            invalid_current_group.context_fingerprint(),
        );
        store
            .commit_translations(
                snapshot
                    .project()
                    .extracted_raw_fingerprint()
                    .expect("Extract 应保存原始指纹"),
                &[
                    crate::generic::TranslationWrite {
                        group_id: current_group.id().to_owned(),
                        unit_id: current_unit.id().to_owned(),
                        expected_source_text: current_unit.source_text().to_owned(),
                        expected_group_context: current_group.context_fingerprint(),
                        translation: "已有上下文 {hero}".to_owned(),
                        state_fingerprint: current_state,
                        expected_translation: None,
                        was_current_rejected: false,
                    },
                    crate::generic::TranslationWrite {
                        group_id: invalid_current_group.id().to_owned(),
                        unit_id: invalid_current_unit.id().to_owned(),
                        expected_source_text: invalid_current_unit.source_text().to_owned(),
                        expected_group_context: invalid_current_group.context_fingerprint(),
                        translation: "损坏的已有译文".to_owned(),
                        state_fingerprint: invalid_current_state,
                        expected_translation: None,
                        was_current_rejected: false,
                    },
                ],
            )
            .expect("应该可保存测试译文");
        let snapshot = store.load_snapshot().expect("应该可重读 Generic 快照");
        let language_module: Arc<dyn LanguageModule> = Arc::new(JapaneseLanguageModule::new(
            JapaneseResidualPolicy::new(
                NonZeroUsize::new(2).expect("测试阈值应该非零"),
                Vec::new(),
            )
            .expect("日文残留策略应该合法"),
        ));
        let prepared = prepare_generic_translation(
            &snapshot,
            Arc::clone(&terminology),
            &placeholder_rules,
            &GenericPlaceholderRuleSource::ProjectSnapshot,
            language_module,
            NonZeroUsize::new(10_000).expect("常量应该非零"),
            false,
            &CooperativeCancellation::default(),
        )
        .expect("翻译任务应该可规划");
        let message =
            render_generic_user_message(&prepared.plan().tasks()[0], terminology.as_ref());
        let wire = compact_json(&message);

        assert!(wire.contains("\"source\":\"魔王\",\"translation\":\"魔王（Demon King）\""));
        assert_eq!(
            wire.matches("\"source\":\"魔王\",\"translation\":\"魔王（Demon King）\"")
                .count(),
            1,
            "TaskBlock 命中的术语必须按文件顺序合并后只提供一次"
        );
        assert!(wire.contains("\"kind\":\"dialogue\""));
        assert!(wire.contains("\"text\":[\"已有上下文 "));
        assert!(wire.contains("\"text\":[\"あ "));
        assert!(wire.contains("\"id\":\"0\""));
        assert!(wire.contains("\"id\":\"1\""));
        assert!(
            prepared.plan().reused().is_empty(),
            "只供阅读的源文回退不能成为重复 Unit 的复用译文"
        );
        assert!(!message.contains("损坏的已有译文"));
        assert!(!message.contains("{hero}"));
        assert!(!message.contains("{rival}"));
        for hidden_identity in [
            "secret-context-group",
            "secret-invalid-current-group",
            "secret-invalid-current",
            "secret-reuse-group",
            "secret-reuse",
            "secret-output-group",
            "secret-output",
            "secret-context",
            "nested/scene.jsonl",
        ] {
            assert!(
                !message.contains(hidden_identity),
                "模型输入不应泄漏稳定项目身份：{hidden_identity}"
            );
        }
    }

    #[test]
    fn stable_source_packing_keeps_oversized_groups_and_renderer_keeps_full_content() {
        let temporary = tempfile::tempdir().expect("应该可建立临时目录");
        let source_root = temporary.path().join("source");
        fs::create_dir_all(&source_root).expect("应该可建立输入目录");
        let mut lines = Vec::new();
        for index in 0..18 {
            let units = if index == 1 {
                (0..12)
                    .map(|unit| {
                        serde_json::json!({
                            "id": format!("unit-{unit}"),
                            "text": format!("こんにちは \"{unit}\"\n魔王"),
                        })
                    })
                    .collect::<Vec<_>>()
            } else {
                let text = if index == 0 {
                    format!("{} 魔王", "こんにちは".repeat(80))
                } else {
                    format!("こんにちは \"{index}\"\n魔王")
                };
                vec![serde_json::json!({"id": "unit", "text": text})]
            };
            lines.push(
                serde_json::json!({
                    "id": format!("group-{index}"),
                    "kind": "dialogue\"kind",
                    "units": units,
                })
                .to_string(),
            );
        }
        fs::write(
            source_root.join("scene.jsonl"),
            format!("{}\n", lines.join("\n")),
        )
        .expect("应该可写入 Generic 输入");
        let (store, _) = GenericProjectStore::initialize(GenericInitRequest {
            project_name: "size-test".parse().expect("项目名应该合法"),
            workspace_root: temporary.path().join("project"),
            source_root: Some(source_root),
            source_language: Some(LanguageId::parse("ja").expect("源语言应该合法")),
            target_language: Some(LanguageId::parse("zh-Hans").expect("目标语言应该合法")),
        })
        .expect("Generic 项目应该可初始化");
        store.extract().expect("Generic 输入应该可提取");
        let snapshot = store.load_snapshot().expect("应该可读取 Generic 快照");
        let terminology = Arc::new(
            compile_terminology(vec![TerminologyEntry::new(
                "魔王",
                "魔\"王 King",
                vec!["魔王".to_owned()],
            )])
            .expect("术语应该可编译"),
        );
        let placeholder_rules = GenericPlaceholderService::default()
            .compile(Vec::new())
            .expect("空 Placeholder 规则应该合法");
        let language_module: Arc<dyn LanguageModule> = Arc::new(JapaneseLanguageModule::new(
            JapaneseResidualPolicy::new(NonZeroUsize::MIN, Vec::new())
                .expect("日文残留策略应该合法"),
        ));
        let target = NonZeroUsize::new(260).expect("常量应该非零");
        let prepared = prepare_generic_translation(
            &snapshot,
            Arc::clone(&terminology),
            &placeholder_rules,
            &GenericPlaceholderRuleSource::ProjectSnapshot,
            language_module,
            target,
            false,
            &CooperativeCancellation::default(),
        )
        .expect("翻译任务应该可规划");

        assert!(prepared.plan().tasks().len() > 2);
        assert!(
            prepared
                .plan()
                .tasks()
                .iter()
                .any(|task| task.groups().len() > 1),
            "除超大 Group 外，目标大小应允许多个 Group 同处一个 Task"
        );
        let rendered = prepared
            .plan()
            .tasks()
            .iter()
            .map(|task| render_generic_user_message(task, terminology.as_ref()))
            .map(|message| compact_json(&message))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("\\\""));
        assert!(rendered.contains("\",\"魔王\"]"));
        assert!(rendered.contains("\"id\":\"10\""));
    }
}
