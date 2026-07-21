# RPG Maker 翻译现行规格

本文定义 MV/MZ Standard Translate 的资源生命周期、规划、模型协议、验收、state 与提交。
术语字段由[术语现行规格](terminology.md)唯一规定，Placeholder 字段由
[规则文件](rules.md#6-placeholder-rules)唯一规定，Lua Translate API 见
[Lua 技术参考](lua.md#7-translatepreparecurrent-与-accept)。

## 1. 命令与阶段顺序

<!-- att-example: illustrative -->
```text
att --config FILE mz translate --name NAME PROFILE_ID \
  [--terms TERMS_TOML] [--placeholders PLACEHOLDERS_TOML] [--lua SCRIPT_LUA]

att --config FILE mv translate --name NAME PROFILE_ID \
  [--terms TERMS_TOML] [--placeholders PLACEHOLDERS_TOML] [--lua SCRIPT_LUA]
```

提供术语或 Placeholder 文件时，先严格解析并完整替换对应 canonical 资源；省略参数复用
项目当前资源。空定义必须分别显式提供 `term = []` 或 `rule = []`。任一资源无效时不开始
Standard 请求。

Standard 完成后才执行显式 Lua。Standard 的 `Complete`、`Partial`、`Unavailable` 是
正常业务结果，不阻止 Lua；技术错误阻止后续阶段。Standard 已提交译文不会因 Lua 失败
组合回滚。

项目开启时验证冻结来源、活动 owner 来源指纹和资产快照指纹。翻译以标准语义单元身份
读写，不以物理 JSON 地址寻址。

## 2. 公共翻译语义

Profile 由 `[[rpg_maker.translation_profiles]]` 精确选择 `PROFILE_ID`，再解析它引用的
公共 LLM Client。语言对来自项目 metadata，Prompt 精确读取：

<!-- att-example: illustrative -->
```text
<prompts.root>/rpg_maker/<source>--<target>.md
```

Prompt 必须是普通 UTF-8 非空白文件；没有父语言、大小写、默认文件或目录首项回退。
Standard 与 Translate Lua 复用本轮解析出的 engine、语言对、语言模块、Prompt、Client、
实际 Placeholder 和实际有序术语命中。Lua 不继承 Standard 的 planning、request 或任务
并发策略。

MV 与 MZ 共享翻译流程，但 engine 是语义事实：两者 Builtin 控制符矩阵不同，state 也
绑定 engine。精确矩阵见[规则文件](rules.md#68-mvmz-builtin-控制符矩阵)。

## 3. NaturalText、术语和 Placeholder

每个原文先执行 engine 对应的 Builtin Placeholder 和当前 Custom Placeholder，得到有序
opaque 段及 NaturalText 段。术语只逐段扫描 NaturalText，不扫描 opaque 外壳，不跨
OpaqueBoundary 或 Lines 元素拼接。

自定义 Placeholder 文件的解析或编译错误在任何单元规划前拒绝整份资源；规则已经成功
编译后，某个单元发生保护跨度冲突、占用 `⟦ATT_` 保留前缀或无法安全投影，只形成该单元
自己的 `planning-unresolved`。它不取得模型 ID、不产生 LLM attempt，也不发给模型；同轮
其他单元继续规划。该单元的旧译文不再 Current，并与本轮合法术语/Placeholder 资源一起
在请求模型前由 Preparation 原子失效和提交。

同一条术语的多个 trigger 命中多次也只输出一次；多个命中条目保持术语文件顺序。该有序
结果由 Prompt、Standard state 与 Lua `prepared.terms` 共用，不对 raw original 二次匹配。
术语中的 Markdown 字符由序列化器按字面转义。

去除 opaque 后没有任何非空白 NaturalText 时，单元是 `FullyProtected`；源语言模块判断无需翻译时是
`NonSourceLanguage`。二者均不请求模型。

## 4. 持久自然顺序、上下文与去重

Reader 只接受 Extract 已验证的顺序：owner 固定 Builtin、Rules、Lua；owner 内按连续
`group_order`；组内按连续 `unit_order`。Planner 不再按角色字符串、显示位置或完成时间
重新排序。并发可改变准备/响应完成时间，不能改变代表选择、任务 ID 或提交顺序。

一个语义组永不跨 TaskBlock 拆分。对话 Speaker/Body、完整 Choices、完整 ScrollingText
分别保持 Extract 的逻辑原子和 Lines 边界。

去重规则：

- Speaker 按完整源 Value 全局精确复用，不与 Body 去重；
- Body 包含完整有序 Lines 和该组源 Speaker 上下文；
- Choices 不跨 group 去重；
- ScrollingText 按完整有序 Lines 去重；
- Scalar 按来源语义域与字段角色去重，不把所有同名字段混为一类。

不 trim、不折叠大小写、不做 Unicode 或换行模糊匹配。重复集合使用自然顺序最早代表；
已有一个 Current 译文时复用，多个有效译文冲突时失败。recipe 的 Literal 外壳不是翻译
语义；仅外壳变化、而语义内容和上下文相同时可继承译文。

## 5. 任务规划与模型消息

Planner 先保持最大相关组，再按 Profile 的最终 messages 字符预算切 TaskBlock。单组超过
预算时明确失败，不切碎。活动单元从 1 连续编号。

user message 是最小 Markdown 载荷，只包含实际命中术语、活动 ID、自然语言角色、必要
形状约束和直接有用的无编号上下文。owner、路径、内部 kind、传播目标、去重原因和空区
不发送。已有/复用译文作上下文时优先目标文，没有时用源文。

<!-- att-example: illustrative -->
```markdown
术语：

- 星港 → 星港

## 对话

说话人 [1]（单行）：ミレア

正文 [2]（自由断行）：

> 潮風が強くなってきました。
> 灯台へ戻りましょう。

## 选项

选项 [3]（2 项，逐项对应）：

> 戻る
> 進む
```

Prompt 作者必须要求模型只翻译带 ID 内容、无 ID 内容只作语境、每个 ID 恰好返回一次、
遵守当前返回 wire、区分自由断行与严格对齐、精确保留 ATT token，且不输出解释文字。

公共 LLM 根提交 `model`、`messages`、`stream=false` 并透传 Client 的受信 parameters。
网络重试仅按 Profile 明确策略；协议失败不伪装成网络错误。

## 6. 响应结构与候选验收

Standard 响应必须提供当前 TaskBlock 的每个 ID 恰好一次，不能缺失、重复或增加 ID。
Value/Lines 形状必须符合角色：

- Speaker 和严格单行字段是一条非空候选；
- 自由断行 Value 可由多模型行组成，最终以 LF 连接；
- Choices 和严格 ScrollingText 必须与源 Lines 数量逐项对应并保持空槽；
- 每个结构行拒绝 CR、LF、NUL 和非法空白形状。

候选随后执行：BOM/全空白检查、ATT token 数量与对齐检查、占位符无歧义恢复、目标自然
语言检查、源语残留分析及当前语言模块允许的修复。失败仅拒绝相应候选并形成结构化
unresolved；不会伪造译文。

原文中相同占位符出现多次时，每个源位置仍是独立槽。**新**模型响应若无法无歧义恢复，
以 `placeholder_normalization_ambiguous` 拒绝。已经验收并持久化的译文按下一节 Current
规则处理，不重新执行这项反向正规化。

## 7. translation state 与 Current

state 是当前译文成立所依赖语义的 SHA-256 摘要。它绑定：

- engine、语言对、语言模块；
- 公共 Prompt 和影响模型语义的 Client 事实；
- kind、完整 original、Lines 边界与源上下文；
- 实际选中的 Placeholder 有序绑定；
- 实际有序术语命中；
- 最终已验收译文。

规则诊断编号、未命中术语/Placeholder、并发、重试、队列和完成时间不进入 state。
自定义 Placeholder 编号也不进入 token、label 或 state。因此插入、删除或重排未命中规则
不会重译；重排已命中术语会改变有序命中并失效。

项目中译文/state 成对存在，且重新计算的 state 精确相等时直接判为 Current：不重新请求
模型，也不把恢复后的旧译文再次送入候选正规化。这样重复相同占位符的合格译文第二次
运行为零 LLM。任何已绑定语义变化后，旧译文不再 Current。

Lua 暴露相同语义的 64 字符小写十六进制 state，但私有身份和成对事务由脚本负责，见
[Lua Translate API](lua.md#7-translatepreparecurrent-与-accept)。

## 8. 并发、提交与任务结果

`max_in_flight_tasks` 个 HTTP 消费者执行已经物化的有序 TaskBlock；完成响应立即进入有界
CPU 准备。产物仍落到计划 index 的独立槽，顺序 finalizer 只按 `0..n` 提交。

每个有写入的任务使用独立 SQLite 事务，验收通过的重复集合在同事务传播。技术失败停止
后续工作；已经提交任务保持。取消停止新请求，等待已接管工作到明确终态，不猜测外部
请求是否未发生。

最终：全部需要翻译的单元 Current 为 `Complete`；仍有可解释 unresolved 为 `Partial`；
没有可执行产出为 `Unavailable`。三者是业务结果，退出成功不等于 `Complete`。技术错误、
状态不一致或提交结果未知使用更强失败语义。

模型响应拒绝作为 task unresolved 记录；每个译前 Placeholder 投影失败单元各用一条独立
`translation_planning_unresolved` 审计事件记录逻辑位置和专用原因，不伪造 task ID、模型
attempt 或响应拒绝原因，也不把任意数量失败塞进一条无界记录。两者都计入最终
`remaining_decisions` 与 `remaining_locations`；
因此即使所有已发送任务都完成，只要仍有 planning-unresolved，本轮也不能解释为全部
Current。

该事件的 `payload.failure` 恰好描述一个单元，包含 `location` 和 `reason`；`reason.kind`
只有 `placeholder_protection`（匹配、保留前缀或保护跨度无法成立）和
`placeholder_projection`（已选 token 无法建立 NaturalText/opaque 投影），并携带非空
`message` 用于诊断。事件没有 `task_index`、`id`、`attempts` 或模型响应元数据。
