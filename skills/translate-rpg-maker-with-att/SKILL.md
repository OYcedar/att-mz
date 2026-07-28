---
name: translate-rpg-maker-with-att
description: 通过唯一实体任务账本规划、执行、恢复和审计 ATT 的 RPG Maker MV/MZ 翻译交付，强制覆盖解包、提取、翻译、写回、封包以及全局配置变更验证。用于新建、继续、修复、诊断或验收通过 ATT 完成的游戏翻译项目；不用于开发 ATT Rust 源码、设计 ATT 架构或不经 ATT 的普通翻译。
---

# 用 ATT 交付 RPG Maker 翻译

## 核心契约

把一个由执行 Agent 亲自创建并持续维护的实体 Markdown 任务账本作为执行控制面。先取得
项目全局视角和最佳宏观方案，再创建账本；创建前只做只读勘察，不修改游戏、ATT 项目、
配置或输出。

始终保留并核实五个阶段：

1. 解包
2. 提取
3. 翻译
4. 写回
5. 封包

固定的是五阶段、进入核实门、结果验收门和证据责任。项目专属 TODO 必须随实际发现动态
增补、拆分、替代或重排。每完成一项工作立即更新账本，禁止结束时集中补写。

## 事实来源

先完整读取 [ATT 文档入口](../../docs/README.md)。把标为“现行规格”的文档作为外部契约，
把指南作为调查和机制选择方法；不要凭记忆发明命令、参数、字段、状态、数据库结构或
发布语义。

日志、数据库、文件、哈希、候选和最终产物说明项目实际发生了什么。任务账本说明 Agent
如何理解、计划、执行、调整和验收。账本是 Agent 行为与责任的权威窗口，不是 ATT 项目
业务状态数据库。两类证据不一致时，先以对应语义所有者的项目事实核清，再更新账本。

## 强制启动流程

1. 完整读取 [账本契约](references/ledger-contract.md) 和
   [全局规划与变更控制](references/planning-and-change-control.md)。
2. 只读调查用户目标、游戏实际加载根、发行结构、ATT 配置与项目状态、翻译资源、现有
   Owner、候选和输出；按 [五阶段门禁](references/stage-gates.md) 建立跨阶段视角。
3. 在稳定的操作者任务根中搜索同一 ATT 项目的未完成账本：
   - 存在时，完整读取并与真实项目状态对账后继续；
   - 不存在时，先形成当前证据支持的最佳整体方案，再由主 Agent 亲自从
   [任务账本模板](assets/task-ledger-template.md) 创建唯一实体文件。
4. 一次建立五阶段宏观责任及固定门，只写当前证据支持的具体 TODO。
5. 从本 `SKILL.md` 所在目录解析脚本绝对路径并运行只读校验器，不要相对游戏或任务根
   猜测脚本位置：

   ```powershell
   uv run --no-project python "<本 Skill 目录>/scripts/validate_ledger.py" "<账本绝对路径>"
   ```

6. 只有账本已存在、结构有效、当前写操作有 TODO 且进入门已通过，才能开始项目修改。

不得把聊天内计划、内置计划工具、`task-records`、项目日志、子 Agent 报告或第二份 JSON
当作账本替代品。

## 执行控制循环

每次采取实质行动前：

1. 读取账本的整体方案、当前恢复入口、相关阶段和有效变更。
2. 确认行动已有稳定 TODO、完成条件、授权边界和所需证据。
3. 进入阶段前重新读取 [五阶段门禁](references/stage-gates.md) 中该阶段及跨阶段传播规则，
   并完成该阶段进入核实门。
4. 执行最小必要动作。
5. 立即记录真实结果、证据定位、剩余责任和恢复入口。
6. 用项目事实完成结果验收门；发现上游根因时回到最早责任阶段，并为所有受影响的下游
   结论建立重验 TODO。

命令退出 `0`、进度 `N/N`、请求成功、日志存在、候选目录存在或 `task-records` 完整都不能
单独证明阶段完成。区分 Agent 责任状态与 ATT 执行结果；按账本契约处理 `Partial`、
`Unavailable`、失败、取消和 `OutcomeUnknown`。

## 全局修改门

在授权的执行、修复或交付任务中，可以自主修改范围内、可恢复且有证据支持的 Rules、
Placeholder、术语、Prompt 等项目语义资源。只要求分析、解释或建议时保持只读。

每次修改前，按 [全局规划与变更控制](references/planning-and-change-control.md) 在账本中：

- 记录根因、语义所有者、全部现实消费者、作用范围和受影响状态；
- 比较候选方案的保留行为、损失和验证成本；
- 建立修改 TODO、修改前验证 TODO 和修改后验证 TODO；
- 对完整受影响集合做差分或等价的全量验证；
- 使已经失效的下游完成声明退出当前完成判断。

不得自主改变 API key、模型或成本选择、目标语言、外部服务、用户交付范围或不可恢复
副作用。缺少这些选择时暂停并请求用户决定。

修改 Placeholder 时，不得只验证报错样本。按真实 kind 覆盖 Standard Builtin、Rules、
Lua Standard、Managed 以及低级 `translation.prepare` 的实际输入，核对修改前后保护跨度、
新增与丢失命中、Custom/Custom 和 Custom/Builtin 重叠。具体语法和 scope 只从
[Rules 现行规格](../../docs/rpg-maker/rules.md)读取。

## Lua 路由

先判断 Builtin、Rules 或 Standard 是否已准确表达责任。Lua 负责私有发现与映射、翻译单位
可独立表达时，默认使用 Managed 高级接口：

- Extract：`ctx.translations.replace`
- Translate：`ctx.translations.translate`，成功后可 `open`
- WriteBack：`ctx.translations.open` 后由 Lua 明确映射目标

Managed 让 ATT 承担去重、任务协议、并发、重试、验收、state、增量提交和 task-records，
但不会自动修改游戏资产。

只有私有 grammar、跨 unit 原子关系、特殊模型协议或自定义恢复状态确实无法由 Managed
表达时，才显式使用 `ctx.translation`、`ctx.llm`、`ctx.db` 等低级接口。此时在账本中写清
协议所有者、身份、事务、并发、幂等、三阶段交接和恢复；ATT 不自动降级，也不为低级
`ctx.llm` 提供 Managed 的任务记录或协议保证。采用 Lua 前完整读取
[Lua 技术参考](../../docs/rpg-maker/lua.md)；需要实现范例时再读
[Lua Cookbook](../../docs/rpg-maker/lua-cookbook.md)。

## 恢复与多 Agent

上下文压缩、中断、续作或 Agent 更换后，先完整重读账本并核对项目状态。项目事实与账本
不一致时追加变更记录，不静默改写历史。发布或事务终态未知时保留现场、停止同类副作用，
先按现行规格核清终态。

主 Agent 是账本唯一写入者。委派前先建立 TODO ID；子 Agent 只能返回该 ID 对应的观察、
实际改动、证据定位、结果、不确定性和建议重开项。主 Agent 核验真实载体后亲自更新账本，
不能把子 Agent 的“完成”自述直接记为 `DONE`。

## 完成与交付

交付前运行：

```powershell
uv run --no-project python "<本 Skill 目录>/scripts/validate_ledger.py" "<账本绝对路径>" --final
```

只有五阶段当前责任均为 `DONE` 或经核实的 `N/A`、阶段总览所指的最新门全部 `DONE`、
无开放责任、
所有配置修改完成全局验证，并且用户成功条件与最终产物都有当前有效项目证据时，才能在
账本中写“完成”并向用户宣告交付。

最终回复从账本汇总改动范围、验证结果、产物、未验证部分和范围外风险，不依赖临时记忆。
