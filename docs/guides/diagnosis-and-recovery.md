# ATT 诊断与恢复指南

命令失败、Translate 未完整、取消或出现恢复提示时，先确定已经生效的结果，再选择继续方式。
本指南组织排错步骤，具体状态与事务边界由对应现行规格定义。

## 1. 确认本次结果

先收集当前命令直接产生的事实：

1. stdout 的业务摘要；
2. stderr 的对象、原因、影响和处理办法；
3. 存在时读取 `<project>/logs/run-000001.jsonl` 中同次运行的事件；
4. Translate 实际发过模型请求时，读取
   `<project>/task-records/run-000001/task-000001.md`；
5. 检查项目数据库和输出目录是否存在、是否仍能由对应普通命令读取。

Manual export/check/apply 的执行结果直接显示在 stdout/stderr，不建立 RunId 或项目日志。

不要先删除数据库、任务记录、目录发布工作区或临时文件，也不要用重跑试探结果未知的
提交。项目日志和任务记录只是证据，不是数据库或目录恢复权威。

## 2. 用三个维度分类

### 2.1 进程终态

- 成功：命令取得明确结果；Translate 可以是明确的 Incomplete；
- 失败：命令没有完成目标，但状态影响可判断；
- 取消：合作取消已经完成收尾；
- 需要恢复：状态明确，但存在必须保留或由同目标命令处理的恢复现场；
- 结果未知：是否生效无法确认，不能继续写入或重跑试探。

### 2.2 当前阶段的业务结果

区分 Init、Extract、Translate、Manual、Lua 和 WriteBack。相同的“失败”在不同阶段有不同
权威状态和重试入口。

### 2.3 状态影响

确认本次是否：

- 没有修改项目；
- 保留了已经确认的前序进度；
- 应用了本次修改；
- 已发布输出但清理失败；
- 留下需要恢复的数据库事务或目录；
- 无法确认是否生效。

诊断的“影响”说明已经确认的状态变化；再结合当前数据库、输出目录和对应规格判断下一步。

## 3. 通用处理顺序

1. 读对象、原因、影响和处理办法；
2. 打开当前阶段规格；
3. 确认数据库或输出是否已经变化；
4. 判断是输入问题、当前项目状态、外部服务、文件系统、数据库、日志还是呈现故障；
5. 只修改直接原因；
6. 只有状态明确允许时才重跑；
7. 重跑后重新检查业务摘要和实际产物。

定位翻译条目时，使用 Manual 或高级 Lua 的可读 ID；只有任务需要调查数据库结构时，才使用
`ctx.db` 低级接口。

## 4. 按进程终态判断

### 4.1 成功

成功只表示本命令得到明确结果。Translate Incomplete、WriteBack 保留原文，以及译后 QA 的
`needs_review` 或 `unverified` 都可能退出 `0`，仍需按业务摘要和验收指南继续处理。

### 4.2 失败

按诊断修正输入、配置、规则、权限或项目状态。确认结果明确且恢复现场已经处理后，重跑同一
命令。局部译文问题用 Manual 修订；全局规则只在已确认存在系统性错误时调整。

### 4.3 取消

Translate 已确认的前序译文保留；尚未开始的模型任务不产生记录。Lua 只回滚取消时仍打开的
事务，之前的 autocommit 或显式 COMMIT 保留。重新运行前先读取当前项目状态，不假设“整个
命令都已回滚”。

### 4.4 需要恢复

目录发布工作区固定为：

```text
<parent>/.directory-publish/<target-name>/stage
<parent>/.directory-publish/<target-name>/backup
<parent>/.directory-publish/<target-name>/journal
```

保持项目、输入、目标和这些路径不变。先解除诊断指出的占用、权限或磁盘问题，再运行同一
项目、同一目标的 Init 或 WriteBack；命令会在建立新候选前先恢复。

journal 损坏、目标与已知旧目录都缺失、必要 backup 缺失，或一次恢复后仍无法取得明确状态
时，停止自动重试。不要手工删除、改名或移动工作目录。

### 4.5 结果未知

停止同一项目的后续写入和重跑，保留数据库、SQLite sidecar、输出目标、目录发布工作区、
日志与任务记录。当前普通 CLI 没有通用 status 或人工提交恢复命令；报告已经确认的事实和
当前能力限制。

### 4.6 日志、任务记录或终端呈现失败

日志和任务记录故障不改变已经确定的业务结果。先读取数据库或输出确认实际结果，再修正
路径、权限或磁盘问题。警告或终态本身无法呈现时，进程返回 `1`；不能据此推断业务修改未
发生。

## 5. 按失败来源选择处理办法

| 失败来源 | 先读 | 处理原则 |
| --- | --- | --- |
| CLI 或配置 | [CLI](../runtime/cli.md)、[配置](../runtime/configuration.md) | 修正显式输入，不猜默认值 |
| Manual TOML | [Manual](../manual/README.md) | 按可读 ID 修正语法、原文、type、形状、空槽或 Placeholder |
| Placeholder | [Placeholder](../translation/placeholders.md) | 只有系统性规则错误才修改全局规则 |
| 术语或语言 | [术语](../translation/terminology.md)、[语言](../translation/language.md) | 区分结构错误与翻译质量问题 |
| 模型请求 | [OpenAI-compatible HTTP](../runtime/openai-compatible.md)、[任务记录](../translation/task-records.md) | 保留已确认进度，按请求事实判断重试 |
| SQLite | [SQLite](../runtime/sqlite.md) | 普通命令只接受当前 schema；raw Lua 修改自行承担结果 |
| 目录发布 | [目录发布](../runtime/directory-publishing.md) | journal 是恢复权威，不用日志重放 |
| 日志或任务记录 | [项目日志](../runtime/project-log.md)、[任务记录](../translation/task-records.md) | 作为证据故障处理，不改写业务结果 |

## 6. 按阶段处理

### 6.1 启动、配置与发行

Help 和 Version 不需要配置。其他命令从 `att.exe` 同目录读取本次需要的固定资源。配置、Prompt、
语言模块或发行资源有误时，先按诊断修正。下载包用 `SHA256SUMS.txt` 核验，解压后的文件集合、
资源一致性与独立运行按[发行物规格](../runtime/distribution.md)检查。

### 6.2 Init

Generic 首次 Init 使用 `<project>/.project.db.init.tmp`，失败时会尝试清理候选和 SQLite
sidecar。清理失败时保留准确路径，先解除占用并确认内容，再按诊断处理。

MV/MZ Init 使用目录发布器建立来源与数据库。交换前失败保持旧项目；已发布但清理失败或
需要恢复时，按 4.4 节运行同目标 Init。项目数据库不符合当前 schema 时，按数据库损坏处理。

### 6.3 Extract

Generic Extract 整体原子提交；输入在读取期间改变时数据库不变，重新稳定输入后再运行。
MV/MZ 每个 owner 独立提交，后续 owner 失败不撤销已经成功的前序 owner。

原文或实际结构变化会让对应人工记录过期，旧正文继续保留并由高级 Lua 查看；无关人工译文
保持当前。Extract 的结果由当前内容视图与摘要确认。

### 6.4 Translate

#### NoWork 或 Complete

确认剩余与 Rejected 为零，再检查当前译文并进入 QA、WriteBack。NoWork 表示本轮没有需要
执行的模型任务；Complete 表示本轮要求的翻译已经完成。两者都不代替完整游戏的质量与覆盖验收。

#### Incomplete

Incomplete 是 Translate 的业务结果，进程仍可退出 `0`。Partial、Unavailable 和未开始描述
其中各 Task 的情况；接下来还要按条目状态选择动作：

| 当前情况 | 下一步 |
| --- | --- |
| pending，服务短暂失败或请求耗尽 | 修复服务原因，确认状态明确后再次 Translate，保留前序进度 |
| Rejected，候选违反控制符或结构契约 | 用 `manual export --selection rejected` 导出并修订；确需重新请求模型时显式使用 `--retry-rejected` |
| Prompt、Placeholder 或语言规则有可复现的系统性错误 | 修正对应资源；再次 Translate 时显式选择是否重试 Rejected，普通重跑仍跳过它们 |
| 少量剩余、同文异译或局部质量问题 | 用 Manual 集中补译或修订 |
| 含义不明 | 把全部待查 ID 合并到一次 `ctx.translation.context(ids)`，同时读取 `ctx.terminology.list()` |
| 仍缺上下文或领域事实 | 记录准确位置、缺失事实和人工调查场景，保留当前项目 |

Manual 默认流程为 export、填写、apply。apply 已执行与 check 相同的结构和 Placeholder 检查；
只在需要事先试检或单独诊断 TOML 时先运行 check。Manual 负责 TOML 结构与 Placeholder 验收；
apply 后按[翻译验收指南](acceptance.md)继续检查残留英文、译文等于原文、术语偏好和翻译质量，
需要复核数据库全部当前正文时重新执行 `translation export`。

#### 请求或模型结果不可用

Unavailable Task 没有产生可接受译文，可能仍保存了能够唯一定位的 Rejected 候选。结合本次日志、
任务记录和 `translation export` 确认条目状态，再按上表处理。普通网络错误耗尽通常只影响当前
Task；认证、权限、额度或普通 429 停发按 [HTTP 规格](../runtime/openai-compatible.md#4-失败与重试)
区分。修正服务问题不会自动清除已经保存的 Rejected。

#### 整体无效响应与逐 ID 失败

整体根结构无效时该 Task 不提交；逐 ID 问题只拒绝对应项，其他合法项可以形成 Partial 并
保存。任务记录中的数字 ID 每次请求重新分配，不能传给 Manual 或 Lua。

#### 取消、并发与提交状态

任务可以并发执行，但按自然顺序确认和提交。取消或后续失败不撤销已经确认的前序进度。
当前人工译文出现时，模型结果不能覆盖它。

### 6.5 Manual 与 Lua

普通人工补译使用 Manual。TOML 不携带上下文，必要时用高级 Lua 一次批量读取。高级
`translation.set/clear` 与 Manual 共用结构和 Placeholder 检查。

Raw `ctx.db` 从 autocommit 开始，可以执行 DML、DDL、PRAGMA 和显式事务，也可以故意删除表、
制造孤儿关系或写乱码状态。ATT 不做 schema 或业务保护。失败和取消只回滚当时仍打开的
事务；已经自动提交或显式提交的修改保留。只有任务明确需要低级数据库操作时才使用它。

### 6.6 WriteBack 与目录发布

WriteBack 可以在翻译未完整的项目上运行；pending 和 Rejected 条目使用原文。当前人工译文优先
于自动译文，两者都必须满足当前 Placeholder 契约。显式排版规则先经验证并保存；正文随后依次
执行自动译文标点修复、规则断行和续行空白补全。人工译文跳过标点修复，其他步骤按各自开关与
规则执行；完整行为见对应 WriteBack 与[排版规则规格](../translation/write-back-layout-rules.md)。

规则未覆盖、无法安全断行或仍需人工判断的布局问题由译后 QA 或实际界面观察报告。按可读 ID 导出修订 Manual，调整后再次运行 QA；
需要 Group 语境时批量调用 Lua context。修订完成后重新 WriteBack，并在隔离副本中检查
实际显示。

候选验证失败或目录交换前取消时，上一次输出保持。发布已经开始后按 4.4 和 4.5 区分可恢复
与结果未知。不要依据路径名猜恢复动作，固定工作目录仍以 journal 为权威。

### 6.7 SQLite 与可观测性

普通命令发现当前 schema 不完整时停止并报告问题。公开诊断不显示 SQLite 查询、code、
数据库行或内部指纹。Raw Lua 破坏数据库后，普通命令失败是预期结果；Generic Lua 仍可直接
打开 `project.db` 继续调查。

日志和任务记录缺失不证明请求或提交没有发生。以数据库、输出目录和对应事务或 journal
为准。

## 7. 从质量问题返回正确阶段

- 漏提文本：回到 Extract、Rules 或 Generic JSONL；
- 模型协议、术语或语言系统性错误：回到 Translate 资源；
- 少量错译、漏译、同文异译或需要硬换行：Manual；
- 批量上下文、复杂筛选、计算生成或特殊修改：Lua 高级 API；
- 明确需要绕过全部保护：Raw Lua，并承担数据库可能完全损坏的结果；
- 输出结构或发布问题：WriteBack 与目录发布；
- 实际游戏没有采用输出：外部部署与消费者检查。

用局部 Manual 修订处理单个译文问题；有可复现证据时再修正系统规则。无法确定的问题保留
准确位置和缺失事实，供后续调查。
