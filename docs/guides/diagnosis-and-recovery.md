# ATT 诊断与恢复指南

这份指南用于继续旧任务，或处理失败、Partial、Unavailable、取消、警告、恢复现场和
结果未知。它负责把观察到的事实交给正确规格，并选出安全的下一步；具体状态与操作仍以
对应现行规格为准。

## 1. 不先重跑，先确定发生了什么

先收集并记录：

- 实际 `att.exe`、发行目录、版本、SHA-256 和调用 cwd；
- 引擎、项目、命令、参数与 RunId；
- 退出码、终端完整诊断和 `run.finished` 终态；
- 当前阶段的业务结果、警告、状态影响与恢复位置；
- 当前项目日志和可用的模型任务记录；
- 输入、配置、Prompt、Rules、术语、Placeholder 或 Lua 的身份；
- ATT 管理的项目数据库、输出和发布现场是否仍在原位。

项目日志和任务记录是诊断证据，不是提交、译文或发布的权威。日志缺失不证明操作没有
发生；目录存在不证明发布成功；退出码 `0` 也不证明整个翻译已经完成。

当前 CLI 没有独立的 `status` 命令。需要完整检查 Unit 与译文时，按 [Lua 规格](../lua/README.md)
使用当前数据库查询入口；Lua 是可写命令，即使脚本只查询也会取得写租约。完成或修复翻译
的任务已包含这项项目内操作；只读调查未获写入授权时，只能报告现有诊断能够证明的事实，
不能把日志推断成数据库状态。

## 2. 用三个维度分类

每次问题同时记录三个维度，不能只写“失败”或“没翻完”。

### 2.1 进程终态

- `succeeded`
- `failed`
- `cancelled`
- `recovery_required`
- `outcome_unknown`

终态定义见 [CLI](../runtime/cli.md)与[项目日志](../runtime/project-log.md)。

### 2.2 当前阶段的业务结果

例如 Init 是否建立项目、Extract 哪些 owner 提交、Translate 的 `translation.finished` 是
Complete、Incomplete 还是其他终态、各 Task 是 Partial 或 Unavailable、Lua 事务是否提交、
WriteBack 是否发布及是否要求人工布局。

### 2.3 状态影响

- `unchanged`：状态未改变；
- `progress_preserved`：前序进度已提交；
- `applied`：本次操作已经生效；
- `applied_run_plan_not_saved`：业务已经生效，但 RunPlan 未保存；
- `applied_finalization_failed`：业务已经生效，但收尾失败；
- `recovery_required`：状态明确且必须保留或处理恢复现场；
- `outcome_unknown`：是否生效无法确认。

只有这三个维度都明确，才能决定继续、修正后重做、人工或 agent 修订、处理恢复现场，
还是停止写入。

## 3. 通用处理顺序

1. 读取 occurrence 的 `scope` 和 `report`；检查 `effect`、主诊断的 `code`、`stage`、具体
   `issue`、类型化 `resolution`，以及递归 `related`。
2. 完整读取拥有该事实的阶段规格，以及诊断涉及的公共或运行时规格。
3. 从规格指定的权威位置重新观察当前状态；不要解析自由文本猜测。
4. 确定最早失效的事实和项目内可用操作。
5. 授权足够时执行该操作，随后重新观察；需要新的外部值或授权时才询问用户。
6. 权威状态重新明确后，返回[翻译项目指南](translation-project.md)的相应阶段。

禁止手工编辑 ATT 数据库，或删除、移动 ATT 管理的项目、candidate、stage、backup、
journal、日志和任务记录。只有 [Lua](../lua/README.md) 明确提供的事务入口可以修改数据库。

## 4. 按进程终态判断

### 4.1 `succeeded`

这只说明本次命令产生了明确业务结果。继续读取本阶段结果：Translate 仍可能是 Partial 或
Unavailable，Extract 可能带警告，WriteBack 可能保留原文或要求人工布局。处理完这些事实
后才能进入下一阶段。

### 4.2 `failed`

读取主错误、相关错误和状态影响。明确回滚或未改变时，修正具体原因后可按阶段规格重做；
若已有 Translate 前序提交、发布已生效，或日志、任务记录、终端呈现失败，则先确认已经
生效的部分，不能把
整个运行当作未发生。

`run.finished` 只汇总进程结果并引用主 occurrence。只要该 occurrence 的主 report 或任一
递归 related report 的 `effect = "recovery_required"`，即使 `run.finished` 是 `failed`，
仍必须按第 4.4 节处理具体 issue 中列出的全部恢复产物；不能只按进程终态选择恢复办法。

### 4.3 `cancelled`

取消会停止新工作，但已经进入提交或发布边界的操作会完成到明确结果。Translate 已确认
的前序进度会保留，已经形成的模型任务记录会完成收尾。重新执行前先按项目日志和数据库
确认本次留下的状态，再只处理剩余工作。

### 4.4 `recovery_required`

业务状态已经明确，但诊断列出的现场必须保留。若这是目录发布恢复，读取
[目录发布规格](../runtime/directory-publishing.md)。从 `publication.finished` 引用的
`diagnostic.publication` occurrence 读取完整 report；Publication issue 直接保存
`output_root`、`candidate_root`、`residual_path` 或 `recovery_artifacts`，嵌套 backend
diagnostic 保存具体文件系统问题。只有恢复产物是该目标受管的
`.directory-publish-*.(stage|backup|journal)`，且与同一 `output_root` 匹配时，才保持项目、
目标、输入和恢复产物不变并继续判断。

`recovery_artifacts` 列出与同一操作匹配的 backup/journal 或可清理 stage，且主 report 与
全部 related report 都没有 `filesystem.journal_corrupt`、目标与已知旧目录均缺失或缺少
必要 backup 时，先修正实际文件系统问题，再执行一次相应 MV/MZ Init 或 WriteBack；MV/MZ
Init 会在从 `project.db` 复用省略的游戏路径、读取项目状态和继承设置之前恢复，WriteBack
会在建立新候选之前恢复。I/O 问题读取 `filesystem.io` issue 的
`context.operation`、`problem.failure.kind` 和 `problem.failure.raw_os_code`；其他问题按
稳定 code 与封闭 `problem.kind` 分流。

自动恢复本身报告 `filesystem.journal_corrupt`、目标与已知旧目录均缺失、缺少必要
backup，或一次恢复后仍得到 `effect = "recovery_required"` 时，现行接口没有修复入口。
保留新的完整 occurrence 和具体 issue 中的全部路径，停止重跑；不自行删除、改名或移动。

SQLite 恢复仍只按 [SQLite 规格](../runtime/sqlite.md)处理，不能把目录恢复方法套到数据库。
Generic 初始数据库候选和 WriteBack scratch 也不属于目录发布器，分别按第 6.2、6.6 节
处理。当前发行没有通用 `recover` 或 `status` 子命令；不得只看到进程终态就套用一种办法。
一次运行的主诊断和相关诊断可以列出多种恢复路径，必须逐项分类并全部处理；一种路径已经
恢复，不能证明同一运行的其他残留也已恢复。

### 4.5 `outcome_unknown`

这表示提交、目录交换或进程异常后无法证明操作是否生效。立即停止同一项目或目标上的
重跑和写入，保留数据库、sidecar、候选、backup、journal 与日志。只允许进行不会改变
现场的观察；无法通过现行公开接口确认时，按上一节报告能力缺口。

### 4.6 日志、任务记录或终端呈现失败

这些失败不会自动改写已经确定的业务结果。根据仍可用的数据库、目录发布终态和终端诊断
单独确认业务是否生效。不得因为任务记录缺失而重发模型请求，也不得因为进程返回 `1` 就
假定数据库没有提交。

## 5. 按失败来源选择处理办法

| 失败来源 | 必读规格 | 项目内处理方向 |
| --- | --- | --- |
| 发行资源、配置或 Prompt 缺失 | [发行物](../runtime/distribution.md)、[配置](../runtime/configuration.md) | 停止项目操作；恢复同一发行包的完整资源，不从其他安装拼接 |
| 输入、Rules、JSONL 或翻译资源无效 | 对应格式与阶段规格 | 修正实际输入，回到最早读取它的阶段 |
| HTTP 传输、限速或超时 | [Chat Completions](../runtime/chat-completions.md) | 先区分运行时有限重试是否已经耗尽；只有适用的暂时性原因才再次运行 Translate |
| Endpoint、Model、凭据、额度或 parameters | [配置](../runtime/configuration.md)、[Chat Completions](../runtime/chat-completions.md) | 不为试探而擅自换值；任务必须继续模型路径时才取得用户给出的精确新值，否则可按 Translate 分支转入 agent Lua 修订 |
| 模型 JSON、ID、形状、Placeholder 或语言验收 | [Prompt](../translation/prompts.md)、对应 Translate、[任务记录](../translation/task-records.md) | 保留已提交项；判断应重跑、修正资源，还是由人工或 agent 修订 |
| SQLite、事务、租约或 sidecar | [SQLite](../runtime/sqlite.md) | 明确回滚后修正原因再做；结果未知时停止写入 |
| 候选、暂存、目录交换或恢复文件 | 当前 Init/WriteBack 规格与本指南第 4.4、6.6 节；恢复路径属于 `.directory-publish-*` 时再读[目录发布](../runtime/directory-publishing.md) | 遍历主诊断与相关诊断，按 operation 和每条恢复路径分别处理；不得把一种恢复办法套给全部现场 |
| 译文遗漏或质量问题 | [全量验收](acceptance.md)、[Lua](../lua/README.md) | 先定位责任阶段，再自动重做或由人工或 agent 原子修订 |

## 6. 按阶段处理

### 6.1 启动、配置与发行

Help 与 Version 之外的命令使用实际 `att.exe` 同目录的固定发行资源。若二进制、配置、
Prompt、文档、Skill 或许可集合不完整，按[发行物规格](../runtime/distribution.md)修复发行包；
项目和任务材料不能充当发行资源。

配置解析、语言、Profile 或 Client 选择错误按[配置规格](../runtime/configuration.md)处理。
当前任务确需修改外部选择时，必须先取得用户给出的精确新值；不能为试探而擅自更换。

### 6.2 Init

完整读取对应 [MV/MZ Init](../rpg-maker/init.md)或 [Generic Init](../generic/init.md)，再按诊断
区分输入检查、旧项目保持与数据库事务；MV/MZ Init 还要处理目录发布，Generic Init 则处理
初始数据库候选。

MV、MZ 或 Generic 项目的当前 schema 无效时，现行产品都不迁移或覆盖原项目；在同一发行
下使用新的项目名重新 Init，再从当前真实来源执行 Extract。MV/MZ 从游戏根重新建立冻结
来源，Generic 从外部 JSONL 根重建；旧译文不会自动复制，是否保留必须在项目外审查后按
当前项目能力重新翻译或精确修订。

若结构化 operation 是 `cleanup_generic_initial_database_candidate`，恢复路径是 Generic
工作区内的 `.project.db.init-*.tmp` 或它的 `-journal` / `-wal` / `-shm` SQLite sidecar。
ATT 没有清理这类残留的公开入口，也不会在下一次 Init 自动处理它。保留旧路径并报告能力
限制；使用新项目名重新 Init + Extract 可以继续翻译，但旧残留只有在操作者核实诊断路径
并明确授权外部删除后才能处理，不能把它算作已恢复。

只有[目录发布规格的 MV/MZ Init OS 5](../runtime/directory-publishing.md#5-mvmz-init-发布阶段的-os-5)
全部条件同时成立时，才允许用完全相同的命令重试一次。其他 OS 5、目标已存在、
`recovery_required` 或 `outcome_unknown` 都不能套用这一分支。

### 6.3 Extract

MV/MZ 分别确认 Builtin、Rules 与 MV dialogue owner。每个 owner 独立提交，因此一个 owner
失败不代表另一个 owner 没有更新；按 [Extract](../rpg-maker/extraction.md)检查当前保存的
owner 和资源。Rules 非字符串跳过警告是成功结果的一部分，必须按
[Rules](../rpg-maker/rules.md)逐类解释，不能当作失败，也不能忽略。

Generic 按 [JSONL](../generic/jsonl.md)和 [Extract](../generic/extraction.md)处理语法、重复身份、
来源变化与事务错误。任何来源、分组、身份、自然顺序或写回映射改变，都从 Extract 重新
建立当前状态。

如果调查发现某批 MV/MZ 文本原先被直接分配给 Generic，必须先重新核对 Builtin 与 Rules
能力；能由 Rules 完整表达的内容应回到 MV/MZ 项目，而不是继续维护无必要的外部 JSONL。

### 6.4 Translate

完整读取对应引擎 Translate、[Prompt](../translation/prompts.md)、
[任务记录](../translation/task-records.md)、[项目日志](../runtime/project-log.md)和实际失败
涉及的资源或 HTTP 规格。

#### Complete

Complete 只说明当前项目本轮 Translate 的目标明确完成。进入全量验收，仍要检查项目范围、
人工状态、输出和实际消费者。

#### Partial

合法 ID 已经保存，失败项留给后续处理。先读取当前 RunId 的 `task.finished`、它引用的
`diagnostic.translation_task` occurrence、唯一 `translation.finished` 与可用任务记录。
`translation.finished.payload.result.kind` 应为 `incomplete`，其中保存完整任务计数和引擎
专用汇总；按具体 issue 和 `resolution` 分类：

- 暂时性外部失败或模型偶发输出，且再次运行有合理进展预期：使用同一项目、Profile 和
  资源再次 Translate；Current 保留，ATT 重新为仍需模型的 Unit 分配临时 ID，并保留完整
  TaskBlock 语境。
- Rules、JSONL、语言、术语、Placeholder、Prompt 或配置事实错误：先修正根因，再按对应
  规格让 Translate 重新判断状态。
- 重复运行没有新增提交，或同一确定性验收错误持续出现：停止无效模型请求；按
  [Lua](../lua/README.md)枚举当前候选和完整 Group 语境，由人工或 agent 翻译并使用
  `ctx.translation.set` 精确提交，再全量复验。

任务记录中的数字 ID 只属于一次请求，不是数据库 locator。不得把临时 ID 写进 Lua，
也不得只拿失败原文脱离 Group 语境补译。

#### Unavailable

先看每个任务的结构化原因。运行时只对 [Chat Completions](../runtime/chat-completions.md)
规定的传输与 HTTP 情况执行有限重试；操作者再次运行 Translate 是另一项决定，不是内部
重试的自动延长。

暂时性服务故障可以在原因消失后用同一项目和资源继续。Fatal HTTP、认证、额度、Endpoint、
Model 或参数问题不能靠重跑或擅自换配置解决。只有任务明确要求继续模型路径时，才为需要
改变的配置取得用户给出的精确新值；若目标是完成或修复翻译，当前 Unit、完整语境、语言、
术语和 Placeholder 已足以让 agent 负责译文，则直接使用上一节的 Lua agent 修订路径，
不因缺少新的 Endpoint、Model 或凭据而停下。确定性模型响应反复无进展时也使用该路径，
不无限请求同一模型。只有缺少会改变译文结果的真实材料时才询问用户。

#### 整体无效响应与逐 ID 失败

响应根或 JSON 无效时，该任务不提交；根有效时合法 ID 可以单独保存，其余形成 Partial。
诊断时同时查看 System、User、可用 Thinking、Raw Assistant 和逐 ID 原因，但这些记录
不是数据库权威。根据是否有新增提交和失败是否暂时，选择继续 Translate、修正资源，
还是由人工或 agent 修订。

#### 取消、并发与提交状态

取消或后续任务失败不会撤销已确认的前序提交。CAS 或并发变化不会覆盖新状态。重新执行前
先确认当前数据库候选，不按旧任务记录重放已完成译文。

### 6.5 Lua

按 [Lua 规格](../lua/README.md)区分编译前失败、脚本或 SQL 失败、最终校验失败、取消、
明确提交与结果未知。明确回滚后可修正脚本重做；提交后重新查询受影响 Unit；
`outcome_unknown` 时停止所有写入。

### 6.6 WriteBack 与目录发布

按对应 WriteBack 和[目录发布规格](../runtime/directory-publishing.md)区分：

- 候选建立或验证失败：上一次成功输出保持；修正根因后重新生成；
- Partial 项目写回：未译内容可能保留当前原文，不能因此宣称翻译完成；
- 人工布局警告：按诊断中的 `group_location + role` 用 Lua 取得唯一 locator、当前译文和
  完整 Group 形状，并检查该显示请求内的译文、保留原文、控制序列和硬换行。只有原因是
  行过宽或没有安全自动断点时，才按 `region` 与 `max_fullwidth_chars` 加入显式硬换行，用
  `ctx.translation.set` 保持字符串或字符串数组形状并提交；若是无效 Placeholder、控制字符
  或译文语法则返回相应阶段修正。游戏有效但布局器无法理解的控制语法可以保留，必须记录
  原因，并在所有相关实际场景中确认显示正确；这种情况重新 WriteBack 后仍有警告是预期
  结果，不能靠反复加换行消除；
- `report.effect = "applied_finalization_failed"` 且 Publication issue 给出 `output_root` 与
  `residual_path`：新输出已发布；先修正嵌套 backend diagnostic 中的清理失败，再按第 4.4
  节条件执行一次同目标命令。`publication.finished.payload.result.kind =
  "recovery_required"` 并引用同一 occurrence；Generic 与 RPG Maker 都使用该契约；
- 主 report 或 related report 的 `effect = "recovery_required"`：保留 issue 中列出的
  `.directory-publish-*` 现场，按第 4.4 节的稳定 code、类型化问题与产物组合分流；只有符合
  自动恢复条件时才执行一次同目标、同输入命令；
- Generic occurrence 的 `related` 中存在 `relation = "cleanup"` 时，遍历每个 FileSystem
  issue 的精确 path：`.directory-publish-*` 仍按第 4.4 节条件分流；项目内
  `.generic-write-back-*` 没有清理旧残留的公开入口。两类路径可以在同一运行同时存在，
  必须分别处理。对后者再按相关主诊断判断输出是否发布。
  主状态明确未发布时，修正原失败后可以重新 WriteBack，但只会建立新 scratch，不会清除
  旧路径；旧残留必须报告，得到操作者对精确路径的外部删除授权前不能算作已恢复；
- `outcome_unknown`：禁止重复发布，先确认实际目标状态；
- Generic 输入与最近 Extract 不一致：返回 Generic Extract。

### 6.7 SQLite 与可观测性

数据库拥有项目、Unit 与译文状态；目录 journal 拥有发布恢复事实；项目日志和模型任务
记录只保存诊断证据。各自的读取者和失败语义见 [SQLite](../runtime/sqlite.md)、
[目录发布](../runtime/directory-publishing.md)、[项目日志](../runtime/project-log.md)和
[任务记录](../translation/task-records.md)。任何一种记录都不能替代其他权威来源。

## 7. 从质量问题返回正确阶段

| 观察到的问题 | 最早责任位置 |
| --- | --- |
| 漏提、重复项目所有权、Rules 本可覆盖却误用 Generic | 来源调查与项目分配 |
| Group、语境、自然顺序、稳定 ID 或写回映射错误 | Extract 或外部 JSONL 转换 |
| 语言、术语、Placeholder、Prompt 或模型要求错误 | Translate 准备 |
| 个别译文错误、同文异译或剩余候选可直接补译 | Lua 人工或 agent 修订 |
| 控制符、形状、候选结构或布局错误 | Translate 验收或 WriteBack |
| Generic 反向转换错误 | 外部转换 |
| 游戏没有读取交付文件 | 部署与实际消费者 |

修复后从最早失效位置重新执行全部下游检查。权威状态明确、恢复现场已按规格处理、下一
阶段输入仍有效时，才返回正常流程。
