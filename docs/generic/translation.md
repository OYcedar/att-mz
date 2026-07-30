# Generic Translate 现行规格

```text
att --config CONFIG generic translate --name NAME [PROFILE_ID] \
  [--terms TERMINOLOGY_TOML] [--placeholders PLACEHOLDER_TOML]
```

显式 Profile 必须存在于公共 `[translation].profiles`。省略时复用项目最近一次成功保存的
Profile；项目没有保存值时必须显式提供。术语和 Placeholder 分别属于当前 Generic 项目。

Translate 首先确认外部 JSONL 与最近成功 Extract 一致，然后准备、去重、分组、请求模型、
逐 ID 验收并保存有效结果。

## 1. Unit 与 Current

每个 Unit 独立拥有译文和状态。空白、没有源语内容或完全受 Placeholder 保护的 text 不
请求模型，但会在其 Group 被发送时参与语境。

自动译文的 Current 状态绑定当前源文、Group 语境、语言、实际术语、实际 Placeholder、
Prompt 和 Client 语义。人工 Lua 译文不绑定术语、Prompt、Profile 或 Client；源文、
Group 语境、语言或实际 Placeholder 变化仍会使它失效。

## 2. 全局去重

每次 Translate 在整个 Generic 项目内计算去重族。去重键包含完整源 `text`、保护后的文本
和实际 Placeholder 绑定，不包含文件、kind、Group 或 ID。

- 一个族没有 Current：选择自然顺序最早的未译 Unit 请求模型，并向其他未译成员传播；
- 只有一种 Current 译文：直接向未译成员传播，不请求模型；
- 已经有多种不同 Current：全部保留；存在未译成员时，从未译成员重新选择代表；
- 任何传播都不覆盖已有 Current。

去重只减少请求，不要求相同原文永久使用相同译文。少量例外可在翻译后使用
[原子数据库 Lua](../lua/README.md)精确修订。

## 3. TaskBlock

- 一个 TaskBlock 不跨越 JSONL 文件；
- 一个文件可以产生多个 TaskBlock；
- Group 永远不拆开，超过 Profile 目标字符数时独占一个任务；
- 只有含模型代表的 Group 才发送；
- 一旦发送 Group，其全部 Unit 按原顺序参与语境；
- 只有代表项带临时数字 ID 并要求输出；
- Current、复用项、非代表项、非源语、完全保护和空文本只参与语境。

已有目标文本的语境项显示目标文本；其他语境项显示保护后的原文。Group ID 和 Unit ID
不发送给模型，user message 只含 kind、有序文本、必要术语和临时 ID。

## 4. 响应与提交

Generic 要求每个 value 为字符串：

```json
{"1":"你好\n世界","2":"爱丽丝"}
```

字符串可以自由改变 LF 数量，但不得含 CR、NUL 或只含空白。每个 ID 独立执行
Placeholder 恢复、源语残留检查和安全修复。目标语言由项目语言对和 Prompt 明确要求；
ATT 不用不可靠的短文本语言识别猜测译文语言。

合法 ID 可以保存，其他 ID 形成 Partial。整个信封或 JSON 无法解析时，该任务不保存。
任务并发执行，但按自然顺序确认和提交；取消或后续失败不会删除已经确认的前序进度。

完成结果分为 Complete、Partial 与 Unavailable。Partial 和 Unavailable 是正常结果，退出
成功不表示整个项目已经翻译完成。
