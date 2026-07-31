# Generic Translate 现行规格

```text
att generic translate --name NAME [PROFILE_ID] \
  [--terms TERMINOLOGY_TOML] [--placeholders PLACEHOLDER_TOML]
```

显式 Profile 必须存在于公共 `[translation].profiles`。省略时复用项目最近一次成功保存的
Profile；项目还没有保存值时显式提供即可。术语和 Placeholder 分别属于当前 Generic 项目。

Translate 首先确认外部 JSONL 与最近成功 Extract 一致，然后准备、去重、分组、请求模型、
逐 ID 验收并保存有效结果。

## 1. Unit 与 Current

每个 Unit 独立拥有译文和状态。空白、没有源语内容或完全受 Placeholder 保护的 text 直接
保留，不请求模型；其 Group 被发送时，它们仍按原样参与语境。

自动译文的 Current 状态绑定当前源文、Group 语境、语言、实际术语、实际 Placeholder、
Prompt 和 Client 语义。人工 Lua 译文的绑定更少：术语、Prompt、Profile 和 Client 的变化
对它没有影响；源文、Group 语境、语言或实际 Placeholder 变化仍会使它失效。

## 2. 全局去重

每次 Translate 在整个 Generic 项目内计算去重族。去重键包含完整源 `text`、保护后的文本
和实际 Placeholder 绑定，不包含文件、kind、Group 或 ID。

- 一个族没有 Current：选择自然顺序最早的未译 Unit 请求模型，并向其他未译成员传播；
- 只有一种 Current 译文：直接向未译成员传播，不请求模型；
- 已经有多种不同 Current：全部保留；存在未译成员时，从未译成员重新选择代表；
- 已有 Current 始终优先，任何传播都会跳过它们。

去重的作用是减少请求；相同原文需要不同译文时，多种译文可以共存。少量例外可在翻译后
使用[原子数据库 Lua](../lua/README.md)精确修订。

## 3. TaskBlock

一个 TaskBlock 不跨越 JSONL 文件。ATT 只在同一个文件内，
按文件内自然顺序把完整 Group 依次加入 TaskBlock。当前块已有 Group，并且加入下一个
Group 会让最终 user message 的实际字符数超过 Profile 目标时，ATT 在这个 Group 前结束
当前块，再建立下一个 TaskBlock。ATT 不重排、回填或跨越 JSONL 文件补充容量；一个文件
可以产生多个 TaskBlock。

Group 永远作为整体进入任务。单个 Group 超过 Profile 目标字符数时仍独占一个任务，后续
Group 继续按同一目标组合。目标字符数不是硬上限，也不决定 JSONL 的 Group 边界；建立
Group 和文件范围时遵守 [Generic JSONL 分组规则](jsonl.md#3-从源格式建立-group-与文件范围)。
必须共同出现的语境应当已经位于同一 Group，不能依赖相邻 Group 恰好进入同一个
TaskBlock。

发送 TaskBlock 时：

- 只有含模型代表的 Group 才发送；
- 一旦发送 Group，其全部 Unit 按原顺序参与语境；
- 只有代表项带临时数字 ID 并要求输出；
- Current、复用项、非代表项、非源语、完全保护和空文本只参与语境。

已有目标文本的语境项显示目标文本，其他语境项显示保护后的原文。模型收到的 user
message 只有 kind、有序文本、必要术语和临时 ID；Group ID 和 Unit ID 留在 ATT 内部。

## 4. 响应与提交

Generic 要求每个 value 为字符串：

```json
{"1":"你好\n世界","2":"爱丽丝"}
```

字符串可以自由改变 LF 数量；CR、NUL 和纯空白字符串属于无效译文。每个 ID 独立执行
Placeholder 恢复、源语残留检查和安全修复。目标语言由项目语言对和 Prompt 明确规定，
ATT 依靠这条契约确定译文语言，不做短文本语言识别猜测。

合法 ID 直接保存，其余 ID 形成 Partial。整个信封或 JSON 无法解析时，该任务不保存。
任务并发执行，并始终按自然顺序确认和提交；取消或后续失败时，已经确认的前序进度原样
保留。

完成结果分为 Complete、Partial 与 Unavailable。Partial 和 Unavailable 是正常结果；
退出成功只说明本次命令正常结束，项目是否全部译完以结果报告为准。
