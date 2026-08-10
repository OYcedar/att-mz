# ATT 翻译项目指南

这份指南把真实游戏内容组织成一个可执行、可检查的 ATT 任务。它负责调查顺序、项目
选择和阶段衔接；命令参数、格式、状态变化和失败语义由链接的现行规格负责。

遇到失败、不完整结果、取消或状态不明时，转到[诊断与恢复指南](diagnosis-and-recovery.md)。
进入审校、补译、WriteBack 后检查或交付时，转到[全量验收指南](acceptance.md)。

## 1. 先定义最终结果与操作范围

在 Init 前写清：

- 游戏版本、补丁、DLC、MOD 和实际运行组合；
- 源语言、目标语言、包含范围和明确排除范围；
- 玩家最终运行或读取的消费者，以及输出怎样进入消费者；
- 当前发行已经配置的模型与外部服务，以及用户明确指定的新值；
- 用户只要求只读调查，还是要求完成或修复翻译；
- 最终需要哪些静态检查、实际加载和人工场景检查。

“完成”或“修复”ATT 翻译已经包含在 ATT 项目工作区中建项、Extract、调用当前配置的
模型、用 Manual TOML 补齐或修订译文和生成 WriteBack 输出，除非用户明确排除，不得为
每条命令再次询问。需要补充语境时可以用 Lua 一次批量读取相关上下文。改 Endpoint、
Model、parameters 或凭据，覆盖游戏原件，写入外部系统，或执行任务没有要求的破坏性
操作，仍需用户提供明确新值或授权。

长期、跨会话或多人任务按[任务材料规范](task-artifacts.md)维护唯一任务清单。清单记录
任务事实和证据，不代替 ATT 数据库、日志、任务记录或发布状态。

## 2. 建立完整文本位置清单

不要从文件扩展名或某次搜索结果直接推导项目方案。先调查运行时实际读取的内容，并对
声明范围内每类玩家可见文本记录：

- 精确来源文件、对象、字段、事件参数、插件参数或脚本位置；
- 游戏在什么条件下读取它；
- 同一条内容需要一起理解的上下文和自然顺序；
- 原值怎样安全替换，译文怎样写回；
- 控制符、标识符、资源名、协议壳和不可改写部分；
- 它与补丁、MOD、其他项目或外部转换的覆盖关系。

位置清单必须覆盖全部声明范围。搜索和抽样可以帮助发现来源，不能证明没有遗漏。
完成一次 Survey（对本次游戏来源的一次性调查）后，如果仍不知道哪个程序、插件或场景会读取某段
文本，必须在第 3 节的所有权决定前使用隔离游戏副本做运行观察。无法访问的消费者保持待调查，不得
先分配所有者再在 Translate 后返工。

标准图片、音频、视频、字体和 RPG Maker 加密资源字段，以及整值资源路径，是资源引用，
不是玩家文本。它们不进入 Rules 或术语候选，也不能写入 `allowed_terms` 掩盖错误提取。
自然句中出现 `.png` 等扩展名不因此成为资源引用；`.txt`、`.json` 和 `.js` 也只是容器
类型，不能按后缀排除其中的可见文本。补丁说明或其他根目录文本只能先记为候选；必须确认
当前游戏实际读取并显示后，才继续判断所有者。

## 3. 逐类选择唯一项目所有者

### 3.1 MV/MZ 必须先走原生能力判断

只要文本来自 MV/MZ 游戏，先完整读取 [MV/MZ Extract](../rpg-maker/extraction.md)和
[Rules](../rpg-maker/rules.md)，按以下顺序判断：

1. **Builtin**：逐项核对 Extract 规格的精确覆盖矩阵。
2. **Extract Rules**：Builtin 未覆盖时，继续核对 Rules 是否能从已知来源建立确定、
   可逆的读取与写回。
3. **Generic**：默认不建立。只有某个具体来源同时满足本节后述的运行时可见性、Rules
   表达边界、外部往返和唯一所有权条件时，才把该来源交给外部转换和独立 Generic 项目。

Extract Rules 当前能够处理的来源类别包括：

- 已知 RPG Maker 数据 JSON 文件或 `Map*.json` 中的确定路径；
- `js/plugins.js` 中实际启用插件的参数对象；
- Map、CommonEvents 和 Troops 中指定 `code + parameter` 的事件参数；
- 路径终点的完整字符串，或由一个 `text` 捕获确定的局部字符串；
- 规格允许的逐层 JSON 字符串解码、数组遍历和可逆 recipe。

因此，插件参数、`note` 标签、自定义 JSON 字段、指定事件 command、嵌套 JSON 字符串或
需要 PCRE2 捕获的复杂文本，都不能只因 Builtin 没覆盖或编写规则麻烦就改判 Generic。

常见 MZ 位置应这样判断；表中只说明能力选择，实际规则字段、路径和失败条件仍以
[Rules 完整规格](../rpg-maker/rules.md)为准：

| 实际位置 | 首选能力与边界 |
| --- | --- |
| `MapInfos.json[*].name` | Builtin 不读取，但 Rules 可用 `file + path` 完整读写 |
| `Items.json`、`Map*.json` 等数据文件的 `note` 标签值 | Rules 可用确定路径和单个 `text` 捕获保留标签外壳 |
| `js/plugins.js` 中已启用插件的 `parameters` | Rules 的 `plugin + path`；路径可逐层解码嵌套 JSON string |
| 单条 `code = 357` 的指定参数及其中确定字段 | Rules 的 `code + parameter + path` |
| 单条 `code = 355` 参数内边界确定、可逆的一个字面片段 | Rules 的 `code + parameter + pattern` |
| 相邻 `355/655` 组成的完整脚本、跨 command 关系或多目标同步 | Rules 无法表达，使用外部转换和 Generic |
| `js/plugins/*.js` 插件源码中的任意静态界面字面量 | Rules 不读取插件源码，使用外部转换和 Generic |

Rules 不能猜测可见性，也不能表达动态对象键枚举、条件筛选、跨文档关系、一个译文同步
写入多个目标、插件 JavaScript 源码内任意静态字面量或规格未定义的路径行为。只有实际
需求落在这些边界外，或无法形成确定且可逆的读取与写回时，才使用 Generic。最终判断以
Rules 规格的完整字段、路径、冲突和失败条件为准。

Generic 的启用单位是一个已经确认的具体来源，不是文件扩展名、目录或整类插件。该来源
必须同时具备以下事实：

- 位于游戏目录且不是图片文字，有精确自然位置；
- 当前游戏实际加载对应消费者，并有证据证明游玩过程中会向玩家显示；
- Builtin 没有覆盖，Rules 也无法完整、确定、可逆地读取和写回；
- 外部转换已经确定提取、Group/Unit、稳定 ID、译后写回和来源变化处理；
- 与 MV/MZ 和其他 Generic 项目没有重复所有权。

只看到文件、源码引用、插件复杂度或疑似界面字符串，都不能作为启用依据。缺少任一事实时
保持 Generic 关闭，并把来源记为明确排除或尚待调查；尚待调查的来源存在时不能宣称文本
范围已经完整。一个来源通过时也只纳入该来源，不递归翻译整个 `.txt`、`.js` 或游戏目录。

事件 command 也要按实际结构判断：Rules 可以处理单条 command 的指定参数与其中的确定
捕获，但不会把相邻 `355/655` 或多条 command 合成一个 JavaScript 脚本块。需要跨命令理解
或重写完整脚本时使用外部转换，不能因 `code` 可选择就宣称 Rules 已覆盖整个脚本。

### 3.2 Generic 的责任

Generic 只接收符合 [JSONL 规格](../generic/jsonl.md)的输入。外部操作者或工具必须同时
负责：

- 从真实来源生成每个文件、Group、Unit、kind 和稳定 ID；
- 保留完整语境、自然顺序和来源到 JSONL 的逐项映射；
- 消费译后 JSONL，并把译文准确写回实际游戏或文本系统；
- 在来源变化时更新 JSONL，再按 Generic Extract 规格同步项目。

Generic 不是“任何复杂文本”的默认项目，也不是 MV/MZ 项目的隐藏补丁层。

### 3.3 组合项目

同一游戏可以有一个 MV/MZ 项目和一个或多个 Generic 项目。为每类文本记录：

| 事实 | 必须明确的内容 |
| --- | --- |
| 唯一所有者 | 具体 MV、MZ 或 Generic 项目 |
| 输入 | 游戏来源或外部 JSONL 映射 |
| 提取能力 | Builtin、哪份 Rules，或外部转换 |
| 输出 | 对应 WriteBack 结果 |
| 消费 | 部署、外部反向转换和最终加载顺序 |

一段文本不得由两个项目重复拥有；没有项目负责的内容必须明确排除或补上所有者。

## 4. 绑定实际 ATT 发行

项目首次执行前：

1. 确认实际 `att.exe`、版本、发行目录和调用 cwd；
2. 读取同目录 `README.md`、`docs/README.md` 与本阶段规格；
3. 按[发行物规格](../runtime/distribution.md)确认当前命令需要的固定资源；
4. 按[配置规格](../runtime/configuration.md)确认确实需要的外部选择。

发行资源缺失或互相不一致时停止，不从源码、其他安装或任务材料中拼接替代文件。

## 5. 建立或更新项目

### MV/MZ

按 [MV/MZ Init 规格](../rpg-maker/init.md)建立项目和冻结来源。记录实际引擎、项目名、
来源和语言选择。来源游戏变化时，由 Init 规格决定如何更新；不能直接修改 ATT
管理的来源副本或数据库。
游戏路径和语言已固定，且 Init 的来源复制不会与当前磁盘任务争用时，可以在 Agent 审核 Survey
关系组（按同一判断合并的紧凑候选）时并行 Init；否则按顺序执行。Extract 必须等两者都完成。

### Generic

按 [Generic Init 规格](../generic/init.md)绑定外部 JSONL 根和语言。外部 JSONL 仍由任务
操作者管理，ATT 项目只保存当前绑定与 Extract 状态。

Init 失败、取消或项目数据库无效时，不进入 Extract。MV/MZ 目录发布或 Generic 初始数据库
候选出现恢复现场或结果未知时也一样，转到
[诊断与恢复指南](diagnosis-and-recovery.md#62-init)。

## 6. 建立当前可翻译范围

### MV/MZ Extract

按位置清单选择 Builtin、MV dialogue rules 和 Extract Rules。MV 必须先调查实际姓名框协议，
并提供有效 dialogue rules 或明确的 `rule = []`；MZ 使用原生 Speaker，不制作 MV 姓名规则。
Extract Rules 也必须在调查后提供有效规则或明确的 `rule = []`，不能以未传文件代替已经确认
没有规则。执行后完整核对：

- 每个声明来源由正确 owner 提取；
- Group、Unit、上下文、自然顺序和写回 recipe 符合实际关系；
- Builtin 与 Rules 没有遗漏、重复所有权或 Mutation Claim 冲突；
- Rules 警告逐项得到解释；
- 项目保存的 owner 与资源就是本轮预期选择。

规则候选经过审核后，同时保存当前 Rules TOML 和逐条对应的自然 rule manifest。Extract
完成后，从同一项目快照成对导出 Manual 与 ownership JSONL，再对照 inventory、当前 Rules、
manifest 和来源审核决定检查唯一所有者。manifest 必须与当前 TOML 的自然规则序号和完整
规则定义逐条一致；一个规则可以产生多个 Manual 条目，但每个条目只能有一个 owner。不能
从 Manual ID 前缀推断 Builtin、Rules 或 Generic，也不能用任务数量推断覆盖率。
所有权审计（逐位置核对 ATT 实际提取所有者）完成后，把该 Manual 固定为本轮语料。后续如果改动
Extract 或所有权，必须废弃已生成的 Placeholder 与术语作业，不能混用旧产物。

每个关系组都必须有 `rules`、`generic`、`exclude` 或 `unresolved` 状态，但不要求把所有未知项都强行
确认。运行观察被禁止、场景无法到达或静态证据不足时，保留精确未确认项并继续处理已确认范围；不要
只为得到 `complete=true` 反复扫描或逐成员穷举大型关系组。

精确覆盖、继承、owner 独立提交和失败范围见
[MV/MZ Extract](../rpg-maker/extraction.md)与 [Rules](../rpg-maker/rules.md)。

### Generic Extract

先按 [JSONL 规格](../generic/jsonl.md)对声明范围建立完整来源映射，再按
[Generic Extract](../generic/extraction.md)同步项目。核对所有文件、Group 和 Unit，而非只
抽查几个代表项；每个稳定 ID 必须能够追溯到来源和最终写回位置。

提取结果有警告、部分 owner 已提交、输入读取期间变化或事务状态不明时，转到
[Extract 诊断](diagnosis-and-recovery.md#63-extract)。

## 7. 准备翻译资源

按实际任务完整读取[公共翻译入口](../translation/README.md)及适用专题：

- [语言](../translation/language.md)；
- [术语](../translation/terminology.md)；
- [Placeholder](../translation/placeholders.md)和 MV/MZ 的引擎补充规则；
- [TaskBlock 规划](../translation/task-planning.md)；
- [Prompt 与模型协议](../translation/prompts.md)；
- [模型任务记录](../translation/task-records.md)；
- [配置](../runtime/configuration.md)与 [Chat Completions](../runtime/chat-completions.md)。

最终 Extract 完成后、首次 Translate 前，先用 Manual export 取得本轮完整待译原文。调查
控制符并提供有效 Placeholder 文件或明确的 `rule = []`；再从同一份稳定原文制作术语表，
术语允许为空。姓名投影或 Extract Rules 改变后，旧 Manual 语料和据此生成的 Placeholder、
术语候选全部失效，必须重新导出。

所有权审计完成并固定 Manual 后，立即启动 Formic（从完整语料批量找术语候选的外部工具）。
在 Formic 网络等待期间，并行完成 Preflight（Translate 前的 Placeholder 候选检查）和已结束
ATT 命令的日志汇总。不读取仍在写入的日志。

上述工作分别使用调查、译前检查、术语和日志的专用 Python 助手。Agent 只用它们的显式
输入输出安排顺序；不让助手互相调用，也不新建通用流程框架。

`allowed_terms` 只用于玩家可见译文中确实允许保留的源语片段，不是资源名白名单。盘点中
发现的文件名、图片名、音频名、字体名、加密资源名和内部标识符不得机械写入该配置；如果
它们被提取为可翻译原文，先修正来源分类或规则。

术语内容的发现、筛选与定译和 ATT 术语文件接入是两件事。先确定实际术语要求，再按 ATT
规格建立输入。修改共享 Prompt、Client 或配置前，先确认全部实际使用者与用户给出的新值。

## 8. 执行 Translate

每个项目按自己的 Translate 规格执行：

- [MV/MZ Translate](../rpg-maker/translation.md)
- [Generic Translate](../generic/translation.md)

本轮结果明确且满足当前目标时进入验收。出现 Partial、Unavailable、任务失败、HTTP
问题、响应无效、资源失效、取消、日志降级或状态不明时，转到
[Translate 诊断](diagnosis-and-recovery.md#64-translate)。不能预设“无限重复 Translate”、
“立即换模型”或“全部改用 Lua”。少量剩余条目进入 Manual 流程，不得只为局部问题修改
全局 Placeholder、ignored terms、语言规则、Prompt 或术语并触发大范围重新翻译。

Translate 开始后持有项目租约，即防止同一项目两个 ATT 命令同时改状态的独占锁。模型等待期间不能对同一
项目执行 export、Manual 或 WriteBack；只能准备不读写该项目的隔离副本等独立工作。没有独立工作时就等待，
不启动并行命令试探租约。

## 9. 审校、补译与返修

按[全量验收指南](acceptance.md)检查全部声明范围。发现问题后回到最早失效的事实：

| 问题 | 返回位置 |
| --- | --- |
| 来源、项目归属、遗漏、重复拥有或写回映射错误 | 第 2、3、6 节，重新确定 Extract 输入 |
| Group、语境、自然顺序或稳定身份错误 | Extract |
| 语言、术语、Placeholder、Prompt、Profile 或自动译文质量问题 | Translate 准备或 Translate |
| 已经定位的单个或批量译文需要人工或 agent 直接修订 | 使用 [Manual TOML](../manual/README.md)，再按验收指南复查 |
| 候选内容、布局、外部转换、部署或实际加载错误 | WriteBack 或外部消费步骤 |

先对全部当前译文做一次静态 QA，即不启动游戏的译后检查；再把它生成的自然 ID 集中导出为一份
Manual。填写前主动读取并参考项目术语表；条目含义不明确时，把全部待查 ID 合并后用 Lua 一次批量
读取上下文，不得为每条译文分别启动脚本。未填写项可以留在 TOML 中，结构正确时不妨碍应用其他条目。

编辑完成后默认直接 `manual apply`。Apply 会在单个数据库事务内执行与 `manual check` 相同的结构检查；
发现任何错误时不修改任何条目。只在需要事先试检或单独诊断 TOML 时执行 `manual check`。静态 QA 发现的问题
通常先集中修改一轮，但轮次不是完成条件。Apply 成功后重新执行 Translation export 和静态 QA，确认本轮
修改结果后才执行 WriteBack；新证据或用户实机检查发现问题时可以再做第二轮。

Lua 仍可程序化写入人工译文，也能直接执行任意数据库修改，但普通人工补译不以 Lua 为
首选。只有复杂筛选、计算生成、批量变换、诊断或特殊修改更适合脚本时，才直接使用 Lua。
无法确认问题属于系统性规则缺陷时，完成 Manual 补译，或停止并报告无法确认的内容。

## 10. WriteBack 与外部集成

每个项目分别按对应规格从当前输入和当前项目状态生成输出：

- [MV/MZ WriteBack](../rpg-maker/write-back.md)
- [Generic WriteBack](../generic/write-back.md)

Generic 输出还必须由任务中已经确定的外部过程完整消费。组合项目按第 3.3 节记录的顺序
整合输出。候选验证、译后 QA、输入变化、发布恢复或结果未知转到
[WriteBack 与目录发布诊断](diagnosis-and-recovery.md#66-writeback-与目录发布)。

固定顺序是：静态 QA 后集中 Manual，再 WriteBack，把输出部署到隔离副本并完成运行观察，
最后用 Manual 后已经重新导出的 Translation export、WriteBack 验证报告和运行报告合并最终 QA。运行报告、
用户实机检查或返修后新出现的事实可以触发下一轮；不得为了避免第二轮而在译前穷举无法确认的内容。

## 11. 继续旧任务

1. 读取唯一任务清单及其证据引用；
2. 重新确认实际发行、配置、输入、项目数据库与输出身份；
3. 从数据库、当前输入状态和发布协议确定最后一个权威事实；
4. 状态明确时回到本指南最早失效的阶段；状态不明时先走诊断与恢复指南；
5. 不把旧日志、旧任务记录、目录存在或清单中的勾选当作当前权威状态。

## 12. 进入最终验收

完成所有项目 WriteBack 之后，仍需执行[全量验收指南](acceptance.md)。只有声明范围、每个
项目、全部输出、外部转换和实际消费者的要求都成立，任务才能标记完成。
