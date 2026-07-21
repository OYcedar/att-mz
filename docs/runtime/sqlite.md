# SQLite 生产运行时现行规格

`RusqliteStorage` 是建库、只读查询、短事务、数据库快照和 Lua 唯一交互会话的生产根，使用 bundled SQLite。

## 1. 共享预算

配置显式建立短操作线程/队列、连接总数、worker 栈、statement/参数/查询行数/查询字节、busy timeout、journal mode 和 synchronous。短操作在固定 OS worker 上有界背压；进入队列后由根执行到明确终态，即使等待 Future 被丢弃也不撤销。

当前产品同一时刻最多有一个 Lua 交互会话。交互命令通道容量固定为 1，会话计入连接总预算。Lua 可以来自本次显式非空文件，也可以来自项目数据库中按阶段保存的主程序快照。

## 2. 建库与快照

建库以 `OpenOptions(create_new)` 原子占有路径，再用 SQLite READ_WRITE 且不启用 CREATE 打开，应用统一策略，并在 `BEGIN IMMEDIATE` 内写 schema 和 metadata。失败时只清理已经证明属于本次操作的主文件和 sidecar。

终态固定为 `AlreadyExists | NotCreated | ResidualArtifact | OutcomeUnknown`。无法确认提交、文件身份或清理结果时不得自动重试或猜测未创建。

已有项目收敛使用 rusqlite online backup 把数据库复制到未发布候选；快照和后续候选数据库修改都位于外层项目租约内。

## 3. 查询与短事务

只读查询确认数据库已存在后以 READ_ONLY 打开，缺失不创建。结果保留 NULL、INTEGER、REAL、TEXT、BLOB 和列顺序；超过行数或字节预算时整次拒绝。

需要共同构成一个领域快照时，一次提交一至四条查询；运行时使用同一个只读连接，在显式读事务中按输入顺序执行并返回对应的分组结果。statement、参数、结果行和结果字节预算仍逐查询独立应用，因此整组资源上界固定为单查询预算的四倍。空查询组或超过四条的查询组在打开数据库前拒绝。任一查询失败即回滚读事务；结束或回滚读事务失败属于只读查询失败，不使用写事务的 `OutcomeUnknown` 语义。

短写事务固定 `BEGIN IMMEDIATE`。事务计划继续拥有全部 `SqliteValue`；运行时从计划中的
值直接借用参数进行绑定，`TEXT` 与 `BLOB` 不因每次执行再次复制。该绑定方式不改变
参数类型、资源预算、执行顺序或事务终态契约。

事务步骤按计划顺序执行：

- `Execute`；
- `ExecuteMany`（只 prepare 一次）；
- `ExecuteManyExactlyOne`（只 prepare 一次，每组参数必须恰好修改一行）；
- `RequireNoRows`（命中即停止并回滚）；
- `RequireNoRowsMany`（只 prepare 一次，每组参数都必须不返回行）。

批量步骤按参数组的输入顺序执行；首个驱动失败、查询命中或修改行数不为一时立即停止，
后续参数组不再执行，整笔事务回滚。条件步骤的失败不携带内部字符串检查 ID。并发修改、
Owner 冲突或翻译计划失效由各领域消费方在自己的边界映射。

提交失败后使用 `is_autocommit()` 判断是否仍在事务，并在可能时回滚。可确认回滚才返回 `NotCommitted`；提交或回滚结果不明返回 `OutcomeUnknown`。根不做应用层重试。

## 4. 命令运行方案与 Lua 主程序

项目数据库采用当前唯一 schema，分别保存 Init、Extract、Translate、WriteBack 的强类型
singleton 运行方案：

- Init 保存上次成功来源路径；语言对和三类宽度继续以 metadata 为权威；
- Extract 保存可执行 owner 的完整集合；Rules 保存已验证的 canonical 语义，不保存输入
  TOML 路径；
- Translate 保存上次成功 Profile ID；术语与 Placeholder 继续使用已有 canonical 资源表；
- WriteBack 保存 Lua 是否启用；尚无记录时由上层解释为固定的 Standard-only 行为。

Extract、Translate、WriteBack 的 Lua 主程序使用 phase-keyed 表分别保存非空正文 BLOB、
SHA-256 和无损 Windows 规范解析路径。自动复用执行 BLOB，不重新读取主文件；路径只用于
chunk 名、`require` 搜索目录和诊断。脚本主动加载的模块、文件和进程仍是外部动态依赖。
零字节输入由命令边界解释为清除对应阶段程序，不以空程序写入表。

项目租约覆盖方案读取、业务执行和最终方案替换。只有业务成功且所有必要非日志根完成
收尾后，才以最后一个短 `BEGIN IMMEDIATE` 事务原子替换本命令整套方案。确认回滚时旧
方案保持不变；提交终态无法确认时返回 `OutcomeUnknown`，上层明确说明业务结果及方案
状态不能确认。失败、取消或其他必要收尾失败不尝试更新方案。

运行方案是后续命令消费的权威状态，保存失败会影响命令结果；普通项目日志不是数据库
事务参与方，其任何故障都不改变方案读取、提交或退出码。当前 schema 一次性生效，不在
运行时识别、迁移或兼容其他 schema。

## 5. 唯一 Lua 交互会话

每次解析后的运行方案启用 Lua 时，打开一个 actor 线程和一条 `rusqlite::Connection`：

```text
OpenedSqliteInteractiveSession
├─ Arc<Operations>   query / execute / begin / commit / rollback
└─ SessionFinalizer  唯一、不可克隆、finalize(self)
```

普通命令通道容量固定为 1，finalizer 使用独立控制通道，因此即使命令通道已满也能停止准入。actor 排空已接管命令，使用 `is_autocommit()` 观察权威事务状态，回滚活动事务，关闭连接并结束线程。

`begin()` 固定执行 `BEGIN DEFERRED`。内部仍保留事务结果未知、回滚失败和连接关闭双错等完整事实，但 Host 只接收当前调用者需要的收敛结果：

```text
成功：had_unclosed_transaction
失败：cleanup_primary + optional_connection_close_failure
```

Lua 正常结束但留下活动事务时，actor 回滚并以 `had_unclosed_transaction = true` 告知 Host，Host 将其作为未关闭事务失败处理。finalizer 直接 Drop 仍触发唯一清理，但正常调用必须按值消费它以取得报告。

## 6. shutdown

shutdown 停止新短操作和新交互会话；若唯一会话存在，则与 finalizer 共享同一次终结，停止命令准入、排空已接管命令、回滚并关闭。finalizer 与 shutdown 并发只执行一次清理。

随后排空短操作并 join worker。没有超时强拆；已接管的 SQLite 副作用必须到明确终态。
