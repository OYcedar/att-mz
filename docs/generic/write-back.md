# Generic WriteBack 现行规格

```text
att generic write-back --name NAME [--layout-rules FILE]
```

输出固定为：

```text
<att-dir>/projects/generic/<name>/write_back/
```

命令永远不修改外部输入目录。

## 1. 输出内容

- 保留输入 `.jsonl` 的相对路径；
- 保留 Group 顺序、Unit 顺序、ID 和 kind；
- 当前 Unit 优先用人工译文，其次用自动译文替换 `text`；
- 其他 Unit 保留当前原文；
- 每条 Group 使用紧凑 JSON 占一行；
- 文件使用 LF，非空文件末尾有 LF；
- 输出只包含输入根内的 JSONL 文件，其他文件不复制。

Partial 项目允许写回。结果明确报告使用译文的 Unit 数与保留原文的 Unit 数。

WriteBack 重新执行当前 Placeholder 与结构强校验；源语言残留仍只是一项 Review，不会拒绝
候选。随后按[配置规格](../runtime/configuration.md#4-writeback-正文开关)执行独立标点修复和
续行补空白，并按[WriteBack 排版规则](../translation/write-back-layout-rules.md)只处理明确
命中的 Unit。

Generic 排版只在 Unit `text` 内插入 LF，序列化后表现为 JSON 字符串中的 `\n`；不会新增
Group、Unit 或物理 JSONL 记录。关闭标点修复时标点逐字采用数据库译文；开启时只处理自动
译文中已经存在且唯一对应的标点，人工译文跳过。补空白不依赖排版规则。未命中规则或无法
找到安全断点的正文保持排版前文本。语言、术语、措辞和其他布局风险仍由译后 QA 报告。

## 2. 验证与发布

WriteBack 启动时确认外部输入与最近 Extract 一致，先在候选目录生成全部文件，再使用生产
JSONL 解析器重新读取，并确认除 `text` 外的全部事实与当前输入一致。

候选完成后再次确认外部输入指纹。输入在生成期间改变时，候选不发布，并明确要求重新
Extract。

所有验证成功后，ATT 一次替换整个 `write_back/`。进入目录交换前失败或取消时，上一次
成功输出保持。发布结果无法确认时，ATT 保留恢复现场并如实报告实际影响，不宣称成功也
不擅自回滚；恢复位置按[目录发布规格](../runtime/directory-publishing.md)说明。

目录发布恢复路径固定为
`<parent>/.directory-publish/<target-name>/{stage,backup,journal}`。保持项目、输入、目标和恢复
路径不变，按[目录发布规格](../runtime/directory-publishing.md)处理诊断中的对象、原因和修改
方法。journal 损坏、目标与已知旧目录都缺失、必要 backup 缺失或结果未知时，不重跑试探，
也不手工删除、改名或移动工作目录。

Generic WriteBack 必须写 `publication.started` 和唯一 `publication.finished`。成功结果为
`published`，汇总 `files`、`translated_units` 和 `retained_source_units`；失败结果为 `not_published`、
`recovery_required` 或 `outcome_unknown`。具体问题由同次可读 `diagnostic.publication` 说明，
不附内部诊断引用。

发布完成后，外部操作者仍需消费全部译后 JSONL，并按
[全量验收指南](../guides/acceptance.md)核对完整写回、源语残留、组合项目和实际消费者。
Generic WriteBack 成功只证明 ATT 输出明确，不证明最终游戏已经采用译文。
