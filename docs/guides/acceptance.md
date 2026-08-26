# ATT 全量验收指南

这份指南用于遗漏调查、人工或 agent 补译、质量返修、WriteBack 后检查和最终交付。验收
以任务声明范围、每个 ATT 项目、全部输出和实际消费者为对象；抽样只能帮助发现问题，
不能证明全范围正确。

## 1. 先固定验收范围

验收前记录：

- 游戏版本、补丁、MOD 与实际运行组合；
- 声明包含和排除的文本来源；
- 每类内容的唯一 MV、MZ 或 Generic 项目所有者；
- 每个项目的当前输入、语言、Extract 选择和翻译资源；
- 每个输出怎样部署、转换和被实际消费者读取；
- 用户要求的质量、布局、场景和平台范围。

“全量”至少表示：声明范围内每个来源位置、每个项目的全部 Group 与 Unit、全部输出文件、
全部源语残留和全部实际消费路径都进入检查。无法执行的部分标为未验证，不能写成已完成。

## 2. 建立来源到消费者的完整台账

对每类文本记录：

```text
真实来源
→ Builtin / Rules / 外部 JSONL 转换
→ ATT 项目与 Unit
→ ATT WriteBack 输出
→ 外部反向转换或部署
→ 实际消费者
```

MV/MZ 内容必须按[翻译项目指南](translation-project.md#31-mvmz-必须先走原生能力判断)
重新核对 [Extract 覆盖矩阵](../rpg-maker/extraction.md)和 [Rules 能力](../rpg-maker/rules.md)。
具体来源边界只由这两份规格说明。

组合项目还要执行两项集合检查：

- 声明范围中的每项内容恰好有一个所有者；
- 每个项目实际处理的内容都在声明范围内，没有跨项目重复写回。

## 3. 发行、输入与项目身份

确认实际 `att.exe`、配置、Prompt、文档和 Skill 属于同一发行目录，并按
[发行物规格](../runtime/distribution.md)检查所需资源。每个项目的引擎、名称、来源、语言、
最近成功 Extract 与任务台账一致。

输入、项目 schema、SQLite 提交或目录发布状态不明时，停止验收，先走
[诊断与恢复指南](diagnosis-and-recovery.md)。

## 4. Extract 范围验收

### MV/MZ

对完整位置清单逐项确认：

- Builtin 覆盖与排除符合 [Extract 规格](../rpg-maker/extraction.md)；
- 每份 Rules 的真实来源、命中、路径、捕获、Group、自然顺序和写回 recipe 正确；
- ownership 导出中的每个 Manual 条目都有唯一 owner；Rules 的自然 rule number 与审核时
  保存的 manifest、当前 Rules TOML 和 inventory 来源逐条一致；
- Rules 的所有跳过警告逐项有结论；
- Builtin 与 Rules 没有 Mutation Claim 冲突、遗漏或重复归属；
- 需要 Placeholder 保护的控制符和协议壳已经纳入当前资源。

图片、音频、视频、字体、RPG Maker 加密资源和整值资源路径应当作为资源引用排除，而不是
作为可翻译文本或 `allowed_terms` 接受。自然句中出现资源扩展名时仍按完整语境判断，不能
用后缀过滤整句。

### Generic

对所有外部来源逐项确认：

- 每个来源位置映射到唯一 JSONL 文件、Group 和 Unit；
- 必须共同解释的文本没有被拆开；
- 文件与 Group 保持真实自然顺序；
- 稳定 ID、kind、文本和写回位置可以往返追溯；
- 当前 JSONL 与最近成功 Extract 一致；
- 译后 JSONL 的外部消费过程覆盖全部文件与 Unit。

只检查最大文件、几个 Group 或几个页面不能代替上述集合检查。

## 5. 全部 Unit 与翻译状态

ATT 当前没有独立 `status` 命令。需要完整枚举当前 Unit、有效译文、来源和过期人工快照时，
使用高级 `ctx.translation.list()`；需要 Group 语境时，把全部待查可读 ID 合并到一次
`ctx.translation.context(ids)`。普通验收不读取 raw schema，也不使用内部位置字段。

数据库中译文非空不能单独证明它可以发布。自动正文先要求 V2 状态与当前源文、完整实际
Group 来源语境、项目语言对、位置、角色和写回结构精确匹配，再由消费入口独立执行当前
Placeholder 与结构强验收；人工正文另绑定写入时的语言对，但不绑定兄弟语境。Prompt、
Profile、Client、术语和语言检查阈值只影响后续请求或质量审查，不能用来否定或删除既有
V2 身份。

验收当前状态时使用 `translation export`、当前 Extract 事实和实际 WriteBack 输出交叉核对，
不为“重判”构造新 Client 或发出模型请求。来源、语言对、项目或实际输出之间缺少可核对
绑定时，结果必须为 `unverified`，不能从不透明 `translation_state`、旧任务记录或手工报告推导。

数据库中没有译文的 Unit 是“待人工分类候选”，不必然等于应该翻译：空白、没有源语
自然文本或完全受 Placeholder 保护的内容也可以没有译文。对每个候选按当前语言、
Placeholder 和来源语境分类：

- 应翻译但尚未有 Current；
- 按规格不适用并保留原文；
- 来源、语境或资源错误，需要返回更早阶段；
- 无法判断，需要补充真实消费者证据。

完成验收要求没有未解释候选，也没有未处理的 Partial、Unavailable、失败任务或并发变化。
任务记录中的临时数字 ID 不能代替 Manual 或高级 Lua 的可读 ID。

## 6. 人工或 agent 补译与精确修订

适用情况包括：

- 自动 Translate 反复没有新增进度；
- 确定性 JSON、ID、形状、Placeholder 或语言验收问题无法由同一模型稳定解决；
- 少量或批量剩余内容由人工或 agent 直接翻译；
- 已有 Current 需要审校修正；
- 相同原文在特定语境需要不同译文。

处理顺序：

1. 读取 [Manual 规格](../manual/README.md)和当前引擎 Translate、语言、术语、Placeholder
   规格。译后 QA 已定位条目时，先运行 `translation_qa.py manual` 生成自然 ID JSONL，再用
   `manual export --ids` 导出预填当前译文的 TOML；普通待译项使用默认 pending 导出。
2. 读取项目术语；含义明确的条目直接补译。
3. 收集全部含义不明的可读 ID，在一次 Lua 脚本中调用 `ctx.translation.context(ids)`，不要
   为每条译文分别启动 Lua，也不要从旧任务临时 ID 猜位置。
4. 在完整语境中填写 TOML，检查术语、角色、控制符、形状和源语残留。
5. 默认直接运行 `manual apply`；apply 会在一个事务内执行与 `manual check` 相同的结构检查，
   失败时不修改任何条目。需要事先试检或单独诊断 TOML 时才先运行 `manual check`。
6. apply 成功后重新检查全部受影响 Group，并对修改范围重新执行本指南第 5 至第 9 节。

Manual 不检查语言质量、术语、文风、语境或源语残留，这些仍由验收承担。复杂筛选、计算
生成或批量变换可以使用 Lua 高级 API；Raw `ctx.db` 能绕过全部保护并破坏数据库，不是普通
补译步骤。agent 执行的补译与人工补译使用同一套责任和证据要求。通常先集中处理一轮静态问题；
返修轮次不设硬限制，用户实机检查或其他新证据发现问题时按第 9 节再执行下一轮。

## 7. 译文质量与全量静态检查

对全部声明译文检查：

- 含义、上下文、人物关系、人称、语气和叙事一致性；
- 专名、技能、物品、地点、系统用语与当前术语要求；
- 数量、数字、标点、换行、空槽和数组形状；
- Placeholder、RPG Maker 控制符、插件协议和不可改写片段；
- 同原文不同语境与同角色连续对话；
- 目标语自然度、UI 长度和可读性；
- 源语言残留、拒绝语、模型说明、JSON 痕迹和异常转义。

标准流程使用 `translation_qa.py scan` 一次读取 `translation export`、survey、coverage、coverage
同目录的逐规则 manifest、术语、
可选的 ATT 当前实际 `write_back` 目录以及 NW.js 运行记录，生成 `qa-summary.json`、完整机器明细 `findings.jsonl` 和
紧凑的 `review-groups.jsonl`。Agent 先读 Review 组及少量样例，只有决定处理某组时才按
`review_group_id` 查看对应明细；不得把数千条启发式命中逐项读完后再分类。`manual` 默认只
输出确定问题的自然 ID，使用一个或多个 `--review-group` 才加入已审核的启发式组。状态只取
`clean`、`needs_review` 或 `unverified`；问题数量不会使该工具非零退出，也不会把结构合法
译文改成 Rejected。
不存在独立的 WriteBack preview 报告契约；QA 必须读取真实输出并核对其与 survey、coverage、
translation export 和运行观察的来源关系。任一必要关系无法证明时，没有静态 finding 也只能是
`unverified`。
Coverage 不能自行证明 Rules Unit。QA 必须从 Survey 位置、实际 Rule recipe 和 pattern 重新投影
`manual_id`、原文、类型、控制契约与所有权，并核对 Rules disposition 的全部候选都有唯一实际
Rule 消费；空投影或只改 coverage 的伪所有权属于输入不一致。
NW.js 场景证据还必须包含完整观察 hooks 和安装轮询、严格递增的完整事件与可重算子集、每个场景
独立的序列边界和语义匹配绘制，以及足以审核文本的可解码截图；同时核对隔离副本中未写回的 Survey
基线字节与实际 WriteBack 部署字节。PID、布尔标签、空事件、空 hooks 或极小图片本身都不是运行
证据。结构和字节一致性也不能证明报告无法伪造，因此执行记录不得声称工具无法提供的绝对来源。

源语残留扫描必须覆盖数据库当前译文、全部 WriteBack 输出和外部转换后的最终文件。扫描
命中逐项分类，不能因某些合法专名或协议片段存在就整体忽略。扫描无命中也不能替代语义
审校。
Generic 有精确 recipe 时按实际 Unit 输出核对原文序列，并区分译文已有残留与 WriteBack 新引入
残留。RPG Maker 尚无完整 Unit 写回 recipe 时，只能把同一输出文件内出现的源文精确文字序列列为
启发式 Review，并继续保留 Unit 映射未验证；不得把文件级命中冒充精确 Unit 结论。

## 8. WriteBack、实际消费者与组合项目

### 8.1 WriteBack 输出

从当前输入和当前项目状态重新 WriteBack，并对所有输出检查：

- 文件集合、完整差异和结构符合预期；
- 未声明位置没有变化；
- 所有保留原文逐项有解释；
- 输出能被对应生产解析器完整读取；
- 译后 QA 的布局、控制符、源语残留和未验证场景已经按自然 ID 处理或明确接受；
- 发布终态明确，没有未处理的 candidate、backup、journal 或结果未知。

WriteBack 成功不表示 Translate 完成；Partial 输出可能合法保留源文，但这种输出只有在任务
明确接受并记录全部保留项时才可交付。

### 8.2 MV/MZ 实际加载

把 `write_back` 内容按真实部署方式放入隔离游戏副本，使用与玩家一致的运行入口。对全部
受影响数据文件、事件、插件参数、菜单、战斗、地图和脚本路径建立场景清单，确认游戏实际
读取了交付内容，且没有解析错误、控制符破坏、截断、溢出或错误覆盖。

### 8.3 Generic 实际消费

让任务中确定的外部工具消费全部译后 JSONL，核对每个来源位置的写回结果，再由最终游戏
或文本系统实际读取。只验证 ATT JSONL 可解析，不能证明外部转换或游戏结果正确。

### 8.4 组合项目

各项目分别 WriteBack 后，按真实部署顺序合并。全量检查：

- 项目之间没有遗漏、重复翻译、互相覆盖或顺序错误；
- MV/MZ Rules 能处理的内容没有被无必要地留在外部 Generic 转换中；
- 外部转换不会覆盖 MV/MZ WriteBack，反之亦然；
- 合并后的完整游戏通过源语残留扫描和实际加载。

## 9. 返修后的重新验收范围

| 最早改变的事实 | 必须重新执行 |
| --- | --- |
| 来源、项目归属、Rules、JSONL 分组或稳定身份 | Extract 及其后全部验收 |
| 项目语言对、源文、Group 语境或 Placeholder 强不变量 | 重新确认译文适用性；非 Current 正文不发布，新结果原子替换后再做全部下游验收 |
| 术语、Prompt、Profile、Client 或语言检查阈值 | 不改变既有 Current；重新审查受影响的质量要求，并将新选择用于后续实际模型请求 |
| Manual、Lua 或其他译文修订 | 受影响 Group、全量语言/Placeholder/质量扫描、WriteBack 与实际加载 |
| WriteBack recipe、布局或外部转换 | 完整输出、残留、转换和实际消费者验收 |
| 部署方式或消费者版本 | 全部实际消费与组合检查 |
| SQLite 或发布状态不明 | 先诊断与恢复，状态明确前不继续验收 |

## 10. 完成记录

完成记录至少包含：

- 每个项目的实际发行、输入、资源和最终权威状态；
- Init、Extract、Translate、Lua 和 WriteBack 的相关 RunId，以及使用过的 Manual TOML；
- 全量来源台账与跨项目所有权结果；
- Unit 候选分类、自动翻译范围以及人工或 agent 修订范围；
- 全量源语残留、结构、Placeholder 和输出差异检查的范围、数量与结果；
- 外部转换、部署、实际消费者和场景清单的结果；
- 仍需人工执行的步骤、未验证内容与剩余风险。

声明范围内仍有未解释遗漏、未译候选、质量问题、恢复现场、结果未知或未验证消费者路径
时，任务不能标记完整完成。
