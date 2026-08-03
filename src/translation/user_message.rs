//! 多个翻译引擎共用的模型 user message JSON 协议。
//!
//! 引擎负责提供已经按完整 TaskBlock 顺序整理的 Group、Unit、角色和行形状；本模块唯一
//! 负责字段名称、省略规则、字符串数组以及块内临时 ID 的 wire 表示。

use std::fmt;
use std::io::{self, Write};

use serde::Serialize as DeriveSerialize;
use serde::ser::{Serialize, SerializeSeq, Serializer};

use crate::execution::CooperativeCancellation;

use super::task_planning::TaskId;

#[derive(Clone, Copy, Debug, Eq, PartialEq, DeriveSerialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum TranslationReturnType {
    Strict,
    Free,
}

#[derive(Clone, Copy)]
pub(crate) struct TranslationUserText<'a>(&'a str);

impl<'a> TranslationUserText<'a> {
    pub(crate) const fn split_lines(text: &'a str) -> Self {
        Self(text)
    }
}

impl Serialize for TranslationUserText<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut sequence = serializer.serialize_seq(None)?;
        for line in self.0.split('\n') {
            sequence.serialize_element(line)?;
        }
        sequence.end()
    }
}

#[derive(DeriveSerialize)]
pub(crate) struct TranslationUserTerminology<'a> {
    source: &'a str,
    translation: &'a str,
}

impl<'a> TranslationUserTerminology<'a> {
    pub(crate) const fn new(source: &'a str, translation: &'a str) -> Self {
        Self {
            source,
            translation,
        }
    }
}

#[derive(DeriveSerialize)]
pub(crate) struct TranslationUserUnit<'a> {
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_task_id"
    )]
    id: Option<TaskId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<&'a str>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    return_type: Option<TranslationReturnType>,
    text: TranslationUserText<'a>,
}

impl<'a> TranslationUserUnit<'a> {
    pub(crate) const fn context(role: Option<&'a str>, text: &'a str) -> Self {
        Self {
            id: None,
            role,
            return_type: None,
            text: TranslationUserText::split_lines(text),
        }
    }

    pub(crate) const fn translated(
        id: TaskId,
        role: Option<&'a str>,
        return_type: TranslationReturnType,
        text: &'a str,
    ) -> Self {
        Self {
            id: Some(id),
            role,
            return_type: Some(return_type),
            text: TranslationUserText::split_lines(text),
        }
    }
}

fn serialize_task_id<S>(value: &Option<TaskId>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match value {
        Some(id) => serializer.collect_str(id),
        None => serializer.serialize_none(),
    }
}

#[derive(DeriveSerialize)]
pub(crate) struct TranslationUserGroup<'a> {
    kind: &'a str,
    units: Vec<TranslationUserUnit<'a>>,
}

impl<'a> TranslationUserGroup<'a> {
    pub(crate) const fn new(kind: &'a str, units: Vec<TranslationUserUnit<'a>>) -> Self {
        Self { kind, units }
    }
}

#[derive(DeriveSerialize)]
pub(crate) struct TranslationUserMessage<'a> {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    terminology: Vec<TranslationUserTerminology<'a>>,
    groups: Vec<TranslationUserGroup<'a>>,
}

impl<'a> TranslationUserMessage<'a> {
    pub(crate) const fn new(
        terminology: Vec<TranslationUserTerminology<'a>>,
        groups: Vec<TranslationUserGroup<'a>>,
    ) -> Self {
        Self {
            terminology,
            groups,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TranslationUserMessageCancelled;

impl fmt::Display for TranslationUserMessageCancelled {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("模型 user message JSON 序列化已取消")
    }
}

impl std::error::Error for TranslationUserMessageCancelled {}

/// 把受信 TaskBlock 投影成两空格缩进的 JSON wire，并在任意长度正文复制期间观察取消。
pub(crate) fn render_translation_user_message(
    message: &TranslationUserMessage<'_>,
    cancellation: &CooperativeCancellation,
) -> Result<String, TranslationUserMessageCancelled> {
    if cancellation.is_requested() {
        return Err(TranslationUserMessageCancelled);
    }
    let mut output = Vec::new();
    let (result, cancelled) = {
        let mut writer = CancellableJsonWriter {
            output: &mut output,
            cancellation,
            cancelled: false,
        };
        let result = serde_json::to_writer_pretty(&mut writer, message);
        (result, writer.cancelled)
    };
    if cancelled || cancellation.is_requested() {
        return Err(TranslationUserMessageCancelled);
    }
    result.expect("向内存写入受信 user message JSON 只能因合作式取消失败");
    Ok(String::from_utf8(output).expect("serde_json 只能生成 UTF-8"))
}

/// 返回一个完整源 Group 在首个位置和后续位置占用的稳定 JSON 字符数。
///
/// 计数使用紧凑 JSON 作为完整源 Group 的稳定结构投影，不把展示缩进计入 TaskBlock 装箱
/// 目标。调用方应传入省略 ID 与 `type`、并使用原始源文的 Group，从而保持装箱与本轮模型
/// 责任无关。
pub(crate) fn measure_translation_user_group(
    group: &TranslationUserGroup<'_>,
    cancellation: &CooperativeCancellation,
) -> Result<Option<(usize, usize)>, TranslationUserMessageCancelled> {
    if cancellation.is_requested() {
        return Err(TranslationUserMessageCancelled);
    }
    let mut counter = CancellableCharacterCounter {
        characters: 0,
        cancellation,
        cancelled: false,
        overflowed: false,
    };
    let result = serde_json::to_writer(&mut counter, group);
    if counter.cancelled || cancellation.is_requested() {
        return Err(TranslationUserMessageCancelled);
    }
    if counter.overflowed {
        return Ok(None);
    }
    result.expect("计数受信 user message JSON 只能因合作式取消失败");
    let Some(first) = "{\"groups\":["
        .len()
        .checked_add(counter.characters)
        .and_then(|value| value.checked_add("]}".len()))
    else {
        return Ok(None);
    };
    let Some(following) = counter.characters.checked_add(1) else {
        return Ok(None);
    };
    Ok(Some((first, following)))
}

struct CancellableJsonWriter<'a> {
    output: &'a mut Vec<u8>,
    cancellation: &'a CooperativeCancellation,
    cancelled: bool,
}

struct CancellableCharacterCounter<'a> {
    characters: usize,
    cancellation: &'a CooperativeCancellation,
    cancelled: bool,
    overflowed: bool,
}

impl Write for CancellableCharacterCounter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;

        if self.cancelled {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "已取消"));
        }
        for chunk in buffer.chunks(CANCELLATION_CHECK_BYTES) {
            if self.cancellation.is_requested() {
                self.cancelled = true;
                return Err(io::Error::new(io::ErrorKind::Interrupted, "已取消"));
            }
            let chunk_characters = chunk
                .iter()
                .filter(|byte| (**byte & 0b1100_0000) != 0b1000_0000)
                .count();
            let Some(characters) = self.characters.checked_add(chunk_characters) else {
                self.overflowed = true;
                return Err(io::Error::other("user message 字符数溢出"));
            };
            self.characters = characters;
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Write for CancellableJsonWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;

        if self.cancelled {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "已取消"));
        }
        for chunk in buffer.chunks(CANCELLATION_CHECK_BYTES) {
            if self.cancellation.is_requested() {
                self.cancelled = true;
                return Err(io::Error::new(io::ErrorKind::Interrupted, "已取消"));
            }
            self.output.extend_from_slice(chunk);
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_the_single_user_message_contract() {
        let cancellation = CooperativeCancellation::default();
        let id = TaskId::new(0);
        let message = TranslationUserMessage::new(
            vec![TranslationUserTerminology::new("魔王", "魔王")],
            vec![TranslationUserGroup::new(
                "dialogue",
                vec![
                    TranslationUserUnit::context(Some("speaker"), "村人"),
                    TranslationUserUnit::translated(
                        id,
                        Some("body"),
                        TranslationReturnType::Free,
                        "第一行\n\n",
                    ),
                ],
            )],
        );

        assert_eq!(
            render_translation_user_message(&message, &cancellation).unwrap(),
            r#"{
  "terminology": [
    {
      "source": "魔王",
      "translation": "魔王"
    }
  ],
  "groups": [
    {
      "kind": "dialogue",
      "units": [
        {
          "role": "speaker",
          "text": [
            "村人"
          ]
        },
        {
          "id": "0",
          "role": "body",
          "type": "free",
          "text": [
            "第一行",
            "",
            ""
          ]
        }
      ]
    }
  ]
}"#
        );
    }

    #[test]
    fn omits_empty_terminology_and_context_only_fields() {
        let message = TranslationUserMessage::new(
            Vec::new(),
            vec![TranslationUserGroup::new(
                "name",
                vec![TranslationUserUnit::context(None, "context")],
            )],
        );
        assert_eq!(
            render_translation_user_message(&message, &CooperativeCancellation::default()).unwrap(),
            r#"{
  "groups": [
    {
      "kind": "name",
      "units": [
        {
          "text": [
            "context"
          ]
        }
      ]
    }
  ]
}"#
        );
    }

    #[test]
    fn group_measurement_remains_a_compact_structural_projection() {
        let group =
            TranslationUserGroup::new("name", vec![TranslationUserUnit::context(None, "context")]);
        let (first, following) =
            measure_translation_user_group(&group, &CooperativeCancellation::default())
                .unwrap()
                .unwrap();
        assert_eq!(
            first,
            r#"{"groups":[{"kind":"name","units":[{"text":["context"]}]}]}"#
                .chars()
                .count()
        );
        assert_eq!(
            following,
            r#"{"kind":"name","units":[{"text":["context"]}]}"#.chars().count() + 1
        );

        let wire = render_translation_user_message(
            &TranslationUserMessage::new(Vec::new(), vec![group]),
            &CooperativeCancellation::default(),
        )
        .unwrap();
        assert!(wire.chars().count() > first);
    }
}
