---
name: translate-with-att
description: 使用 ATT 规划、建立、继续、诊断、审校、Manual 补译、Lua 特殊修改、写回和验收 RPG Maker MV、MZ、Generic 或组合式游戏翻译。适用于用户明确使用 ATT、提供 ATT 项目，或要求处理项目选择、Init、Extract、Builtin、Rules、JSONL、Translate、语言、术语、Placeholder、Prompt、模型任务记录、Manual、Lua、WriteBack、运行错误、发布恢复和已有翻译任务续作。
---

# 使用 ATT 完成翻译任务

本 Skill 负责辨认当前任务、选择必读文档、得到必要的规则与验收结果。命令、
格式和错误语义以现行文档为准；随 Skill 的 Python 工具只减少重复扫描和临时脚本。

## 1. 绑定本次发行

1. 确认实际使用的 `att.exe`、它所在的发行目录和调用 cwd。
2. 始终完整读取该发行目录中的 `README.md` 与 `docs/README.md`。
3. 只使用该发行目录中的配置、Prompt、Skill 和文档解释这份程序。其他安装、源码、
   Git 历史、旧对话和模型记忆都不是本次产品事实。
4. 固定资源缺失或发行内容不一致时停止项目操作，读取发行物规格；不得从其他位置拼接。

## 2. 先确认当前事实

开始前确定：

- 用户要求只读调查，还是要求完成或修复翻译；后者已包含在 ATT 项目工作区中建项、调用
  当前配置的模型、用 Manual TOML 补译、按需用 Lua 批量读取上下文或执行复杂修改，以及
  生成 WriteBack 输出，除非用户明确排除；
- 游戏版本、实际输入、实际消费者和声明范围；
- 涉及 MV、MZ、Generic 中的哪些项目；
- 当前属于项目规划、Init、Extract、Translate、人工或 agent 修订、WriteBack、恢复还是验收；
- 实际观察到的结构化诊断、业务结果、权威状态与恢复现场。

长期、跨会话或多人任务按任务材料文档维护唯一清单；简单的一次性任务不强制建清单。

对互不写同一文件的只读扫描、运行记录分析和候选审查，主动使用 subagent 可以节省
时间。是否使用、分几项、由谁执行和先后顺序由当前 Agent 按游戏规模和依赖决定，
不建立固定角色或数量。重叠文件、ATT 项目和最终规则只交给一个写入者。

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
| 从完整游戏原文制作、补做或重做术语表 | `skills/extract-game-terminology/SKILL.md` |
| 语言、术语、Placeholder、TaskBlock、Prompt 或模型任务记录 | `docs/translation/README.md` 与对应专题规格 |
| CLI、配置或发行资源 | `docs/runtime/cli.md`、`docs/runtime/configuration.md`、`docs/runtime/distribution.md` 中适用的规格 |
| HTTP、超时、重试、代理、限速或模型服务 | `docs/runtime/chat-completions.md` |
| 数据库、事务、项目状态、锁或提交结果未知 | `docs/runtime/sqlite.md` |
| 日志、RunId、诊断呈现或记录失败 | `docs/runtime/project-log.md`；涉及模型请求时同时读模型任务记录规格 |
| 候选目录、原子发布、恢复现场或发布结果未知 | `docs/runtime/directory-publishing.md` |
| 人工或 agent 补译、定点修订 | `docs/manual/README.md`；需要上下文、复杂筛选或程序化修改时再读 `docs/lua/README.md` |
| 原始数据库查询、诊断或特殊修改 | `docs/lua/README.md` |

组合式游戏对每个项目分别选择文档；同一问题跨越多个责任方时，相关规格全部读取。

## 4. MV/MZ 的固定结果

常见 MV/MZ 翻译准备必须得到以下结果；Agent 可以并行调查，但不能跳过未确认项：

1. 识别实际内容根、MV/MZ、活动插件、Builtin 精确覆盖和所有其他文本来源。
2. MV 调查实际姓名框消费协议，最终保存有效规则或明确 `rule = []`；MZ 使用原生
   Speaker，不制作 MV 姓名规则。
3. 先确认 Builtin，再调查 Extract Rules，最终保存已验收规则或明确 `rule = []`。
4. 用最终 Extract 得到完整当前可翻译原文；从该语料调查 Placeholder，保存已验收规则或
   明确 `rule = []`。
5. 审计每个非 Builtin 来源的唯一所有者：Rules、已证实的 Generic、有依据的排除，或未确认。
   只要还有未确认来源，就不能声称提取覆盖完整。
6. 在首次 Translate 前，从这份最终完整原文制作术语表；没有需要约束的术语时保存合法空集。
7. Translate 后用项目日志的结构化计数和诊断判断 Complete/Partial/Unavailable；WriteBack 后检查
   输出结构、JSON 可读性和游戏原件未被修改。

## 5. Generic 默认关闭

默认不建立 Generic 项目。只对同时满足以下事实的一个精确来源启用 Generic：

1. 来源位于游戏目录内，有精确自然路径，且不是图片文字；
2. 当前游戏实际启用了读取它的运行时消费者；
3. 已经追到这份文字在真实游玩中向玩家显示的调用与条件；
4. Builtin 不拥有它；Extract Rules 也无法完整、确定、可逆地读取和写回；
5. 已确定外部提取、Group/Unit 和写回映射；
6. 它与 MV/MZ 项目不会重复提取或写回同一文本。

文件存在、插件多、代码中出现字符串、来源复杂或“可能显示”都不足以启用。一个来源
通过后只纳入该精确来源，不扩大为整个目录或文件类型。

## 6. 优先使用随包工具

先检查 Python 3.11+。可用时优先运行下表中对应的完整程序；它们只使用标准库，不联网、
不执行游戏 JavaScript、不读 SQLite、不修改游戏原件。缺少 Python 时使用已读规格手工完成，
不自动安装 Python。所有输出必须用显式路径；默认拒绝覆盖，确认可替换时才传 `--replace`。

| 任务 | 程序 |
| --- | --- |
| 内容根、插件、Builtin、活动插件脚本、间接加载代码和其他文本来源盘点 | `scripts/inspect_rpg_maker.py` |
| MV 姓名 marker 统计与审核后规则 | `scripts/analyze_mv_dialogue.py` |
| JSON/嵌套 JSON/插件/事件参数候选与审核后 Rules | `scripts/analyze_extract_rules.py` |
| 最终 Manual 语料中的 Placeholder 候选、跨行与重叠风险 | `scripts/analyze_placeholders.py` |
| 指定外部来源的活动消费者和显示调用证据 | `scripts/trace_runtime_text.py` |
| 对照最终 Manual 的每个文本来源唯一所有者审计 | `scripts/audit_text_ownership.py`（必须传 `--manual`） |
| JSONL 阶段、任务结果、Partial/Unavailable 和规划失败汇总 | `scripts/summarize_att_run.py` |
| WriteBack 前关键源文件工作副本与写回后源文件/输出验证 | `scripts/verify_write_back.py` |

每个程序用 `--help` 给出当前参数。候选 JSON 保存机械事实；只有传入 Agent 审核 JSON 时，
对应程序才写最终 TOML。Python 不代替 ATT 的严格 TOML、PCRE2、冲突、状态和往返验收。

不得因为内容复杂、位于插件相关文件、数量多或需要正则，就直接选择 Generic。具体能力
边界只由 MV/MZ Extract 与 Rules 规格判断。

## 7. 按文档推进

1. 用当前目标和权威状态确定下一项操作。
2. 按用户目标判断授权范围；不得把完成或修复任务中的正常 ATT 操作再次拆开询问。
3. 使用所选文档规定的项目内处理办法；操作后重新观察实际结果和权威状态。
4. 根据新事实继续当前阶段、返回最早失效阶段、进入修订、转入恢复或开始验收。
5. 文档已有项目内办法且处于上述操作范围时直接执行，不把执行选择重新丢给用户。

WriteBack 会对 Generic、MV 和 MZ 的自动译文执行全局符号修复，人工译文不参加；不需要
也不接受语言专用引号配置。判断输出差异时读取对应 WriteBack 与语言规格；修复器内部无法
安全判断只会保留原译文，不能用这一行为解释或忽略数据库、Placeholder、布局、候选验证
和发布错误。

Translate 为 Partial 或 Unavailable 时，读取同次运行的自然语言诊断和任务记录。能定位到
具体条目时使用 Manual 或 Lua 返回的可读 ID；任务整体失败时本来没有唯一条目，不使用
临时 ID 或重复原文猜位置。

英语项目需要允许专名、按键名、协议词或单个字母原样保留时，读取语言规格并配置
`allowed_terms`；
它只免除译后 `source_residual`，不改变译前判断和临时 ID 分配。只有确实应从译前判断中排除
的内容才使用 `ignored_terms`，不能混用两者。

重跑命令、改配置、重建项目、重新 Extract、Lua 和人工或 agent 补译都不是万能步骤。
只有当前原因对应的文档允许时才采用。改 Endpoint、Model、parameters 或凭据，覆盖游戏
原件，写入外部系统，或执行用户未要求的破坏性操作，都不属于默认范围。只有确实缺少
外部材料、明确的新值、会改变结果的用户选择或新授权时才询问用户。

若现行文档没有覆盖已经观察到的状态，停止推测和写入，报告事实、已读文档与缺失说明。

## 8. Manual 补译

少量未完成、已经定位的局部问题默认使用 Manual TOML：

1. 运行对应项目的 `manual export`。
2. 读取项目术语；含义明确的条目直接补译。
3. 收集全部含义不明确的 ID，在一次 Lua 脚本中调用 `ctx.translation.context(ids)` 批量读取
   上下文；不得为每条译文分别启动一次 Lua。
4. 填写 TOML，运行 `manual check`。
5. 只修正结构、原文、控制码和 Placeholder 错误；不要因为残留英文、译文等于原文或术语
   偏好而把结构有效条目判成失败。
6. 检查通过后运行 `manual apply`，再继续 WriteBack 和验收。

TOML 不保存上下文。需要复杂筛选、计算生成、批量变换或特殊数据库修改时使用 Lua 高级
API；只有任务明确需要低级数据库操作时才使用 `ctx.db`。原始数据库 API 没有 ATT 业务保护，
能够删除表、破坏关系或使项目不可用，不能把它当作普通补译步骤。

少量剩余条目不得仅为局部问题修改全局 Placeholder、ignored terms 或语言规则并触发大规模
Translate。没有证据证明是系统性规则缺陷时，完成 Manual 补译；无法确认正确译文时停止并
报告缺少的事实。

## 9. 完成

按 `docs/guides/acceptance.md` 对声明范围、每个项目、全部输出和实际消费者完成全量验收。
进程成功、一次 Complete、单条日志、任务记录、输出目录存在或抽样检查都不能单独证明
整个翻译任务完成。

只读任务交付事实、证据、未确认内容和执行所需授权；执行任务还要记录实际修改、输出、
验证结果以及仍未解决的问题。
