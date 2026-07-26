# Windows 文件能力与可恢复目录发布

## 1. 文件能力

`SystemFileSystem` 提供普通读取、目录列举、目录树指纹、跨进程租约、候选编辑和可恢复
目录发布。它拒绝 reparse point、硬链接、Windows 大小写等价冲突、路径逃逸和对象身份
变化，但不规定文件字节、单目录项、树深度、树总字节或恢复产物数量上限。

普通读取实际打开并读取完整文件；不会因为 metadata 宣称很大而提前拒绝。分配、读取、
磁盘或地址空间失败按真实 OS 操作、路径和错误码报告。

文件 worker 数由程序根据 Windows 可用并行度和基准选择。饱和调用在提交前自然等待，
没有用户队列配置或本地准入超时。独立文件允许并行，完成顺序不改变稳定 ordinal 和主
错误选择。

目录遍历与深层数据转换使用堆上工作栈，不能在删除深度上限后把合法深树变成 Rust 栈
溢出。指纹按稳定逻辑路径、对象种类和文件字节计算，并在观察窗口复核物理身份。

## 2. 项目与发布锁

项目租约位于 `<projects.root>/.att-locks/projects/<engine>/`，目录发布锁位于
`<projects.root>/.att-locks/directory-publish/<engine>/`。锁竞争持续等待并响应取消或
shutdown，不设置任意截止时间，也不返回“队列满”或“项目过大”。

每个锁文件使用稳定 Windows 非大小写身份摘要。实际取得锁时验证普通目录、reparse 和
系统锁能力；不对整个 `projects.root` 做无关的文件系统品牌预检。

## 3. 候选构建

`DirectoryStageRequest` 包含目标、`CreateNew | ReplaceExisting`、来源映射、overlay 和
空目录。所有目标必须是安全相对路径；来源与目标必须物理隔离。

候选先建立带自然 ordinal 的确定性 manifest，再并行执行独立文件工作：

- 未修改普通文件复制；
- overlay 使用 `create_new` 写入最终字节；
- 不同物理文档 read → parse → mutation → serialize；
- 错误按 manifest ordinal 稳定归并。

单文档内部的逆序结构修改保持串行。候选在发布前不可见，失败或取消时整树丢弃，因此
不为每个文件额外建立临时替换/rename 流程。

候选编辑只验证当前 stage identity、scope root、目标祖先、当前对象和回滚条件；不得在
bind、每次 Host 操作或每份文档后重复扫描完整树。

## 4. 验证与单次发布

Standard 完成后，单 VM Lua 修改同一候选。随后只执行一次完整 candidate 校验，覆盖普通
对象、Windows 等价名称、reparse、硬链接、物理身份和领域要求的顶层结构；成功后只执行
一次 journal/目录交换。

`CreateNew` 只发布到不存在目标；`ReplaceExisting` 使用同卷 journal、backup 和稳定
file ID 完成恢复安全的目录交换。提交前失败时目标不变；发布结果已生效但收尾失败、需要
恢复或 OutcomeUnknown 必须分别报告准确影响与恢复路径。

候选一经 prepare 交付,publish 与 discard 就必须运行至终态:业务取消只阻止新的等待
与新工作进入,不拒绝已交付候选的发布或清理。取消窗口内的 discard 正常删除候选并
返回成功,不产生取消错误,也不遗留未收尾的候选目录。

发布 journal 的帧长度使用真实 u32 格式字段，不设置更小的 ATT 人工字节门槛。恢复只
处理当前目标命名空间的 stage/backup/journal，不因产物数量提前拒绝，也不扫描或删除
无关文件。

项目日志不是恢复依据。主失败和候选清理失败同时进入同一个 FailureReport，不能互相
覆盖；日志失败不能改变发布终态。
