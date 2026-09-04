# RPG Maker Translate 现行规格

```text
att mv translate --name NAME [PROFILE_ID] \
  [--terms TERMINOLOGY_TOML] [--placeholders PLACEHOLDER_TOML] [--retry-rejected]

att mz translate --name NAME [PROFILE_ID] \
  [--terms TERMINOLOGY_TOML] [--placeholders PLACEHOLDER_TOML] [--retry-rejected]
```

Translate 读取 Extract 建立的语义资产，使用选定 Profile、术语和 Placeholder 构造模型任务，
验收响应并把适用译文提交到当前 MV/MZ 项目。Profile 来自公共 `[translation].profiles`；省略时
复用项目最近一次成功保存的 Profile。术语和 Placeholder 分别保存在当前项目。少量局部补译使用
[Manual TOML](../manual/README.md)，批量上下文处理或特殊数据库修改使用[项目数据库 Lua](../lua/README.md)。

## 1. 准备与当前译文

ATT 从项目数据库读取 Extract 已经明确整理的 Semantic Scope、Group、Unit 和冻结来源
指纹，先按完整原文建立稳定 TaskBlock，再应用语言模块、实际术语命中、RPG Maker
Placeholder 与内置控制符，为每个 Unit 判定去向：Current、需要模型、可以复用，或不能
处理。

自动译文的当前适用性指纹必须与当前原文、完整实际 Group 来源语境、项目语言对、位置、角色和
写回结构精确匹配；随后 Translate 再独立执行当前 Placeholder 强验收，两项都成立才是
Current。术语、Prompt、Profile、Client、模型参数和语言检查阈值只影响后续请求，不改变
既有正文的适用性。当前人工译文来自独立人工表，优先于自动译文；Translate 跳过它，
模型提交也不能覆盖它。

人工译文的适用性只绑定内部位置、Group kind、Unit 角色、写回 recipe、正文形状、原文和
项目语言对；这些事实变化时，旧人工记录过期。完整 Group 语境、相邻文本、术语、Prompt、
Profile 和 Client 不参与人工适用性。Placeholder 配置也不改变适用性，但新强契约会独立
复验仍适用的正文：合法则保留，违反强不变量则保留正文并转入 Rejected。

不匹配当前语言对或 Group 来源语境的自动正文和状态保留但不再是 Current，不参与模型语境、去重复用或
WriteBack。Translate 不在请求模型之前删除它；替代候选通过验收后，按当前来源、Unit、
Group 语境和旧译文状态执行 CAS（比较并交换）原子覆盖。请求失败、取消、额度不足、Partial 或提交冲突
都保留读取时的正文；绑定事实恢复后，原状态可以重新匹配。

Rejected 候选与自动正文共用同一当前适用性指纹。候选原文、来源上下文、项目
语言对、Unit 位置/角色/recipe 和完整 Group 来源语境必须仍匹配，Manual、Lua 与 Translate
才把它视为当前；`readable_id` 仅展示。语言或 Extract 事实变化不删除同一自然 Unit 的候选，
但旧候选不会预填给新目标语言；相关事实恢复后可以再次使用。改变 Client、Prompt、术语或
语言检查阈值不重写历史候选，重新尝试由 `--retry-rejected` 明确触发。

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

### 4.1 候选验收与 Rejected

每个 ID 按[译文候选验收规格](../translation/candidate-validation.md)独立检查。结构合法的
候选立即保存；源语残留、术语、语义和布局只产生 Review，不拒绝候选。唯一绑定但违反强
不变量的候选保存为 Rejected，默认后续 Translate 不重复请求；只有显式
`--retry-rejected` 才重新请求。也可以使用 `manual export --selection rejected` 导出并
修订候选。响应无法建立唯一 ID 映射时，相应 Unit 保持 pending。

已有自动译文经当前强不变量复核不通过时，准备事务按读取快照把正文和违反原因原子转入
Rejected。`--retry-rejected` 的合法响应以准备完成后的空 Current 为提交基线；代表和全部
传播目标仍在该基线时才整项写入，任一位置发生并发变化就整体回滚。请求失败或取消时保留
Rejected 中的原正文和原因，不把它误报为 Current，也不丢失恢复证据。

### 4.2 提交顺序与请求失败

任务之间可以并发执行，确认和提交仍按自然顺序进行。后续任务失败或取消时，已确认提交
的前序进度继续保留。提交时重新检查当前来源、Unit、译文和语义状态，发现并发变化
或当前人工译文时，不覆盖新状态。

永久认证、授权、额度或账户错误一经类型化确认，就停止后续模型请求和 Task 准入，本次
Translate 为 Failed 并退出 `1`。普通 429 的 `Retry-After` 由同一 Client 共享；等待超过配置
上限或重试耗尽时，当前 Task 为 Unavailable，后续 Task 为 not_started，本次结果为
Incomplete 并退出 `0`。普通网络、超时、HTTP 500、502–504 或 520–524 重试耗尽只使当前
Task Unavailable，不会停止后续 Task。停止前已经准入且获得有效结果的 Task 仍按自然顺序验收，并在当前 CAS
成立时提交；单个外部请求失败不能让其他已经付费取得且通过验收的结果失效。

### 4.3 任务状态与命令状态

每个已开始至少一次真实外部 HTTP attempt 的 Task 写 `task.finished`：Complete、Partial、Unavailable、Failed、
NotCommittedAfterEarlierFailure 或 Cancelled；Partial、Unavailable 与 Failed 同时写可读任务
诊断。NotCommittedAfterEarlierFailure 只用于更早的数据库提交、内部最终化或取消边界已经使
后续副作用不再安全的情况；外部模型请求失败本身不能把后续合法响应改成这一状态，也不伪造
当前 Task 的新错误。

每次命令恰好写一条 `translation.finished`：NotStarted、NoWork、Complete、Incomplete、
Failed 或 Cancelled。任务级 Partial 或 Unavailable 表示某个请求的结果；命令级 Incomplete
表示本次 Translate 仍有未完成内容，退出码为 `0`。CLI 显示 `状态：未完整`，并在 stderr 汇总
Partial、Unavailable、协议问题、可恢复请求耗尽、剩余决策和剩余位置；逐任务详情保留在
本次项目日志与任务记录。NoWork 和 Complete 分别显示 `无需处理` 与 `完整`。

### 4.4 计数与规划失败

`translation.finished` 固定保存完整 Task 计数，并保存 RPG Maker 专用的 accepted decisions、
written/remaining locations、remaining decisions、运行结束时仍为 Rejected 的
rejected_locations、protocol diagnostics、recoverable request exhaustions、request admission
stopped 和 reconciliation 计数。Task 计数始终满足
`planned = started + not_started`；remaining decisions 与 remaining locations 按实际提交
递减，不把已准入、冲突或停发后的工作伪装成已完成。started 只在第一次真实 HTTP
attempt 开始时计数；准入前失败、取消或停发仍计入 not_started。Failed 与 Cancelled 在已经形成计划和
引擎汇总时，也把同一份计数和汇总写入 JSONL，并在 stderr 打印一次短汇总；规划前失败或
提前取消不伪造引擎工作量。停止路径不补写 100%。Placeholder 等规划错误在任何模型请求前形成可读
`diagnostic.run_plan`，保存类似 `Map023.json:event17:page1:dialogue42` 的位置、规则文件、
自然规则号、原因和修改方法；结果为 Failed，数据库保持不变。

rejected_locations 必须是 remaining_locations 的子集。准备阶段的失效转入、Rejected 复用，
以及每个已提交 Task 的首次拒绝、再次拒绝和修复都更新同一终态计数；提交失败或冲突不伪造
状态变化。NoWork 与 Complete 要求 remaining 和 Rejected 同时为零。

### 4.5 继续翻译与质量验收

Partial 后再次运行会重新判断剩余 ID，保留已经接受的结果和稳定块边界。pending 可正常
继续；Rejected 需要显式 `--retry-rejected` 或 Manual 修订。系统性资源问题先修正规则或
Prompt，具体分流见[诊断与恢复指南](../guides/diagnosis-and-recovery.md#64-translate)。

Translate 的 Complete 只表示计划内 Unit 都有结构合法的当前译文；译后 QA 另行报告
`clean`、`needs_review` 或 `unverified`，不把质量风险伪装成 Translate 失败。
