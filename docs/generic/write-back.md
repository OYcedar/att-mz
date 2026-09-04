# Generic WriteBack 现行规格

```text
att generic write-back --name NAME [--layout-rules FILE]
```

输出固定为：

```text
<att-dir>/projects/generic/<name>/write_back/
```

WriteBack 从当前项目正文生成译后 JSONL，外部输入目录保持原样。它不发出模型请求，也不
读取 Prompt、Profile、Endpoint、Model 或语言检查配置；既有正文按当前适用性与强契约判断
是否可写回。

## 1. 输出内容

显式提供 `--layout-rules FILE` 时，ATT 先校验并保存本次规则，再构建输出；省略时复用项目
保存的规则。规则的保存事务与正文读取分开，输出排版不会回写数据库译文。规则生命周期见
[WriteBack 排版规则](../translation/write-back-layout-rules.md#1-文件结构与生命周期)。

- 保留输入 `.jsonl` 的相对路径；
- 保留 Group 顺序、Unit 顺序、ID 和 kind；
- 当前 Unit 优先用当前语言对的人工译文，其次用对当前源文、完整 Group 语境、
  语言对和强不变量仍适用的自动译文替换 `text`；
- 其他 Unit 保留当前原文；
- 每条 Group 使用紧凑 JSON 占一行；
- 文件使用 LF，非空文件末尾有 LF；
- 输出只包含输入根内的 JSONL 文件，其他文件不复制。

Partial 项目允许写回。结果明确报告使用译文的 Unit 数与保留原文的 Unit 数。
保留但已不适用于当前语言对或 Group 语境的正文不会写回，对应 Unit 保留当前原文；
WriteBack 不删除这些正文，也不把保留原文伪装成已翻译。

WriteBack 重新执行当前 Placeholder 与结构强校验。Placeholder 预期使用源文已经建立的
binding，不要求规则在译文上下文再次命中；完整候选的规则扫描只检查源 binding 之外的新身份。
源语言残留仍只是一项 Review，不会拒绝候选。正文随后依次执行可选的自动译文标点修复、
规则命中的断行和独立续行补空白。开关与默认值见
[配置规格](../runtime/configuration.md#4-writeback-正文开关)，选择器与断行边界见
[WriteBack 排版规则](../translation/write-back-layout-rules.md)。

Generic 排版只在 Unit `text` 内插入 LF，序列化后表现为 JSON 字符串中的 `\n`；不会新增
Group、Unit 或物理 JSONL 记录。关闭标点修复时标点逐字采用数据库译文；开启时只处理自动
译文中已经存在且唯一对应的标点，人工译文跳过。补空白不依赖排版规则。未命中规则或无法
找到安全断点的正文保持排版前文本。语言、术语、措辞和其他布局风险仍由译后 QA 报告。

## 2. 验证与发布

WriteBack 启动时确认外部输入与最近 Extract 一致，先在候选目录生成全部文件，再使用生产
JSONL 解析器重新读取，并确认除 `text` 外的全部事实与当前输入一致。

候选完成后再次确认外部输入指纹。输入在生成期间改变时，候选不发布，并明确要求重新
Extract。

项目及发布目标先满足[目录发布规格](../runtime/directory-publishing.md)的存储条件。
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
[翻译验收指南](../guides/acceptance.md)核对完整写回、源语残留、组合项目和实际消费者。
Generic WriteBack 成功只证明 ATT 输出明确，不证明最终游戏已经采用译文。
独立 Generic 项目可用随包 `translation_qa.py scan --generic-input <当前 JSONL 根>` 核对完整
Translation export 和实际 `write_back`；外部来源映射、反向转换与实际消费者仍需按任务事实另行验证。
