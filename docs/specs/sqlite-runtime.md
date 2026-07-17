# SQLite 生产运行时现行规格

本文记录 ATT 对项目数据库的生产运行时承诺。`RusqliteStorage` 是建库、
只读查询、短写事务和 Lua 交互会话共用的唯一 SQLite 根实现，使用
bundled SQLite，不依赖系统安装的 SQLite DLL。

## 1. 共享资源边界

统一配置边界必须显式提供：

- 短操作工作线程数与有界队列容量；
- 数据库连接总上限与交互会话上限；
- 交互会话打开队列和单会话命令队列容量；
- 工作线程栈大小；
- statement、参数、单次查询行数和查询结果字节上限；
- busy timeout、journal mode 与 synchronous 策略。

交互会话和短操作从同一连接总预算中取得 permit。交互会话在整个
生命周期内占有一条连接；会话终结不需要额外连接，因此会话用完连接预算也
不会阻止自身回滚和关闭。

短操作在有界队列中异步背压，阻塞的 SQLite 调用只在固定 OS 工作线程
上执行。任务成功入队后已由根接管；即使调用 Future 被丢弃，根仍会把已
接管操作执行到明确终态。

## 2. 建库

建库固定执行：

```text
OpenOptions(create_new) 原子占有路径
        ↓
关闭占位句柄
        ↓
SQLite READ_WRITE 且不启用 CREATE 打开
        ↓
应用 busy timeout / journal mode / synchronous
        ↓
BEGIN IMMEDIATE
        ↓
按顺序执行 schema 与 metadata 参数化命令
        ↓
COMMIT
```

同一路径的并发创建最多一个成功。命令和参数在产生文件副作用前完成
资源校验。根在占有主文件前固定完整父目录链；主文件身份来自 `create_new`
返回的句柄。三个 sidecar 在占有时必须全部不存在，初始化失败时只清理本次
初始化期间出现且已经捕获物理身份的普通文件。清理使用按 file ID 的句柄级
删除；路径被替换、对象身份无法证明或父目录失去固定时，根保留外来对象并
返回 `ResidualArtifact` 或 `OutcomeUnknown`，绝不按字符串路径盲删。

建库只返回以下终态：

- `AlreadyExists`：原子占有发现目标已存在，未覆盖它；
- `NotCreated`：可确认没有可用新数据库，并保留初始化或清理原因；
- `ResidualArtifact`：创建未成功，但可确认仍有产物残留；
- `OutcomeUnknown`：文件状态无法分类，调用方不得自动重试。

## 3. 查询与短写事务

只读查询先确认主数据库是现存普通文件，再以 `READ_ONLY` 打开；缺失路径
绝不会被 SQLite 顺便创建。返回值保留 NULL、INTEGER、REAL、TEXT、BLOB 五种
存储类型和原列顺序。读取超过配置行数或字节预算时，整个查询拒绝，不返回
截断结果。

短写计划在同一连接的 `BEGIN IMMEDIATE` 事务中顺序执行：

- `Execute` 执行一条参数化命令；
- `ExecuteMany` 只 prepare 一次，再按顺序绑定全部参数组；
- `RequireNoRows` 一旦命中就停止后续步骤，完整回滚后返回对应 check ID。

提交失败后以 `Connection::is_autocommit()` 区分终态：仍在事务中时尝试回滚，
只有能确认回滚时才返回 `NotCommitted`；提交可能已生效或回滚结果不明时返回
`OutcomeUnknown`。短 worker 在副作用被接管后 panic 或响应通道异常关闭时，
建库与短事务同样返回 `OutcomeUnknown`，不得把未知状态降为 `NotCreated` 或
`NotCommitted`。根不做应用层重试。

## 4. Lua 交互会话

每个交互会话拥有一条专用 OS actor 线程和一条 `rusqlite::Connection`。工厂
返回两个分离的权限：

```text
OpenedSqliteInteractiveSession
├─ Arc<Operations>     query / execute / begin / commit / rollback
└─ SessionFinalizer   唯一、不可克隆、finalize(self)
```

`begin()` 固定发出明确的 `BEGIN DEFERRED`。任意语句执行后，actor 都以
`is_autocommit()` 校正事务状态，不扫描 SQL 关键字。某次失败同时伴随事务状态
跨越时，该调用返回 `OutcomeUnknown`，actor 进入 `Indeterminate`；除唯一终结令牌外，
后续操作全部拒绝。

普通命令使用有界队列。终结令牌通过独立控制通道停止新命令，不会被已填满
的普通队列阻塞。actor 先排空已接管命令，再根据连接的权威 autocommit 状态回滚
活动事务并关闭连接。

`finalize(self)` 始终返回一份完整报告：

- 终结前事务观察：`Idle / Active / Indeterminate / Unavailable`；
- 回滚结果：`NotRequired / RolledBack / Failed / OutcomeUnknown / NotAttempted`；
- 连接关闭结果：`Closed / Failed / OutcomeUnknown`。

因此回滚失败和关闭失败可以同时保留。Lua 正常返回时若观察到 `Active`，根会
回滚，Host 同时把这个事实报告为 `UnclosedTransaction`。生产令牌在被直接丢弃
时也会发起安全终结，但调用方无法从这种兜底路径取得成功报告，仍必须正常
消费唯一令牌。

## 5. 关闭与取消

`shutdown` 先停止新建库、查询、短事务和会话打开，再按以下顺序终结：

1. 在活动会话注册表的同一原子边界停止新会话注册并取得当前会话快照；
2. 关闭两个准入队列，对快照中的每个 `SessionControl` 主动发起唯一终结；
3. actor 排空已接管命令、回滚活动事务并关闭连接，reaper 完成 join 后从注册表注销；
4. 等待注册表、连接与会话 permit 全部归零，再 join 短操作、会话打开与 reaper 线程。

唯一 finalizer 只拥有完整报告的接收权，不再独占 actor 的终结控制资源。因此
`shutdown` 可以在调用方仍持有 operations 和 finalizer 时完成；调用方随后按值
消费 finalizer，仍会取得该会话原本的完整终结报告。finalizer 与 `shutdown`
并发发起终结时只会有一次 actor join、一次 permit 释放和一份报告。

没有“超时后强拆”路径。一旦队列接管了数据库副作用，Future 取消只放弃
等待结果，不会中断已接管操作。正常进程边界必须显式等待 `shutdown`；最后一个
存储句柄被直接丢弃时，实现仍会关闭队列并在自有线程上排空作为安全兜底，
但不会把这条路径当作已确认的成功关闭。
