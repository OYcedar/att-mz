# Windows 文件能力与可恢复目录发布

## 1. 安全文件树

ATT 拒绝 reparse point、硬链接、Windows 大小写等价冲突、路径逃逸和读取期间对象身份
变化。它不规定文件字节、目录项、深度、树总字节或恢复产物数量上限。

Windows 等价名称按 ordinal ignore-case 的 UTF-16 code unit 判断，不执行 Unicode
规范化或兼容折叠。每个逻辑路径必须独占一个物理普通文件；链接数不等于 1 时拒绝，
避免树外别名绕过指纹，或树内多个路径共享修改。

MV/MZ 来源冻结、Generic 输入一致读取和所有发布候选都使用这些事实。读取完整文件，
失败时报告真实 OS 操作、路径和错误码，不根据 metadata 大小提前拒绝。

独立文件可以并行处理，完成顺序不改变自然 ordinal 和主错误选择。目录遍历使用堆上工作
栈，不把合法深树变成 Rust 栈溢出。

## 2. 租约与发布锁

项目租约位于：

```text
<projects.root>/.att-locks/projects/<engine>/
```

目录发布锁位于：

```text
<projects.root>/.att-locks/directory-publish/<engine>/
```

锁竞争持续等待并响应取消，不设置任意截止时间或本地队列容量。锁身份采用稳定 Windows
名称摘要。

## 3. 候选

候选请求包含目标、`CreateNew | ReplaceExisting`、来源映射、引擎生成的 overlay 和空目录。
目标必须是安全相对路径，来源与候选物理隔离。

ATT 先建立带自然 ordinal 的 manifest，再并行复制未修改文件或写入最终字节。单个文档
内部需要保持顺序的结构修改仍串行。候选在发布前不可见，失败或取消时整树丢弃。

MV/MZ 候选完成 recipe 修改后执行 RPG Maker 全量验证。Generic 候选完成 text 替换后使用
生产 JSONL 解析器重新读取，并再次检查外部输入指纹。Lua 不参与目录候选。

## 4. 一次发布与恢复

完整候选只验证一次，覆盖普通对象、Windows 等价名称、reparse、硬链接、物理身份和引擎
领域结构。成功后只执行一次 journal 与目录交换。

`CreateNew` 只发布到不存在目标；`ReplaceExisting` 使用同卷 journal、backup 和稳定 file
ID 完成可恢复交换。提交前失败时目标不变；已经生效但收尾失败、需要恢复或
`outcome_unknown` 必须分别报告准确影响和恢复路径。

候选一经交付发布器，publish 或 discard 必须运行到明确终态。取消只阻止新工作，不能把
已经进入目录交换的操作留在中间状态。

恢复只处理当前目标命名空间的 candidate、stage、backup 和 journal，不扫描或删除无关
文件。项目日志不是恢复依据；主失败和候选清理失败都保留。
