# SQLite 生产运行时现行规格

`RusqliteStorage` 是建库、只读查询、短事务、数据库快照和 Lua 唯一交互会话的生产根，使用 bundled SQLite。

## 1. 共享预算

配置显式建立短操作线程/队列、连接总数、worker 栈、statement/参数/查询行数/查询字节、busy timeout、journal mode 和 synchronous。短操作在固定 OS worker 上有界背压；进入队列后由根执行到明确终态，即使等待 Future 被丢弃也不撤销。

当前产品同一时刻最多有一个 Lua 交互会话。交互命令通道容量固定为 1，会话计入连接总预算。

## 2. 建库与快照

建库以 `OpenOptions(create_new)` 原子占有路径，再用 SQLite READ_WRITE 且不启用 CREATE 打开，应用统一策略，并在 `BEGIN IMMEDIATE` 内写 schema 和 metadata。失败时只清理已经证明属于本次操作的主文件和 sidecar。

终态固定为 `AlreadyExists | NotCreated | ResidualArtifact | OutcomeUnknown`。无法确认提交、文件身份或清理结果时不得自动重试或猜测未创建。

已有项目收敛使用 rusqlite online backup 把数据库复制到未发布候选；快照和后续候选数据库修改都位于外层项目租约内。

## 3. 查询与短事务

只读查询确认数据库已存在后以 READ_ONLY 打开，缺失不创建。结果保留 NULL、INTEGER、REAL、TEXT、BLOB 和列顺序；超过行数或字节预算时整次拒绝。

短写事务固定 `BEGIN IMMEDIATE`，顺序执行：

- `Execute`；
- `ExecuteMany`（只 prepare 一次）；
- `RequireNoRows`（命中即停止并回滚）。

`RequireNoRows` 的失败不携带内部字符串检查 ID。并发修改、Owner 冲突或翻译计划失效由各领域消费方在自己的边界映射。

提交失败后使用 `is_autocommit()` 判断是否仍在事务，并在可能时回滚。可确认回滚才返回 `NotCommitted`；提交或回滚结果不明返回 `OutcomeUnknown`。根不做应用层重试。

## 4. 唯一 Lua 交互会话

每次显式 Lua 打开一个 actor 线程和一条 `rusqlite::Connection`：

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

## 5. shutdown

shutdown 停止新短操作和新交互会话；若唯一会话存在，则与 finalizer 共享同一次终结，停止命令准入、排空已接管命令、回滚并关闭。finalizer 与 shutdown 并发只执行一次清理。

随后排空短操作并 join worker。没有超时强拆；已接管的 SQLite 副作用必须到明确终态。
