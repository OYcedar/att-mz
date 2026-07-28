# 五阶段门禁

本文件把完整交付保持在同一全局视角中。创建整体方案时完整读取；进入某阶段前重新读取
该阶段、共同循环和失效传播。

## 目录

1. [共同循环](#1-共同循环)
2. [解包](#2-解包)
3. [提取](#3-提取)
4. [翻译](#4-翻译)
5. [写回](#5-写回)
6. [封包](#6-封包)
7. [跨阶段失效传播](#7-跨阶段失效传播)
8. [权威文档路由](#8-权威文档路由)

## 1. 共同循环

每阶段：

1. 读取账本和最新项目事实；
2. 完成 `XXX-002`，核实输入、适用性、Owner、依赖、授权与全局影响；
3. 从 `XXX-004` 起建立或调整项目专属 TODO；
4. 执行最小必要工作，每项结束立即更新账本；
5. 用权威项目证据完成 `XXX-003`；
6. 更新 `XXX-001`、阶段总览、下游 TODO 和恢复入口；
7. 发现上游根因时返回最早责任阶段。

固定顺序是文档和依赖骨架，不是单向流水线。固定门不可取消、替代或标为不适用。

## 2. 解包

### 宏观责任

建立完整、稳定、可复现的游戏来源，并完成 ATT Init。固定五阶段不另设 Init，因此把 Init
作为解包的最后交接门。

### 进入核实

- 原始发行版本、补丁与 MOD 的实际叠加顺序；
- 启动器真正加载的目录、包或归档；
- 来源是 loose tree 还是需外部工具处理的包；
- 不可变原件与允许修改的工作副本；
- 引擎、源/目标语言、三类宽度和输出目标；
- 用户是否授权外部解包、工作副本和 Init。

### 执行

- 使用真实包格式对应的外部工具；ATT 不提供通用解包命令。
- 输出到新路径，不覆盖唯一原件。
- 用目录清单、哈希或等价证据核实完整性和真实加载根。
- 按 [Init 现行规格](../../../docs/rpg-maker/init.md)完成 Init，核实来源指纹和终态。

### 结果验收

- 来源身份和加载链已证实；
- 展开结果完整且可重现；
- 后续读取的是已核实来源；
- Init 为当前来源建立了权威状态；
- 恢复入口与不可变原件明确。

版本、根目录、加载顺序、来源或 Init 终态不明时阻断全部下游。

## 3. 提取

### 宏观责任

完整识别文本载体，为每类文本选择正确 Owner 和机制，建立稳定、可重复、可逆的提取与
写回身份。

### 进入核实

- 解包来源和 Init 状态仍为 current；
- Builtin 数据库字段、事件、插件参数、自定义 JSON/JS、注释和其他真实文本载体；
- Builtin、Rules、Lua Owner 的明确选择及替换/停用语义；
- 每类载体的运行时消费者和写回位置。

### 执行

- 按 [机制选择指南](../../../docs/rpg-maker/README.md)选择 Builtin、Rules 或 Lua。
- 按 [Extract 现行规格](../../../docs/rpg-maker/extraction.md)处理 Owner 顺序、快照、
  grouping、自然顺序、recipe 和 Mutation Claim。
- Lua 单位可独立翻译时优先 Managed；低级 Lua 只作有明确理由的逃生路径。
- 对命中、未命中、误命中、排除项和高风险载体建立证据。

### 结果验收

- 高风险文本载体已覆盖或有证据排除；
- kind、shape、context、分组、顺序、身份和映射准确；
- Managed collection、key、metadata 与来源映射稳定；
- 重复 Extract 收敛；
- 未翻译内容的 Extract—WriteBack 往返不修改无关数据；
- Owner 快照和下游交接均为 current。

来源问题返回解包。覆盖、身份、分组、recipe、Claim 或映射问题留在提取修复。任何提取
语义变化都会使受影响翻译、写回和封包结论失效。

## 4. 翻译

### 宏观责任

基于 current 提取快照，通过 Standard、Managed 或显式低级路径得到经协议验收并确认提交
的译文。

### 进入核实

- Extract 和 Managed manifest current；
- Profile、Client、Prompt、源/目标语言、术语、Placeholder 和 Lua 选择明确；
- 成本与模型选择已获授权；
- Current 基线、剩余工作和不可适用状态明确；
- 共享翻译资源的全部 Owner 与 kind 消费者已识别。

### 执行

- 按 [Translate 现行规格](../../../docs/rpg-maker/translation.md)先 Standard，后 Lua。
- Managed 由 ATT 负责全局去重、任务协议、并发、重试、验收、state、增量提交和记录。
- 低级 Lua 必须显式拥有自己的协议、并发、事务和恢复。
- 只重试非 Current 且仍可恢复的工作；连续无进展时停止，根据结构化原因返回最早阶段。
- 人工 Standard 候选通过 `ctx.standard`，继续经过普通验收与提交。

### 结果验收

- 分开记录 `Complete`、`Partial`、`Unavailable`、技术失败、取消和提交终态；
- 以 Current、剩余 decision/location、拒绝原因和确认提交判断，不以退出码或请求成功判断；
- 任务范围内单位均为 Current，或有合法、已核实的不可适用/保护结论；
- 无未解释的剩余、协议拒绝、`Unavailable` 或终态不明；
- 术语、Placeholder、语言和关键样本质量已验证。

来源、kind、shape、context 问题返回提取。Prompt、Client、语言、术语或 Placeholder 改变
重开翻译及下游验收。

## 5. 写回

### 宏观责任

从冻结来源和 current 翻译状态可重复构建候选，完成 Lua 私有映射，并以明确终态单次发布。

### 进入核实

- 翻译满足输出政策；部分翻译只有在用户明确授权时可作为预览；
- 来源、Owner、manifest 和写回映射 current；
- 每类译文的目标位置明确；
- 不存在尚未核实的事务或发布终态。

### 执行

- 按 [WriteBack 现行规格](../../../docs/rpg-maker/write-back.md)每次从冻结 `source`
  重建，不在旧 `write_back` 上累积修补。
- 执行 Standard WriteBack，再执行 Lua WriteBack。
- Managed Lua 用 key/metadata 读取已提交单位并幂等映射目标。
- 完整验证候选后只进行一次目录发布。

### 结果验收

- 对完整目录核实允许变更集和结构，覆盖 JSON、JS、路径、编码与插件；
- 重复构建得到等价结果或有证据解释差异；
- 在隔离副本实际启动，检查关键 UI、事件、插件、布局和文本；
- 候选与发布终态明确，运行方案保存状态明确。

候选目录存在不等于完成。stale 翻译返回翻译；recipe、Claim、身份或映射问题返回提取；
来源问题返回解包。发布 `OutcomeUnknown` 时保留现场，按
[目录发布规格](../../../docs/runtime/directory-publishing.md)恢复，不盲目重试。

## 6. 封包

### 宏观责任

把已验收候选转化为用户要求的完整包、补丁或 loose tree，并证明运行时实际加载该交付物。

### 进入核实

- 用户要完整包、补丁还是 loose tree；
- 原始发行结构、候选身份、启动器加载顺序、归档/加密格式和运行时依赖；
- 外部封包工具、参数和权限；
- 组装是否会覆盖原件或候选。

### 执行

- 从新暂存目录按原始发行结构组装；
- 按真实补丁/MOD 顺序应用已验收候选；
- 使用格式对应的外部工具；ATT 不提供通用封包命令；
- 输出到新位置，不覆盖唯一原件。

### 结果验收

- 记录交付物清单、大小和哈希；
- 反向解包或等价验证包内内容与候选一致；
- 在干净环境从最终交付物启动；
- 验证实际加载路径、关键文本、插件和必要存档流程；
- 交付物可复现、可运行且身份明确。

用户明确要求并验收 loose tree 时，可按账本契约把封包判为 `N/A`。包格式和组装问题留在
封包；其他问题返回对应最早阶段。

## 7. 跨阶段失效传播

| 新事实或变化 | 返回阶段 | 至少重验 |
|---|---|---|
| 版本、来源、加载根、补丁/MOD 顺序 | 解包 | 全部下游 |
| 载体、Owner、kind、shape、context、分组、recipe、Claim、身份、映射 | 提取 | 翻译、写回、封包 |
| Profile、Prompt、Client、语言、术语、Placeholder、候选验收 | 翻译 | 写回、封包 |
| 写回目标、候选结构、布局、发布字节 | 写回 | 封包 |
| 包格式、组装、最终加载链 | 封包 | 封包自身 |

不要抹掉旧完成记录。追加变更、标记其当前适用性、创建重验 TODO，并只重做权威状态不再
证明有效的工作。

## 8. 权威文档路由

需要给出命令或判断外部行为时，按问题读取：

| 主题 | 现行规格 |
|---|---|
| 文档权重与导航 | [docs/README.md](../../../docs/README.md) |
| CLI、运行方案、退出与取消 | [runtime/cli.md](../../../docs/runtime/cli.md) |
| 配置与用户选择 | [runtime/configuration.md](../../../docs/runtime/configuration.md) |
| Init | [rpg-maker/init.md](../../../docs/rpg-maker/init.md) |
| Extract | [rpg-maker/extraction.md](../../../docs/rpg-maker/extraction.md) |
| Rules 与 Placeholder | [rpg-maker/rules.md](../../../docs/rpg-maker/rules.md) |
| 术语 | [rpg-maker/terminology.md](../../../docs/rpg-maker/terminology.md) |
| Prompt 与模型协议 | [rpg-maker/prompts.md](../../../docs/rpg-maker/prompts.md) |
| Translate | [rpg-maker/translation.md](../../../docs/rpg-maker/translation.md) |
| Lua | [rpg-maker/lua.md](../../../docs/rpg-maker/lua.md) |
| WriteBack | [rpg-maker/write-back.md](../../../docs/rpg-maker/write-back.md) |
| task-records | [rpg-maker/task-records.md](../../../docs/rpg-maker/task-records.md) |
| 项目日志 | [runtime/project-log.md](../../../docs/runtime/project-log.md) |
| SQLite 与 Managed 权威状态 | [runtime/sqlite.md](../../../docs/runtime/sqlite.md) |
| 发布恢复 | [runtime/directory-publishing.md](../../../docs/runtime/directory-publishing.md) |
| Client、重试与敏感信息 | [runtime/chat-completions.md](../../../docs/runtime/chat-completions.md) |

具体字段、空定义、消息格式、状态和错误只从对应现行规格读取，不在 Skill 中维护第二套定义。
