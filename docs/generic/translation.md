# Generic Translate 现行规格

```text
att generic translate --name NAME [PROFILE_ID] \
  [--terms TERMINOLOGY_TOML] [--placeholders PLACEHOLDER_TOML] [--retry-rejected]
```

显式 Profile 必须存在于公共 `[translation].profiles`。省略时复用项目最近一次成功保存的
Profile；项目还没有保存值时显式提供即可。术语和 Placeholder 分别属于当前 Generic 项目。

Translate 首先确认外部 JSONL 与最近成功 Extract 一致，然后按完整文件、Group 和 Unit 建立
稳定 TaskBlock，再准备、去重、分配临时 ID、请求模型、逐 ID 验收并保存有效结果。

## 1. Unit 与当前译文

每个 Unit 独立拥有译文和状态。空白、没有源语内容或完全受 Placeholder 保护的 text 直接
保留，不请求模型；其 Group 被发送时，它们仍按原样参与语境。

自动译文只在对当前源文、完整实际 Group 语境、项目语言对和当前 Placeholder 等强
不变量仍适用时才是 Current。术语、Prompt、Profile、Client、模型参数和语言检查阈值只
影响后续请求，不使既有正文失去 Current。当前人工译文来自独立人工表，优先于
自动译文；Translate 跳过它，模型提交也不能覆盖它。

人工译文只在对应逻辑 Unit、所属文件、Group kind、正文形状、原文或项目语言对变化时
过期。完整 Group 语境、相邻文本、术语、Placeholder 配置、Prompt、Profile 和 Client 变化不
影响已经应用的人工译文。

旧语言对或旧 Group 语境的自动正文保留但不再是 Current，不参与模型语境、去重复用或
WriteBack。Translate 不在请求模型之前删除它；替代候选通过逐 ID 验收后，按读取到的
源文、Group 语境和旧译文状态执行 CAS 原子覆盖。请求失败、取消、额度不足、Partial 或
提交冲突都保留读取时的旧正文。

## 2. 全局去重

每次 Translate 在整个 Generic 项目内计算去重族。去重键包含完整源 `text`、保护后的文本
和实际 Placeholder 绑定，不包含文件、kind、Group 或 ID。

- 一个族没有 Current：选择自然顺序最早的未译 Unit 请求模型，并向其他未译成员传播；
- 只有一种 Current 译文：直接向未译成员传播，不请求模型；
- 已经有多种不同 Current：全部保留；存在未译成员时，从未译成员重新选择代表；
- 已有 Current 始终优先，任何传播都会跳过它们。

去重的作用是减少请求；相同原文需要不同译文时，多种译文可以共存。已定位 Unit 的同文
异译、质量修订或少量补译，优先使用 [Manual TOML](../manual/README.md) 精确提交，不改写
全局去重、Placeholder 或语言规则。复杂筛选、计算生成或批量变换再使用
[Lua](../lua/README.md)。

## 3. TaskBlock

一个 JSONL 文件直接对应一个 Semantic Scope，TaskBlock 不跨越 JSONL 文件。ATT 先只使用完整原文、
kind、自然顺序和紧凑 JSON object 结构计算稳定字符数；固定 `json` 围栏和两空格缩进不参与，
再按文件内自然顺序把完整 Group 依次加入 TaskBlock。
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

因此，外部转换不得把大量相互独立的记录放进单个 Group。ATT 只会在 Group 之间分配
TaskBlock，不会在一个 Group 内按容量切断语境。真正不可拆的长 Group 可以独占任务；由错误
分组造成的超长任务必须回到 JSONL 转换步骤修正，不能靠重试、临时分片或放宽响应验收处理。

发送 TaskBlock 时：

- 只发送至少含一个模型代表的完整 TaskBlock；
- 一旦发送 TaskBlock，其中全部 Group 保持自然顺序，全部 Unit 按原顺序参与语境；
- 只有代表项带临时数字 ID 并要求输出；
- Current、复用项、非代表项、非源语、完全保护和空文本只参与语境。

已有有效目标文本的语境项显示经过该 Unit Placeholder 绑定保护的目标文本，其他语境项
显示保护后的原文。TaskBlock 汇总其中全部 Group 的术语命中，并按术语文件顺序提供一次。
模型收到的 user message 是单一 `json` Markdown 围栏中的公共 JSON，只包含 kind、有序
文本、必要术语和临时 ID；Group ID 和 Unit ID 留在 ATT 内部。Generic Unit 不输出
`role`。带 ID 的 Unit 使用 `type: "free"`；语境 Unit 省略 `id` 和 `type`。每个 `text`
按 LF 拆成字符串数组，保留空行和末尾空槽。

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

每个 ID 按[译文候选验收规格](../translation/candidate-validation.md)独立检查。结构合法的
候选立即保存；源语残留、术语、语义和布局只产生 Review，不拒绝候选。唯一绑定但违反强
不变量的候选保存为 Rejected，默认后续 Translate 不重复请求；只有显式
`--retry-rejected` 才重新请求。响应无法建立唯一 ID 映射时，相应 Unit 保持 pending。
保存 Rejected 时同样核对规划读取到的旧自动正文和状态；旧正文可以与当前 Rejected 同时
保留，候选拒绝本身不得提前清除旧正文，正文或状态已变化时则报告提交冲突。
任务并发执行，并始终按自然顺序确认和提交；取消或后续失败时，已经确认的前序进度原样
保留。

永久认证、授权、额度或账户错误一经类型化确认，就停止后续模型请求和 Task 准入，本次
Translate 为 Failed 并退出 `1`。普通 429 的共享 `Retry-After` 等待超过配置上限或重试耗尽
时，当前 Task 为 Unavailable，后续 Task 为 not_started，本次结果为 Incomplete 并退出
`0`。普通网络、超时或 HTTP 500 重试耗尽只使当前 Task Unavailable，不停止后续 Task。
停止前已经准入且获得有效结果的 Task 仍按自然顺序验收，并在当前 CAS 成立时提交；单个外部
请求失败不能让其他已经付费取得且通过验收的结果失效。

每个已开始至少一次真实外部 HTTP attempt 的 Task 写 `task.finished`：Complete、Partial、Unavailable、Failed、
NotCommittedAfterEarlierFailure 或 Cancelled。Partial、Unavailable 与 Failed 同时写可读任务
诊断。NotCommittedAfterEarlierFailure 只用于更早的数据库提交、内部最终化或取消边界已经使
后续副作用不再安全的情况；外部模型请求失败本身不能把后续合法响应改成这一状态。该终态不伪造
当前 Task 的新错误。
每次命令恰好写一条 `translation.finished`：
NotStarted、NoWork、Complete、Incomplete、Failed 或 Cancelled。含 Partial 或 Unavailable
任务但业务结果明确时，Translate 结果是 Incomplete，退出码仍为 `0`；项目是否全部译完以
该事件和数据库当前状态为准。CLI 明确显示 `状态：未完整`，并在 stderr 汇总 Partial、
Unavailable、写入冲突和响应问题；逐任务详情保留在本次项目日志与任务记录。NoWork 和
Complete 分别显示 `无需处理` 与 `完整`。

`translation.finished` 固定保存 planned、started、complete、partial、unavailable、failed、
cancelled 与 not_started Task 计数，并保存 Generic 专用的 cleared/reused/accepted/written/
conflicted units、response problems、planned_units、remaining_units、recoverable request
exhaustions 与 request admission stopped。Task 计数始终满足
`planned = started + not_started`；remaining_units 等于计划交给模型的 Unit 减去实际写入
的 Unit，并且必须满足 `planned_units = written_units + remaining_units`；CAS 冲突不算写入。
Task 只在第一次真实 HTTP attempt 开始时计入 started，准入前失败或停发仍属于 not_started。
Failed 与 Cancelled 在已经形成计划和引擎汇总时，也把同一份
计数和汇总写入 JSONL，并在 stderr 打印一次短汇总；规划前失败或提前取消不伪造引擎
工作量。停止路径不补写 100%。它取代按 Task 与 Unit 含义混合的通用 Partial 汇总。
Placeholder 等规划错误发生在任何 Task 或模型请求之前时，结果为 Failed；可读诊断保留
规则文件、自然规则号、类似 `story.jsonl:line3:unit2:text` 的位置、原因和修改方法，数据库
保持不变。

Partial 会保留合法 ID 和已确认前序进度；再次运行只给仍需模型的 Unit 分配临时 ID，并
继续提供稳定 TaskBlock 的完整语境。是否继续同一 Translate、修正系统性资源问题，还是用
Manual 完成少量局部补译，要按
[诊断与恢复指南](../guides/diagnosis-and-recovery.md#64-translate)根据具体原因与实际进展判断。

Translate 的 Complete 只表示计划内 Unit 都有结构合法的当前译文；译后 QA 另行报告
`clean`、`needs_review` 或 `unverified`，不把质量风险伪装成 Translate 失败。
