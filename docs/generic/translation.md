# Generic Translate 现行规格

```text
att generic translate --name NAME [PROFILE_ID] \
  [--terms TERMINOLOGY_TOML] [--placeholders PLACEHOLDER_TOML]
```

显式 Profile 必须存在于公共 `[translation].profiles`。省略时复用项目最近一次成功保存的
Profile；项目还没有保存值时显式提供即可。术语和 Placeholder 分别属于当前 Generic 项目。

Translate 首先确认外部 JSONL 与最近成功 Extract 一致，然后按完整文件、Group 和 Unit 建立
稳定 TaskBlock，再准备、去重、分配临时 ID、请求模型、逐 ID 验收并保存有效结果。

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

去重的作用是减少请求；相同原文需要不同译文时，多种译文可以共存。已定位 Unit 的
同文异译、质量修订，或由人工或 agent 补译，都使用[原子数据库 Lua](../lua/README.md)精确提交，
不改写全局去重规则。

## 3. TaskBlock

一个 JSONL 文件直接对应一个 Semantic Scope，TaskBlock 不跨越 JSONL 文件。ATT 先只使用完整原文、
kind、自然顺序和固定 JSON 消息结构计算稳定字符数，再按文件内自然顺序把完整 Group 依次加入 TaskBlock。
当前块已有 Group，并且加入下一个 Group 会让稳定源文投影超过 Profile 目标时，
ATT 在这个 Group 前结束当前块，再建立下一个 TaskBlock。ATT 不重排、回填或跨越 JSONL 文件补充
容量；一个文件可以产生多个 TaskBlock。

Group 永远作为整体进入任务。单个 Group 超过 Profile 目标字符数时仍独占一个任务，后续
Group 继续按同一目标组合。目标字符数不是硬上限，也不决定 JSONL 的 Group 边界；建立
Group 和文件范围时遵守 [Generic JSONL 分组规则](jsonl.md#3-从源格式建立-group-与文件范围)。
Group 是不可拆分的最小语义整体；同一稳定 TaskBlock 内的相邻 Group 也会完整保留，
使重试能够继续提供原来装箱时已经存在的语境。Group 的语义边界不能依赖相邻 Group 恰好进入同一个
TaskBlock。完整公共规则见
[TaskBlock 规划规格](../translation/task-planning.md)。

发送 TaskBlock 时：

- 只发送至少含一个模型代表的完整 TaskBlock；
- 一旦发送 TaskBlock，其中全部 Group 保持自然顺序，全部 Unit 按原顺序参与语境；
- 只有代表项带临时数字 ID 并要求输出；
- Current、复用项、非代表项、非源语、完全保护和空文本只参与语境。

已有有效目标文本的语境项显示经过该 Unit Placeholder 绑定保护的目标文本，其他语境项
显示保护后的原文。TaskBlock 汇总其中全部 Group 的术语命中，并按术语文件顺序提供一次。
模型收到的 user message 是公共 JSON 格式，只包含 kind、有序文本、必要术语和临时 ID；
Group ID 和 Unit ID 留在 ATT 内部。Generic Unit 不输出 `role`。带 ID 的 Unit 使用
`type: "free"`；语境 Unit 省略 `id` 和 `type`。每个 `text` 按 LF 拆成字符串数组，保留
空行和末尾空槽。

Current、复用、去重、语言判断、Placeholder token、术语和 ID 都不参与装箱。全部 Unit
都已经 Current 时仍先建立完整 TaskBlock，随后得到零个实际请求。Partial 后重试也不会
孤立发送失败 Unit；原块中的已完成 Unit 会省略 ID，以安全目标译文继续提供语境。

## 4. 响应与提交

Generic 使用公共的四种 JSON 响应模式。关闭思考与原文回显时，每个 ID 的 value 是译文
字符串数组：

```json
{"0":["你好","世界"],"1":["爱丽丝"]}
```

数组可以自由改变项数，验收后用 LF 连接成 Generic 译文。数组必须至少有一项，每项不得
含 CR、LF 或 NUL，连接后的纯空白文本无效。思考与原文回显的其他组合、外层字段和 ID
规则见[Prompt 规格](../translation/prompts.md)；原文回显只检查字符串数组形状，不比较
内容。

每个 ID 独立执行 Placeholder 恢复、源语残留检查和安全修复。目标语言由项目语言对和
Prompt 明确规定，ATT 依靠这条契约确定译文语言，不做短文本语言识别猜测。

合法 ID 直接保存，其余 ID 形成 Partial。整个响应根结构或 JSON 无法解析时，该任务不保存。
任务并发执行，并始终按自然顺序确认和提交；取消或后续失败时，已经确认的前序进度原样
保留。

完成结果分为 Complete、Partial 与 Unavailable。Partial 和 Unavailable 是正常结果；
退出成功只说明本次命令正常结束，项目是否全部译完以结果报告为准。

Partial 会保留合法 ID 和已确认前序进度；再次运行只给仍需模型的 Unit 分配临时 ID，并
继续提供稳定 TaskBlock 的完整语境。是否继续同一 Translate、修正资源，还是由人工或 agent
修订，要按[诊断与恢复指南](../guides/diagnosis-and-recovery.md#64-translate)根据具体原因与
实际进展判断。
