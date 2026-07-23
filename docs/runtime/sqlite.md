# ATT SQLite 运行时现行规格

## 1. 产品策略

SQLite 不接受用户运行时配置。生产根启动时探测 Windows 可用并行度，短操作 worker
宽度固定为 `min(可用并行度, 4)`；探测失败则带稳定 OS 原因终止启动，不静默退化为
串行。数据库固定使用 WAL + FULL。全新数据库在首个表和 WAL 建立前选择 64 KiB 页；
已经存在的数据库继续使用自身页格式，不为页大小执行 `VACUUM` 或格式改写。生产连接采用
SSPV 消融基准确定的 3 GiB SQLite cache 目标和内存 TEMP；cache 只影响页面驻留策略，
不会预分配对应内存，也不是项目大小上限或容量拒绝条件。

短操作只有一层执行许可，宽度与 worker 数相同。调用方先取得许可，再把工作交给不承载
容量策略的语义传输通道；许可随工作保持到 SQLite 操作结束并投递结果。因此等待和执行中
的短操作总数不会超过 worker 宽度，饱和只会自然背压或响应取消，不产生队列满、准入超时
或项目过大错误。不存在独立 queue capacity。

SQLite 也不设置独立 max connections。普通短操作由实际 worker 各自使用连接；online
backup 因 SQLite 机制真实需要源、目标两个连接；交互路径同时只允许一个会话。实际连接数
由这些正在执行的操作自然决定，不再叠加第二层连接预算或等待窗口。

SQLite busy、项目租约和发布租约不设置任意截止时间，等待必须能够响应 Ctrl+C 或
shutdown。不得用超出 SQLite 参数表达范围的巨大 timeout 伪装无限等待。

ATT 不限制 SQL 字节、参数字节、查询组数、结果行数或结果字节，也不为 Claim、Unit、
Group、Task 等业务总量设置容量。输入只会因 bundled SQLite 的真实能力、地址空间、
内存、磁盘或 SQL/数据错误失败。

## 2. 只读快照

数据库必须已存在，并以 READ_ONLY 打开；缺失时不得创建。一次领域快照可以提交任意
非空数量的查询集合。运行时在同一连接和同一显式只读事务中按输入顺序直接遍历 cursor，
不使用 `LIMIT/OFFSET`，最后返回保持集合与列顺序的内存结果。

同一快照开始后，即使其他连接向 WAL 提交，全部查询仍看到开始时的一致状态。任一查询
失败会回滚只读事务；查询 ID、自然序号和阶段进入安全诊断，SQL 与参数不进入。

结果值保留 NULL、INTEGER、REAL、TEXT、BLOB 和列顺序。完整结果允许保存在内存中；
性能设计使用窄查询、直接 cursor 遍历、连接与 cached statement 复用，避免宽
`UNION ALL`、重复全局排序、重复连接和重复 PRAGMA。查询结果在同一进程内直接归并为
调用方需要的内存结构，不引入逐行跨任务往返。

## 3. 写事务与批量

所有需要共同成立的步骤在一个显式事务中按领域顺序执行。普通批量契约只准备一次
单行 statement，再顺序绑定任意数量的参数组；明确的 bulk INSERT 使用连续参数区，
并按当前连接读取到的 bundled SQLite 真实变量上限自动生成多行 `VALUES` 分块。两者都
不限制业务总行数或整批参数总量。重复的大值使用编号公共参数，在每个 statement 中只
绑定和持有一次。

批量步骤在首个绑定、执行、条件或修改行数错误时停止，后续工作不执行，整个事务回滚。
提交明确失败表示未提交；提交调用返回成功后收尾失败表示已提交但收尾失败；无法判定
事务是否提交时返回 OutcomeUnknown。不得用日志或错误字符串猜测事务终态。

标准资产的完整逻辑 Mutation Claim 由 group kind、location 和 recipe 决定，参与 owner
指纹以及组内、owner 内、跨 owner 和 WriteBack 验证。SQLite
`standard_mutation_claim` 只持久化确定性的跨 owner 冲突摘要：每个
`(owner, resource)` 至多一行；Exclusive 唯一，多个 Intent 保留自然顺序最早 group
代表。WriteBack 必须从 recipe 重建完整 Claim、重算原 owner 指纹，并严格比对摘要；
摘要不是缓存或绕过完整验证的捷径。

标准资产替换会依据非空 incoming Claim 摘要与其他 owner 摘要的实际数量关系选择二级
索引维护算法：小 owner 在线维护；incoming 摘要不少于其余摘要总量时，在同一事务内删除
两个命名索引、直接写正式摘要表、用项目数据库的权威 DDL 恢复索引，再执行冲突条件。
该策略来自最大真实游戏的提交消融，不是项目容量阈值。事务提交后的 schema 与项目数据库
现行定义完全相同；冲突、命令失败或取消均回滚资产行和 DDL，不会留下缺失索引或半快照。
在领域指纹和无变化判断完成后，批量参数会按当前主键/唯一索引的物理顺序排列以减少随机
B-tree 写入；自然顺序仍由显式顺序列决定，不能从 INSERT 顺序推导业务语义。

Translate preparation 使用一个事务完成 baseline CAS、失效、复用和资源更新；每个任务
仍按自然顺序独立事务提交，以保留有效前缀进度。一次 Translate 运行应复用连接与 cached
statements，避免每任务重新打开连接和重复执行相同 PRAGMA。

## 4. Lua 交互会话

每个 Lua 阶段使用唯一受控交互会话；同一 SQLite 根不会同时打开第二个交互会话。`query`
与 `execute` 各接受一条完整 statement；Lua 负责 SQL 和参数内容，Host 只维护连接生命
周期、取消、事务和错误边界。会话内部命令通道只传递顺序语义，不是可配置队列或额外容量
治理。会话完成时必须明确 commit 或 rollback，Drop/shutdown 不能留下占用连接或未知
事务；等待 worker、SQLite busy 和会话收尾都必须可由取消/shutdown 唤醒。

Lua/SQL 正文和参数永不进入 CLI 或 JSONL；诊断仍必须公开 query ID、阶段、数据库路径、
SQLite primary/extended code 和确认的事务终态。

## 5. 项目数据库

项目数据库不包含业务 revision/version 字段。有效性只由当前表结构、约束和领域不变量
决定；满足这些事实的数据库自然可用，不满足时按具体 schema、状态或完整性错误处理。

项目 Inspect 把 schema version、受管 schema、metadata、owner state、翻译资源、项目定义、
运行方案、Lua 程序、`quick_check` 和 `foreign_key_check` 作为有稳定 ID 的窄查询，一次性
提交给第 2 节的同连接只读快照。对账使用该快照中的精确 schema version 和领域事实执行
CAS；不得通过多次独立只读事务拼接项目状态，也不通过新增 revision/version 字段补偿。
