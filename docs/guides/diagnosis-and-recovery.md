# ATT 诊断与恢复指南

命令失败、Translate 未完整、取消或出现恢复提示时，先确定已经生效的结果，再选择继续方式。
本指南组织排错步骤，具体状态与事务边界由对应现行规格定义。

## 1. 确认本次结果

先收集当前命令直接产生的事实：

1. stdout 的业务摘要；
2. stderr 的对象、原因、影响和处理办法；
3. 存在时读取 `<project>/logs/run-000001.jsonl` 中同次运行的事件；
4. Translate 实际发过模型请求时，读取 `<project>/task-records/run-000001/task-000001.md`；
5. 检查项目数据库和输出目录是否存在、是否仍能由对应普通命令读取。

Manual `export`/`check`/`apply` 的执行结果直接显示在 stdout/stderr，不建立 RunId 或项目日志。
项目日志和任务记录仅作诊断证据；恢复判断以数据库和目录发布 journal 为准，排错前不删除数据库、任务记录或目录发布工作区。

## 2. 分类维度

从三个维度确认现场：

- **进程终态**：成功（含明确的 Translate Incomplete）、失败、取消、需要恢复（`recovery_required`）、结果未知（`outcome_unknown`）；
- **当前阶段**：Init、Extract、Translate、Manual、Lua 或 WriteBack；
- **状态影响**：根据诊断中的“影响”字段及数据库/输出目录确认是未修改项目、保留前序进度、已应用修改、已发布但清理失败、留下待恢复现场，还是无法确认是否生效。

## 3. 通用处理顺序

1. 读取诊断中的对象、原因、影响和处理办法，对照当前阶段规格；
2. 确认数据库或输出是否已变化，定位直接原因（输入、项目状态、外部服务、文件系统或呈现故障）；
3. 修正直接原因；仅在状态明确允许时重跑，并核对重跑后的业务摘要和产物。

定位翻译条目使用 Manual 或高级 Lua 的可读 ID；仅在调查底层表结构时使用 `ctx.db`。

## 4. 按进程终态判断

### 4.1 成功

命令退出 `0` 表示取得明确阶段结果。Translate `Incomplete`、WriteBack 保留原文，以及译后 QA 的
`needs_review` 或 `unverified` 均可能退出 `0`，按业务摘要和验收指南继续处理。

### 4.2 失败

按诊断修正输入、配置、规则、权限或项目状态。确认结果明确且恢复现场已处理后，重跑同一命令。
局部译文问题用 Manual 修订；全局规则仅在确认存在系统性错误时调整。

### 4.3 取消

Translate 已确认的前序译文保留；尚未开始的模型任务不产生记录。Lua 只回滚取消时仍打开的
事务，此前的 autocommit 或显式 `COMMIT` 保留。重新运行前先读取当前项目状态。

### 4.4 需要恢复

目录发布工作区固定为：

```text
<parent>/.directory-publish/<target-name>/stage
<parent>/.directory-publish/<target-name>/backup
<parent>/.directory-publish/<target-name>/journal
```

保持项目、输入、目标和上述路径不变。先解除诊断指出的占用、权限或磁盘问题，再运行同一
项目、同一目标的 Init 或 WriteBack；命令会在建立新候选前自动恢复。

journal 损坏、目标与已知旧目录都缺失、必要 backup 缺失，或一次恢复后仍无法取得明确状态
时，停止重试，不手工删除、改名或移动工作目录。

### 4.5 结果未知

停止同一项目的后续写入和重跑，保留数据库、SQLite sidecar、输出目标、目录发布工作区、
日志与任务记录，如实报告已确认事实。

### 4.6 日志、任务记录或终端呈现失败

日志和任务记录故障不改变已确定的业务结果。先读取数据库或输出确认实际结果，再修正
路径、权限或磁盘问题。警告或终态无法向终端呈现时进程返回 `1`，不代表业务修改未发生。

## 5. 按失败来源选择处理办法

| 失败来源 | 权威规格 | 处理原则 |
| --- | --- | --- |
| CLI 或配置 | [CLI](../runtime/cli.md)、[配置](../runtime/configuration.md) | 修正显式输入 |
| Manual TOML | [Manual](../manual/README.md) | 按可读 ID 修正语法、原文、`type`、形状、空槽或 Placeholder |
| Placeholder | [Placeholder](../translation/placeholders.md) | 系统性规则错误时修改全局规则 |
| 术语或语言 | [术语](../translation/terminology.md)、[语言](../translation/language.md) | 区分结构错误与翻译质量提示 |
| 模型请求 | [OpenAI-compatible HTTP](../runtime/openai-compatible.md)、[任务记录](../translation/task-records.md) | 保留已确认进度，按请求事实判断重试 |
| SQLite | [SQLite](../runtime/sqlite.md) | 普通命令只接受当前 schema |
| 目录发布 | [目录发布](../runtime/directory-publishing.md) | 以 journal 为恢复权威 |
| 日志或任务记录 | [项目日志](../runtime/project-log.md)、[任务记录](../translation/task-records.md) | 作为证据故障处理，不改写业务结果 |

## 6. 按阶段处理

### 6.1 启动、配置与发行

Help 和 Version 不读取配置；其他命令从 `att.exe` 同目录读取所需资源。配置、Prompt、
语言模块或发行资源有误时按诊断修正。发行包完整性按[发行物规格](../runtime/distribution.md)检查。

### 6.2 Init

Generic 首次 Init 使用 `<project>/.project.db.init.tmp`，失败时尝试清理候选和 SQLite
sidecar；清理失败时先解除占用并确认内容，再按诊断处理。

MV/MZ Init 使用目录发布器建立来源与数据库。交换前失败保持旧项目；已发布但清理失败或
需要恢复时，按 [4.4 节](#44-需要恢复)运行同目标 Init。

### 6.3 Extract

Generic Extract 整体原子提交；输入在读取期间改变时数据库不变，稳定输入后重跑。
MV/MZ 每个 owner 独立提交，后续 owner 失败不撤销已成功的前序 owner。

原文或实际结构变化会使对应人工记录转为过期（旧正文仍保留，可由高级 Lua 查看）。

### 6.4 Translate

- **NoWork 或 Complete**：确认剩余与 Rejected 为零后进入[译后验收](acceptance.md)。
- **Incomplete**（含 Partial、Unavailable 和未开始 Task）：按条目状态分流处理：

| 当前情况 | 下一步 |
| --- | --- |
| pending，服务短暂失败或请求耗尽 | 修复服务原因，确认状态明确后再次 Translate（自动保留前序进度） |
| Rejected，候选违反控制符或结构契约 | 用 `manual export --selection rejected` 导出修订；需重新请求模型时显式加 `--retry-rejected` |
| Prompt、Placeholder 或语言规则存在系统性错误 | 修正对应资源后重新 Translate（按需加 `--retry-rejected`） |
| 少量剩余、同文异译或局部质量问题 | 用 [Manual](../manual/README.md) 集中补译或修订 |
| 响应包含多余编号，但要求的译文已全部接受 | 按任务记录 `Locations` 核对整段原译对照，检查是否存在错位 |
| 条目含义不明 | 合并待查 ID 调用一次 `ctx.translation.context(ids)` 并读取 `ctx.terminology.list()` |

Unavailable Task 未产生可接受译文，但可能保存了可唯一定位的 Rejected 候选；结合日志、任务记录和 `translation export` 确认条目状态后按上表处理。网络错误与 429/认证错误的停发边界见 [HTTP 规格](../runtime/openai-compatible.md#4-失败与重试)。任务记录中的临时数字 ID 仅属单次请求，不可传给 Manual 或 Lua。

### 6.5 Manual 与 Lua

普通人工补译按 [Manual 规格](../manual/README.md) 执行 `export → 填写 → apply`（`apply` 已内置完整结构与 Placeholder 校验）。批量读取上下文或复杂修改使用 [Lua](../lua/README.md)。

### 6.6 WriteBack 与目录发布

WriteBack 可在未完整项目上运行（pending 和 Rejected 条目保留原文），正文处理顺序与规则见对应 WriteBack 规格和[排版规则规格](../translation/write-back-layout-rules.md)。布局或质量问题通过 Manual 修订后重新 WriteBack。目录发布恢复见 [4.4 节](#44-需要恢复)与 [4.5 节](#45-结果未知)。

### 6.7 SQLite 与可观测性

普通命令发现当前 schema 不完整时停止并报告。Raw Lua 破坏数据库后，普通命令失败属预期行为；Generic Lua 仍可直接打开 `project.db` 调查。日志与任务记录故障处理见 [4.6 节](#46-日志任务记录或终端呈现失败)。

## 7. 从质量问题返回正确阶段

- 漏提文本：回到 Extract、Rules 或 Generic JSONL；
- 模型协议、术语或语言系统性错误：回到 Translate 资源；
- 少量错译、漏译、同文异译或硬换行调整：Manual；
- 批量上下文查询、复杂筛选或批量变换：Lua 高级 API（底层表操作使用 Raw Lua）；
- 输出结构或发布问题：WriteBack 与目录发布；
- 译本未加载新输出：外部部署与消费者检查。
