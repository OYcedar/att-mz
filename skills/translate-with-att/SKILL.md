---
name: translate-with-att
description: 使用 ATT 规划、建立、继续、诊断、审校、修订、写回和验收 RPG Maker MV、MZ、Generic 或组合式游戏翻译。适用于用户明确使用 ATT、提供 ATT 项目，或要求处理项目选择、Init、Extract、Builtin、Rules、JSONL、Translate、语言、术语、Placeholder、Prompt、模型任务记录、Lua、WriteBack、运行错误、发布恢复和已有翻译任务续作。
---

# 使用 ATT 完成翻译任务

本 Skill 只负责辨认当前任务、选择必读文档和控制执行顺序。命令、参数、格式、状态、
处理办法和验收要求全部在文档中，不在这里猜测或复述。

## 1. 绑定本次发行

1. 确认实际使用的 `att.exe`、它所在的发行目录和调用 cwd。
2. 始终完整读取该发行目录中的 `README.md` 与 `docs/README.md`。
3. 只使用该发行目录中的配置、Prompt、Skill 和文档解释这份程序。其他安装、源码、
   Git 历史、旧对话和模型记忆都不是本次产品事实。
4. 固定资源缺失或发行内容不一致时停止项目操作，读取发行物规格；不得从其他位置拼接。

## 2. 先确认当前事实

开始前确定：

- 用户要求只读调查，还是要求完成或修复翻译；后者已包含在 ATT 项目工作区中建项、调用
  当前配置的模型、用 Lua 修订译文和生成 WriteBack 输出，除非用户明确排除；
- 游戏版本、实际输入、实际消费者和声明范围；
- 涉及 MV、MZ、Generic 中的哪些项目；
- 当前属于项目规划、Init、Extract、Translate、人工或 agent 修订、WriteBack、恢复还是验收；
- 实际观察到的结构化诊断、业务结果、权威状态与恢复现场。

长期、跨会话或多人任务按任务材料文档维护唯一清单；简单的一次性任务不强制建清单。

## 3. 按事实完整读取

先读总入口，再完整读取下表中全部适用文档。项目、阶段、错误或目标变化后重新选择；
不能沿用上一阶段的处理办法。

| 当前任务 | 必读文档 |
| --- | --- |
| 新建、规划或继续完整翻译 | `docs/guides/translation-project.md` |
| 失败、不完整、取消、状态不明或命令未达预期 | `docs/guides/diagnosis-and-recovery.md` |
| 遗漏调查、人工或 agent 修订、质量返修、WriteBack 后检查或最终交付 | `docs/guides/acceptance.md` |
| 长期任务或多人协作 | `docs/guides/task-artifacts.md`；需要新清单时再读 `docs/guides/task-list-template.md` |
| MV/MZ 项目或当前阶段 | `docs/rpg-maker/README.md` 与当前阶段规格 |
| MV/MZ 范围判断、项目分配或 Extract | `docs/rpg-maker/extraction.md` 与 `docs/rpg-maker/rules.md` |
| Generic 项目或当前阶段 | `docs/generic/README.md` 与当前阶段规格 |
| Generic 范围判断、JSONL 制作或 Extract | `docs/generic/jsonl.md` 与 `docs/generic/extraction.md` |
| 语言、术语、Placeholder、TaskBlock、Prompt 或模型任务记录 | `docs/translation/README.md` 与对应专题规格 |
| CLI、配置或发行资源 | `docs/runtime/cli.md`、`docs/runtime/configuration.md`、`docs/runtime/distribution.md` 中适用的规格 |
| HTTP、超时、重试、代理、限速或模型服务 | `docs/runtime/chat-completions.md` |
| 数据库、事务、项目状态、锁或提交结果未知 | `docs/runtime/sqlite.md` |
| 日志、RunId、诊断呈现或记录失败 | `docs/runtime/project-log.md`；涉及模型请求时同时读模型任务记录规格 |
| 候选目录、原子发布、恢复现场或发布结果未知 | `docs/runtime/directory-publishing.md` |
| 查询 Unit、人工或 agent 精确修订译文、批量数据库操作 | `docs/lua/README.md` |

组合式游戏对每个项目分别选择文档；同一问题跨越多个责任方时，相关规格全部读取。

## 4. MV/MZ 先判断原生能力

不能把“Builtin 没覆盖”直接判成 Generic。只要文本来自 MV/MZ 游戏，分配项目之前必须：

1. 按 MV/MZ Extract 规格核对 Builtin 的精确覆盖；
2. 按 Rules 规格核对已知数据文件、路径、插件参数、事件参数、局部字符串捕获和可逆写回；
3. 只有实际来源或关系无法由 Builtin 与 Rules 完整表达时，才为那部分内容建立外部转换和
   独立 Generic 项目；
4. 记录每类文本的唯一项目所有者，防止遗漏、重复翻译或重复写回。

不得因为内容复杂、位于插件相关文件、数量多或需要正则，就直接选择 Generic。具体能力
边界只由 MV/MZ Extract 与 Rules 规格判断。

## 5. 按文档推进

1. 用当前目标和权威状态确定下一项操作。
2. 按用户目标判断授权范围；不得把完成或修复任务中的正常 ATT 操作再次拆开询问。
3. 使用所选文档规定的项目内处理办法；操作后重新观察实际结果和权威状态。
4. 根据新事实继续当前阶段、返回最早失效阶段、进入修订、转入恢复或开始验收。
5. 文档已有项目内办法且处于上述操作范围时直接执行，不把执行选择重新丢给用户。

重跑命令、改配置、重建项目、重新 Extract、Lua 和人工或 agent 补译都不是万能步骤。
只有当前原因对应的文档允许时才采用。改 Endpoint、Model、parameters 或凭据，覆盖游戏
原件，写入外部系统，或执行用户未要求的破坏性操作，都不属于默认范围。只有确实缺少
外部材料、明确的新值、会改变结果的用户选择或新授权时才询问用户。

若现行文档没有覆盖已经观察到的状态，停止推测和写入，报告事实、已读文档与缺失说明。

## 6. 完成

按 `docs/guides/acceptance.md` 对声明范围、每个项目、全部输出和实际消费者完成全量验收。
进程成功、一次 Complete、单条日志、任务记录、输出目录存在或抽样检查都不能单独证明
整个翻译任务完成。

只读任务交付事实、证据、未确认内容和执行所需授权；执行任务还要记录实际修改、输出、
验证结果以及仍未解决的问题。
