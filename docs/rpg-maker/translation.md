# RPG Maker Translate 现行规格

```text
att mv translate --name NAME [PROFILE_ID] \
  [--terms TERMINOLOGY_TOML] [--placeholders PLACEHOLDER_TOML]

att mz translate --name NAME [PROFILE_ID] \
  [--terms TERMINOLOGY_TOML] [--placeholders PLACEHOLDER_TOML]
```

Translate 本身不运行 Lua。Profile 来自公共 `[translation].profiles`；省略时复用项目最近
一次成功保存的 Profile。术语和 Placeholder 分别保存在当前 MV/MZ 项目。少量局部补译使用
[Manual TOML](../manual/README.md)，不属于 Translate。

## 1. 准备与当前译文

ATT 从项目数据库读取 Extract 已经明确整理的 Semantic Scope、Group、Unit 和冻结来源
指纹，先按完整原文建立稳定 TaskBlock，再应用语言模块、实际术语命中、RPG Maker
Placeholder 与内置控制符，为每个 Unit 判定去向：Current、需要模型、可以复用，或不能
处理。

自动译文状态绑定当前原文、完整 Group 语境、语言、实际术语、实际 Placeholder、Prompt、
Profile 和 Client 语义。当前人工译文来自独立人工表，优先于自动译文；Translate 跳过它，
模型提交也不能覆盖它。

人工译文只在内部位置、Group kind、Unit 角色、写回 recipe、正文形状或原文变化时过期。
上下文、相邻文本、语言、术语、Placeholder 配置、Prompt、Profile 和 Client 变化不影响
已经应用的人工译文。

## 2. 全局去重

去重在整个当前项目内执行，以翻译角色、完整原文、保护后的输入和实际 Placeholder
绑定确定同族成员。

- 没有 Current：自然顺序最早的未译成员请求模型，再向未译成员传播；
- 只有一种 Current：向未译成员复用，不请求模型；
- 已有多种不同 Current：全部保留，不报冲突；有未译成员时，从未译成员重新选代表；
- 已有 Current 永远不被覆盖。

去重族、代表项和传播关系只在本次 Translate 运行中计算，不写入数据库。已定位 Unit 的
同文异译、质量修订或少量补译优先使用 [Manual TOML](../manual/README.md) 和可读 ID；复杂
筛选、计算生成或批量变换再使用 [Lua](../lua/README.md)。两者都不参与本次自动去重。

## 3. TaskBlock 与模型形状

RPG Maker 的明确 Semantic Scope、Group 物理顺序和 Profile 目标字符数共同决定
TaskBlock。装箱只使用完整原文、Group 类型、Unit 角色和紧凑 JSON object 结构；固定
`json` 围栏、两空格缩进、Current、译文、术语、Placeholder token、去重和临时 ID 不参与。
Group 保持完整、绝不拆开，TaskBlock 不跨 Scope；单个 Group 超过目标字符数时独占一块。完整公共规则见
[TaskBlock 规划规格](../translation/task-planning.md)。

只发送至少含一个模型代表的完整 TaskBlock。发送时保留块内全部 Group 和全部 Unit，只有
模型代表获得临时数字 ID；其他 Unit 省略 `id` 和 `type`，已有有效目标文本时显示经过
自身 Placeholder 绑定保护的目标文本，否则显示保护后的原文。块内所有 Group 的术语
命中按术语文件顺序合并并提供一次。

模型收到单一 `json` Markdown 围栏中的公共 JSON user message。Group 提供 `kind`，Unit
按实际含义提供 `speaker`、`body` 或 `choices` 等 `role`，`text` 始终是字符串数组。带
ID Unit 的 `type` 按现有 RPG Maker 形状映射：

- `single line`、`N lines, corresponding line by line` 和
  `N items, corresponding item by item` 使用 `strict`，译文恰好保持原数组项数和空槽；
- `free line breaking` 使用 `free`，译文至少一项，可以自然改变数组项数量。

响应的 translation 始终是字符串数组，每项不得含 CR、LF 或 NUL。四种响应模式见
[Prompt 规格](../translation/prompts.md)。

Partial 后重试重新判断 ID，但不重新装箱。原块中的已完成 Unit 继续省略 ID，以安全目标
译文提供语境；失败 Unit 获得从 `0` 开始的新临时 ID。一个完整块没有任何 ID 时只是不发送，
不会与相邻块合并。

## 4. 验收、并发和结果

每个 ID 独立检查 JSON 形状、strict/free 数组结构、Placeholder、源语残留，并执行语言模块能够
证明安全的修复。译文语言以项目语言对和 Prompt 的明确要求为准，ATT 不做短文本语言
识别式的猜测。合法 ID 可以提交，其他 ID 形成 Partial；合法响应外层处理、严格解析和
公共保守 JSON 修复都无法建立响应，或整个响应根结构无效时，该任务不提交。

任务之间可以并发执行，确认和提交仍按自然顺序进行。已确认的前序进度落库后，后续
失败或取消都不会把它带走。提交时重新检查当前来源、Unit、译文和语义状态，发现并发变化
或当前人工译文时，不覆盖新状态。

永久认证、授权、额度或账户错误一经类型化确认，就停止后续模型请求和 Task 准入，本次
Translate 为 Failed 并退出 `1`。普通 429 的 `Retry-After` 由同一 Client 共享；等待超过配置
上限或重试耗尽时，当前 Task 为 Unavailable，后续 Task 为 not_started，本次结果为
Incomplete 并退出 `0`。普通网络、超时或 HTTP 500 重试耗尽只使当前 Task Unavailable，
不会停止后续 Task。停止前已经活动且获得有效结果的 Task 仍按自然顺序验收和提交。

每个实际开始的 Task 写 `task.finished`：Complete、Partial、Unavailable、Failed、
NotCommittedAfterEarlierFailure 或 Cancelled；Partial、Unavailable 与 Failed 同时写可读任务
诊断。NotCommittedAfterEarlierFailure 仅表示已有可提交结果，但因更早任务失败没有写入，
不伪造当前 Task 的新错误。每次命令恰好写
一条 `translation.finished`：NotStarted、NoWork、Complete、Incomplete、Failed 或
Cancelled。含 Partial 或 Unavailable 任务但业务结果明确时，Translate 结果是 Incomplete，
退出码仍为 `0`；完整翻译目标尚未达成。CLI 明确显示 `状态：未完整`，并在 stderr 汇总
Partial、Unavailable、协议问题、可恢复请求耗尽、剩余决策和剩余位置；逐任务详情保留在
本次项目日志与任务记录。NoWork 和 Complete 分别显示 `无需处理` 与 `完整`。

`translation.finished` 固定保存完整 Task 计数，并保存 RPG Maker 专用的 accepted decisions、
written/remaining locations、remaining decisions、protocol diagnostics、recoverable request
exhaustions、request admission stopped 和 reconciliation 计数。Task 计数始终满足
`planned = started + not_started`；remaining decisions 与 remaining locations 按实际提交
递减，不把已准入、冲突或停发后的工作伪装成已完成。Failed 与 Cancelled 在已经形成计划和
引擎汇总时，也把同一份计数和汇总写入 JSONL，并在 stderr 打印一次短汇总；规划前失败或
提前取消不伪造引擎工作量。停止路径不补写 100%。Placeholder 等规划错误在任何模型请求前形成可读
`diagnostic.run_plan`，保存类似 `Map023.json:event17:page1:dialogue42` 的位置、规则文件、
自然规则号、原因和修改方法；结果为 Failed，数据库保持不变。

Partial 会保留合法 ID 和已确认前序进度；再次运行会重新判断剩余 ID 而不改变稳定装箱。
是否继续同一 Translate、修正系统性资源问题，还是用 Manual 完成少量局部补译，要按
[诊断与恢复指南](../guides/diagnosis-and-recovery.md#64-translate)根据具体原因与实际进展判断，
不能把任一选择当成所有失败的固定做法。
