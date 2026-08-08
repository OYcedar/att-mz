# Windows 文件能力与可恢复目录发布

## 1. 安全文件树

ATT 拒绝 reparse point、硬链接、Windows 大小写等价冲突、路径逃逸和读取期间对象身份
变化；除此之外不设置文件字节、目录项、深度、树总字节或恢复产物数量上限。

Windows 等价名称按 ordinal ignore-case 的 UTF-16 code unit 判断，Unicode 规范化和兼容
折叠不参与。每个逻辑路径独占一个物理普通文件，链接数必须为 1。MV/MZ 来源冻结、Generic
输入一致读取和全部发布候选都建立在这些约束上。

ATT 总是读取完整文件。失败时公开输出只说明操作对象、直接原因和修改方法，不输出内部
文件身份、指纹或编码路径。独立文件可以并行处理，完成顺序不改变自然顺序和主错误选择。

## 2. 租约与发布锁

项目租约位于：

```text
<att-dir>/projects/.att-locks/projects/<engine>/<project-name>
```

目录发布锁位于：

```text
<att-dir>/projects/.att-locks/directory-publish/<engine>/<target-name>
```

项目名和目标名直接使用已经校验的自然名称，不使用 hash 或 UUID。锁竞争时等待并响应取消；
ATT 不设置任意截止时间或本地队列总量上限。

## 3. 候选目录

候选请求包含目标、`CreateNew | ReplaceExisting`、来源映射、引擎生成的 overlay 和空目录。
发布目标必须是经过校验的绝对路径；候选内目标必须是安全相对路径。来源、候选和目标物理
隔离。

ATT 先建立自然顺序 manifest，再并行复制未修改文件或写入最终字节。MV/MZ 候选完成 recipe
修改后执行 RPG Maker 全量验证；Generic 候选完成 text 替换后使用生产 JSONL 解析器重新
读取，并再次检查外部输入。Lua 和 Manual 不参与目录候选。

候选在发布前不可见。失败或取消发生在目录交换前时，ATT 清理候选；清理本身失败时保留
准确自然路径并报告，不用随机名称隐藏现场。

## 4. 工作目录名称

每个目标只使用一组固定工作路径：

```text
<parent>/.directory-publish/<target-name>/stage
<parent>/.directory-publish/<target-name>/backup
<parent>/.directory-publish/<target-name>/journal
```

`stage` 保存候选，`backup` 保存替换期间的旧目标，`journal` 保存恢复所需事实。目标锁保证
同一目标不会同时使用这组路径。名称不包含 UUID、hash 或随机后缀。

Generic Init 的数据库临时文件使用：

```text
<project>/.project.db.init.tmp
```

独立文件原子写入统一使用 `.<target-file-name>.tmp`。发生错误且自动清理失败时，诊断显示
这个自然路径。

## 5. 一次发布

完整候选只验证一次，覆盖普通对象、Windows 等价名称、reparse、硬链接、物理身份和引擎
领域结构。成功后只执行一次 journal 与目录交换。

`CreateNew` 只发布到不存在的目标。`ReplaceExisting` 在同卷内使用 journal 和 backup 完成
可恢复交换：

- 交换前失败，目标保持原样；
- 新目标已经可见但收尾失败，ATT 明确说明输出已发布并指出残留路径；
- 已知需要恢复时，ATT 保留 stage、backup 或 journal，并说明下一步；
- 是否生效确实无法确认时，ATT 报告结果未知，不宣称成功或已经回滚。

候选一经交付发布器，publish 或 discard 必须运行到明确终态。取消只阻止新工作；已经进入
目录交换的操作会继续取得明确结果。

## 6. 自动恢复

恢复只处理同一目标的固定工作目录，不扫描或删除无关文件。恢复权威是 journal，不是项目
日志。每次 MV/MZ Init 在读取或复用项目目录前、每次 WriteBack 在建立新候选前，都会在同一
目标锁内先执行恢复。

使用者按诊断中的对象、原因和修改方法处理：

- 只有 backup 或 journal 清理失败，而新输出已经发布：解除占用或权限问题后，重新运行同一
  项目、同一目标的命令；下一次准备先清理残留；
- 目标交换尚未完成，但 journal 和所需 backup 完整：修正文件系统原因后，重新运行同一目标
  命令；ATT 先恢复旧目标或确认新目标，再开始新候选；
- journal 损坏、目标与已知旧目录都缺失、必要 backup 缺失，或一次恢复后仍不能取得明确
  状态：保留全部路径并停止，不手工删除、改名或移动；
- 结果未知：禁止用重跑试探，保持项目、输入、目标和工作目录不变并报告当前能力限制。

当前 CLI 没有独立 `recover` 或 `status` 子命令，也不公开 journal 解码和人工目录交换步骤。
满足可自动恢复条件时，同目标 Init 或 WriteBack 就是恢复入口。

## 7. 权限失败

如果新建 MV/MZ 项目时，目标尚不存在，而移动候选目录因权限或占用失败，项目状态没有改变，
可以在解除占用或修正权限后，用完全相同的项目名、游戏路径、语言和布局参数重跑一次 Init。

第二次仍失败、目标已经出现、存在恢复工作目录，或 ATT 报告结果未知时，停止普通重试并按
上一节处理。不要为了绕过权限问题更换项目名、手工移动候选或删除恢复目录。
