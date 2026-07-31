# Windows 文件能力与可恢复目录发布

## 1. 安全文件树

为了守住安全边界，ATT 拒绝 reparse point、硬链接、Windows 大小写等价冲突、路径
逃逸和读取期间对象身份变化；除此之外不设人为上限，文件字节、目录项、深度、树
总字节和恢复产物数量都不受限制。

Windows 等价名称按 ordinal ignore-case 的 UTF-16 code unit 判断，Unicode 规范化
和兼容折叠都不参与。每个逻辑路径独占一个物理普通文件：链接数不等于 1 时直接
拒绝，树外别名因此无法绕过指纹，树内也不会有多个路径共享同一份修改。

MV/MZ 来源冻结、Generic 输入一致读取和所有发布候选都建立在这些事实上。ATT 总是
读取完整文件；失败时报告真实的 OS 操作、路径和错误码，而不是看着 metadata 大小
提前拒绝。

独立文件并行处理，完成顺序不影响自然 ordinal 和主错误选择。目录遍历使用堆上
工作栈，再深的合法目录树也不会变成 Rust 栈溢出。

## 2. 租约与发布锁

项目租约位于：

```text
<projects.root>/.att-locks/projects/<engine>/
```

目录发布锁位于：

```text
<projects.root>/.att-locks/directory-publish/<engine>/
```

锁竞争时持续等待并随时响应取消；ATT 不设任意截止时间，也不设本地队列容量。
锁身份采用稳定 Windows 名称摘要。

## 3. 候选

候选请求包含目标、`CreateNew | ReplaceExisting`、来源映射、引擎生成的 overlay
和空目录。目标必须是安全相对路径，来源与候选物理隔离。

ATT 先建立带自然 ordinal 的 manifest，再并行复制未修改文件或写入最终字节；单个
文档内部需要保持顺序的结构修改仍串行。候选在发布前不可见，失败或取消时整树丢弃。

MV/MZ 候选完成 recipe 修改后执行 RPG Maker 全量验证。Generic 候选完成 text 替换
后使用生产 JSONL 解析器重新读取，并再次检查外部输入指纹。Lua 走自己的通道，
不参与目录候选。

## 4. 一次发布与恢复

完整候选只验证一次，覆盖普通对象、Windows 等价名称、reparse、硬链接、物理身份
和引擎领域结构。成功后只执行一次 journal 与目录交换。

`CreateNew` 只发布到不存在的目标；`ReplaceExisting` 使用同卷 journal、backup 和
稳定 file ID 完成可恢复交换。提交前失败，目标保持原样；已经生效但收尾失败、
需要恢复或结果为 `outcome_unknown` 时，ATT 分别报告准确影响和恢复路径。

候选一经交付发布器，publish 或 discard 必然运行到明确终态。取消只拦下新工作；
已经进入目录交换的操作会继续完成，不会停在中间状态。

恢复只处理当前目标命名空间的 candidate、stage、backup 和 journal，无关文件不
扫描也不删除。恢复依据来自 journal 而非项目日志；主失败和候选清理失败都会
保留。
