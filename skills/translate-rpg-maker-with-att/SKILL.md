---
name: translate-rpg-maker-with-att
description: 使用 ATT 调查、初始化、提取、翻译、审核、写回、续作和诊断 RPG Maker MV/MZ 汉化项目。用于创建、继续、修复、检查或验证 ATT 项目，编写 Extract Rules、术语、Placeholder、Prompt 或可信 Lua，以及把试玩反馈追溯到责任阶段；不用于开发 ATT Rust 源码或不经过 ATT 的普通翻译。
---

# 使用 ATT 汉化 RPG Maker

## 工作边界

把本 Skill 作为 ATT 汉化任务的状态路由。只处理通过 ATT 完成 RPG Maker MV/MZ 汉化的
工作；遇到 ATT Rust 源码开发、架构设计或不经过 ATT 的普通翻译时停止使用。

把现行文档视为产品事实、接口契约和验证方法的唯一来源。不要凭记忆补写命令、参数、
字段、数据库结构、错误语义或协议。文档、实现和测试冲突时，把冲突作为 ATT 项目缺陷，
不要临时发明兼容解释。

涉及日志、诊断或任务记录的内容边界时，读取
[Chat Completions 敏感信息权威规格](../../docs/runtime/chat-completions.md#6-敏感信息闭集唯一权威)，
不要在项目工件或调查结论中扩大或复述其闭集。

先确认使用者授权的文件、网络、模型调用、数据库和发布副作用。默认只读诊断项目数据库；
只有使用者明确要求高级修复时，才在建立可恢复副本后直接写受管表，并使用事务、在提交
前后复核现行不变量。

## 每次触发

1. 明确本次要成立的可观察结果、游戏根、项目工作区和副作用边界。
2. 只读检查游戏、配置、工作区、已保存运行方案、候选输出和最近诊断，区分持久化事实、
   可发现事实和必须由使用者选择的事实。
3. 找出阻止结果成立的最早责任状态，按下表读取该状态的必读资料。
4. 只为当前状态及其直接依赖建立短任务清单；不要机械重跑已经成立的阶段。
5. 执行前先取得真实载体与现行契约；缺少结构证据时先采证，不用占位名称或猜测语法。
6. 用项目状态、权威工件、诊断和副作用终态判断完成、继续、恢复或返回更早状态。

已授权端到端汉化时，在证据允许的范围内自动推进。只有缺少权威事实、需要扩大成本或
范围、或者将产生不可恢复或终态不明的副作用时暂停。

## 状态路由与必读资料

| 当前状态 | 必读资料 |
|---|---|
| 阶段不明、已有项目续作 | [文档总入口](../../docs/README.md)、[运行时导航](../../docs/runtime/README.md)；涉及命令终态或保存方案时再读[命令行规格](../../docs/runtime/cli.md) |
| 游戏调查、初始化或冻结来源受质疑 | [RPG Maker 调查指南](../../docs/rpg-maker/README.md)、[Init 规格](../../docs/rpg-maker/init.md)；创建或修正配置时再读[配置规格](../../docs/runtime/configuration.md) |
| 漏收、误收、错组、recipe 或 Claim 问题 | [Extract 规格](../../docs/rpg-maker/extraction.md)；使用声明式规则时再读[Rules 规格](../../docs/rpg-maker/rules.md)，高级只读数据库调查时再读[SQLite 规格](../../docs/runtime/sqlite.md) |
| 术语、Placeholder、Prompt、Profile、Client 或语言资源问题 | [术语规格](../../docs/rpg-maker/terminology.md)、[Prompt 规格](../../docs/rpg-maker/prompts.md)、[配置规格](../../docs/runtime/configuration.md)；按问题再读[Rules 规格](../../docs/rpg-maker/rules.md)与[Translate 规格](../../docs/rpg-maker/translation.md) |
| 翻译执行、续作或质量审核 | [Translate 规格](../../docs/rpg-maker/translation.md)；Standard 单任务调查读[任务记录规格](../../docs/rpg-maker/task-records.md)，运行级调查读[项目日志规格](../../docs/runtime/project-log.md)，HTTP 外层问题读[Chat Completions 规格](../../docs/runtime/chat-completions.md) |
| 写回、候选差异或隔离试玩 | [WriteBack 规格](../../docs/rpg-maker/write-back.md)；发布失败或终态不明时再读[目录发布规格](../../docs/runtime/directory-publishing.md) |
| 失败、取消、部分结果、状态矛盾或恢复 | [命令行规格](../../docs/runtime/cli.md)、[项目日志规格](../../docs/runtime/project-log.md)和失败阶段规格；按事实再读任务记录、SQLite、目录发布或 Chat Completions 规格 |
| 已证明需要可信 Lua | [Lua 技术参考](../../docs/rpg-maker/lua.md)、[Lua Cookbook](../../docs/rpg-maker/lua-cookbook.md)和对应阶段规格 |

## 判断、停止与恢复

- 用现实消费者、语义原子、物理载体和写回位置判断责任状态；命令成功或单个命中数量不能
  单独证明覆盖或完成。
- 首次采用或修改规则时，把它视为待验证假设。对可静态枚举的范围全量核对命中、未命中
  和误命中；无法枚举的范围明确保留为未证实。
- 只根据已观察问题修改规则、`config.toml`、术语或 Prompt。修改后从最早受影响状态重跑，
  并用真实结果决定是否保留。
- 对 Standard 单任务，任务记录用于核对最终输入、尝试、响应、验收与提交终态；项目
  JSONL 只用于运行级摘要，项目数据库承担业务状态。记录文件缺失不能证明请求未发生。
- 把 `Partial`、`Unavailable` 和已合法提交的部分进度按其现行语义处理，不把退出码单独
  当作完成证据。能够直接续作时复用权威状态继续，不重做已提交工作。
- 来源、引擎、游戏根或宽度事实错误时返回调查与初始化；漏收、误收、错组、recipe 或
  Claim 错误时返回 Extract；资源或语言质量问题返回最早受影响的资源或 Translate 状态。
- 只有证据表明继续 Translate 或调整现有工件已经不能显著推进，且剩余工作适合人工收尾
  时，才考虑可信 Lua。静态 Rules、Placeholder 或术语足以表达时不要升级机制。
- 使用可信 Lua 时保留扩展自己的协议、事务与私有状态边界，复用核心已经拥有的能力；
  不直接修改 ATT 受管翻译表，也不把一次模型响应当作最终提交证据。
- 发布或事务终态未知时保留现场并暂停同类副作用，不自动重试。取得恢复证据后，从规格
  指定的位置继续，并重新验证受影响不变量。

只有当前状态的权威事实、失败语义、剩余缺口和准确续作入口都明确时，才把该状态标记为
完成。候选内容树只是隔离试玩输入，不要把它误称为完整游戏包。
